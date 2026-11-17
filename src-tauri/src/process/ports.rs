//! M2-3b：TCP 端口预检（只读，绝不杀进程）。
//!
//! 用 `GetExtendedTcpTable(TCP_TABLE_OWNER_PID_ALL)` 读取 IPv4/IPv6 的 TCP 表，
//! 只统计 `MIB_TCP_STATE_LISTEN` 行，回答“端口是否已被监听、被谁监听”。
//!
//! # 安全边界
//!
//! - **只读**：不 `bind`、不 `connect`、不 `TerminateProcess`、不修改任何系统状态；
//! - 查询失败一律返回 [`PortCheckError`]（带阶段 + Win32 码），
//!   绝不把“查不到”当成“空闲”；
//! - 进程名只是附加信息：`OpenProcess`/`QueryFullProcessImageNameW` 失败
//!   （权限不足、进程已退出、PID 0）时 `process_name = None`，占用判定不受影响。
//!
//! # 字节序
//!
//! - `dwLocalPort` 的低 16 位是**网络序**，需要 `u16::from_be`；
//! - `dwLocalAddr` 的 4 个字节按内存顺序就是网络序地址（小端下 `to_ne_bytes` 正确）；
//! - IPv6 的 `ucLocalAddr` 本身就是 16 字节网络序。
//!
//! # 未覆盖
//!
//! 不做 UDP、不探测“能否绑定”、不自动结束占用进程。

use std::collections::HashMap;
use std::fmt;
use std::mem::size_of;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_DATA, ERROR_SUCCESS};
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCP_STATE_LISTEN, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID,
    TCP_TABLE_OWNER_PID_ALL,
};
use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::PWSTR;

use super::error::win32_code;
use super::win32::OwnedHandle;

/// TCP 表在探测与读取之间可能增长，允许的重新分配次数。
const MAX_TABLE_ATTEMPTS: usize = 4;

/// `QueryFullProcessImageNameW` 的缓冲区上限（长路径的合理上界）。
const MAX_IMAGE_PATH: u32 = 32_768;

/// IP 地址族；同时决定查询哪张 TCP 表。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpFamily {
    /// IPv4（`AF_INET`）。
    V4,
    /// IPv6（`AF_INET6`）。
    V6,
}

impl IpFamily {
    /// 面向日志/界面的名称。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V4 => "IPv4",
            Self::V6 => "IPv6",
        }
    }

    fn query_stage(self) -> PortCheckStage {
        match self {
            Self::V4 => PortCheckStage::QueryIpv4Table,
            Self::V6 => PortCheckStage::QueryIpv6Table,
        }
    }
}

impl fmt::Display for IpFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一个 TCP 监听项（表里的一行 `LISTEN`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortListener {
    /// 地址族。
    pub family: IpFamily,
    /// 本地监听地址（`0.0.0.0` / `::` 表示通配）。
    pub local_addr: IpAddr,
    /// IPv6 的 scope id；IPv4 恒为 0。
    pub scope_id: u32,
    /// 本地端口（已从网络序转换）。
    pub port: u16,
    /// 占用该监听项的进程 PID。
    pub pid: u32,
    /// 进程文件名；权限不足/进程已退出/PID 0 时为 `None`。
    pub process_name: Option<String>,
}

impl fmt::Display for PortListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.local_addr {
            IpAddr::V4(addr) => write!(f, "{}:{} (pid {})", addr, self.port, self.pid),
            IpAddr::V6(addr) if self.scope_id != 0 => write!(
                f,
                "[{}%{}]:{} (pid {})",
                addr, self.scope_id, self.port, self.pid
            ),
            IpAddr::V6(addr) => write!(f, "[{}]:{} (pid {})", addr, self.port, self.pid),
        }
    }
}

/// 单个端口的检查结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortStatus {
    /// 被查询的端口。
    pub port: u16,
    /// 该端口的全部 `LISTEN` 项（空 = 空闲）。
    pub listeners: Vec<PortListener>,
}

impl PortStatus {
    /// 是否有进程在监听（任意地址族/地址）。
    pub fn is_occupied(&self) -> bool {
        !self.listeners.is_empty()
    }

    /// 去重后的占用 PID，保持表内出现顺序。
    pub fn pids(&self) -> Vec<u32> {
        let mut pids = Vec::new();
        for listener in &self.listeners {
            if !pids.contains(&listener.pid) {
                pids.push(listener.pid);
            }
        }
        pids
    }
}

/// 一次多端口检查的完整结果，条目顺序与首次出现的输入端口一致。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortCheckReport {
    entries: Vec<PortStatus>,
}

impl PortCheckReport {
    /// 按输入首次出现顺序排列的端口结果。
    pub fn entries(&self) -> &[PortStatus] {
        &self.entries
    }

    /// 取某个端口的结果；输入里没有该端口时为 `None`。
    pub fn get(&self, port: u16) -> Option<&PortStatus> {
        self.entries.iter().find(|entry| entry.port == port)
    }

    /// 仅被占用的端口，按输入顺序。
    pub fn occupied(&self) -> impl Iterator<Item = &PortStatus> {
        self.entries.iter().filter(|entry| entry.is_occupied())
    }
}

/// 端口检查的失败阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortCheckStage {
    /// `GetExtendedTcpTable(AF_INET, ...)`。
    QueryIpv4Table,
    /// `GetExtendedTcpTable(AF_INET6, ...)`。
    QueryIpv6Table,
    /// 返回的缓冲区结构不完整（表头/行数与长度不符）。
    ReadTable,
}

impl PortCheckStage {
    /// 面向日志/界面的中文阶段名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::QueryIpv4Table => "查询 IPv4 TCP 表",
            Self::QueryIpv6Table => "查询 IPv6 TCP 表",
            Self::ReadTable => "解析 TCP 表",
        }
    }
}

impl fmt::Display for PortCheckStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 端口检查错误：阶段 + 原始 Win32 错误码 + 说明。
///
/// 出现本错误时调用方必须把结果视为“未知”，**不能**当成端口空闲。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortCheckError {
    pub stage: PortCheckStage,
    pub code: u32,
    pub message: String,
}

impl PortCheckError {
    pub(crate) fn new(stage: PortCheckStage, code: u32, message: impl Into<String>) -> Self {
        Self {
            stage,
            code,
            message: message.into(),
        }
    }

    /// 失败阶段。
    pub fn stage(&self) -> PortCheckStage {
        self.stage
    }

    /// 原始 Win32 错误码；解析类错误为 `ERROR_INVALID_DATA`。
    pub fn code(&self) -> u32 {
        self.code
    }

    /// 错误说明。
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for PortCheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}失败（Win32 错误 {}）：{}",
            self.stage, self.code, self.message
        )
    }
}

impl std::error::Error for PortCheckError {}

/// 检查一组 TCP 端口是否已被监听。
///
/// - 返回的条目按输入**首次出现**顺序排列，重复端口只查一次；
/// - 空输入直接返回空报告，不触达系统；
/// - IPv4 或 IPv6 表查询失败都返回错误，绝不降级为“空闲”。
pub fn check_tcp_ports(ports: &[u16]) -> Result<PortCheckReport, PortCheckError> {
    check_with(ports, &mut query_listeners)
}

/// 表内一行（解析后、尚未补进程名）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RawListener {
    pub(super) family: IpFamily,
    pub(super) local_addr: IpAddr,
    pub(super) scope_id: u32,
    pub(super) port: u16,
    pub(super) pid: u32,
}

fn check_with(
    ports: &[u16],
    query: &mut dyn FnMut(IpFamily) -> Result<Vec<RawListener>, PortCheckError>,
) -> Result<PortCheckReport, PortCheckError> {
    let mut unique: Vec<u16> = Vec::with_capacity(ports.len());
    for &port in ports {
        if !unique.contains(&port) {
            unique.push(port);
        }
    }
    if unique.is_empty() {
        return Ok(PortCheckReport::default());
    }

    // 两张表都先查完再匹配：任何一张失败都整体失败，不会只报告一半。
    let mut listeners = query(IpFamily::V4)?;
    listeners.extend(query(IpFamily::V6)?);

    let mut names: HashMap<u32, Option<String>> = HashMap::new();
    let mut entries = Vec::with_capacity(unique.len());
    for port in unique {
        let matched: Vec<PortListener> = listeners
            .iter()
            .filter(|listener| listener.port == port)
            .map(|listener| PortListener {
                family: listener.family,
                local_addr: listener.local_addr,
                scope_id: listener.scope_id,
                port: listener.port,
                pid: listener.pid,
                process_name: names
                    .entry(listener.pid)
                    .or_insert_with(|| process_name(listener.pid))
                    .clone(),
            })
            .collect();
        entries.push(PortStatus {
            port,
            listeners: matched,
        });
    }
    Ok(PortCheckReport { entries })
}

/// 测试钩子：用注入的查询函数替换真实系统调用（覆盖失败/降级路径）。
#[cfg(test)]
pub(super) fn check_tcp_ports_with(
    ports: &[u16],
    mut query: impl FnMut(IpFamily) -> Result<Vec<RawListener>, PortCheckError>,
) -> Result<PortCheckReport, PortCheckError> {
    check_with(ports, &mut query)
}

/// 读取指定地址族的全部 TCP 表项，只保留 `LISTEN`。
fn query_listeners(family: IpFamily) -> Result<Vec<RawListener>, PortCheckError> {
    let stage = family.query_stage();
    let address_family = match family {
        IpFamily::V4 => AF_INET.0 as u32,
        IpFamily::V6 => AF_INET6.0 as u32,
    };

    // 第一次调用只取所需字节数（预期返回 ERROR_INSUFFICIENT_BUFFER）。
    let mut size = 0u32;
    let code = unsafe {
        GetExtendedTcpTable(
            None,
            &mut size,
            false,
            address_family,
            TCP_TABLE_OWNER_PID_ALL,
            0,
        )
    };
    if code != ERROR_SUCCESS.0 && code != ERROR_INSUFFICIENT_BUFFER.0 {
        return Err(PortCheckError::new(
            stage,
            code,
            format!("GetExtendedTcpTable 探测缓冲区失败（AF={address_family}）"),
        ));
    }
    if size == 0 {
        return Ok(Vec::new());
    }

    for _ in 0..MAX_TABLE_ATTEMPTS {
        // 用 u32 缓冲保证 4 字节对齐；表项仍按 `read_unaligned` 读取。
        let mut buffer = vec![0u32; (size as usize).div_ceil(size_of::<u32>())];
        let mut written = size;
        let code = unsafe {
            GetExtendedTcpTable(
                Some(buffer.as_mut_ptr().cast()),
                &mut written,
                false,
                address_family,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            )
        };

        if code == ERROR_SUCCESS.0 {
            if written as usize > buffer.len() * size_of::<u32>() {
                return Err(malformed(
                    stage,
                    "GetExtendedTcpTable 返回的字节数超过缓冲区",
                ));
            }
            let bytes = unsafe {
                std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), written as usize)
            };
            return parse_table(family, bytes, stage);
        }
        if code == ERROR_INSUFFICIENT_BUFFER.0 {
            if written <= size {
                return Err(malformed(stage, "TCP 表在重试期间未给出更大的缓冲区"));
            }
            size = written;
            continue;
        }
        return Err(PortCheckError::new(
            stage,
            code,
            format!("GetExtendedTcpTable 读取失败（AF={address_family}）"),
        ));
    }

    Err(PortCheckError::new(
        stage,
        ERROR_INSUFFICIENT_BUFFER.0,
        "TCP 表在重试期间持续增长",
    ))
}

/// 解析 `MIB_TCPTABLE_OWNER_PID` / `MIB_TCP6TABLE_OWNER_PID`。
fn parse_table(
    family: IpFamily,
    bytes: &[u8],
    stage: PortCheckStage,
) -> Result<Vec<RawListener>, PortCheckError> {
    if bytes.len() < size_of::<u32>() {
        return Err(malformed(stage, "TCP 表短于表头"));
    }
    let count = u32::from_ne_bytes(bytes[0..4].try_into().expect("长度已检查")) as usize;
    let row_size = match family {
        IpFamily::V4 => size_of::<MIB_TCPROW_OWNER_PID>(),
        IpFamily::V6 => size_of::<MIB_TCP6ROW_OWNER_PID>(),
    };
    let needed = count
        .checked_mul(row_size)
        .and_then(|body| body.checked_add(size_of::<u32>()))
        .ok_or_else(|| malformed(stage, "TCP 表行数溢出"))?;
    if bytes.len() < needed {
        return Err(malformed(
            stage,
            format!(
                "TCP 表长度不足：声明 {count} 行（需要 {needed} 字节），实际 {} 字节",
                bytes.len()
            ),
        ));
    }

    let mut listeners = Vec::with_capacity(count);
    for index in 0..count {
        let row = unsafe { bytes.as_ptr().add(size_of::<u32>() + index * row_size) };
        match family {
            IpFamily::V4 => {
                // SAFETY: 已确认缓冲区覆盖该行，`read_unaligned` 不要求指针对齐。
                let row: MIB_TCPROW_OWNER_PID = unsafe { std::ptr::read_unaligned(row.cast()) };
                if row.dwState != MIB_TCP_STATE_LISTEN.0 as u32 {
                    continue;
                }
                listeners.push(RawListener {
                    family,
                    local_addr: IpAddr::V4(Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes())),
                    scope_id: 0,
                    port: network_port(row.dwLocalPort),
                    pid: row.dwOwningPid,
                });
            }
            IpFamily::V6 => {
                // SAFETY: 同上。
                let row: MIB_TCP6ROW_OWNER_PID = unsafe { std::ptr::read_unaligned(row.cast()) };
                if row.dwState != MIB_TCP_STATE_LISTEN.0 as u32 {
                    continue;
                }
                listeners.push(RawListener {
                    family,
                    local_addr: IpAddr::V6(Ipv6Addr::from(row.ucLocalAddr)),
                    scope_id: row.dwLocalScopeId,
                    port: network_port(row.dwLocalPort),
                    pid: row.dwOwningPid,
                });
            }
        }
    }
    Ok(listeners)
}

/// `dwLocalPort` 低 16 位为网络序。
fn network_port(raw: u32) -> u16 {
    u16::from_be((raw & 0xFFFF) as u16)
}

fn malformed(stage: PortCheckStage, message: impl Into<String>) -> PortCheckError {
    PortCheckError::new(stage, ERROR_INVALID_DATA.0, message)
}

/// 进程文件名；任何一步失败都返回 `None`，不影响占用判定。
fn process_name(pid: u32) -> Option<String> {
    if pid == 0 {
        // PID 0 是 Idle/System 伪进程，无法 OpenProcess。
        return None;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let handle = OwnedHandle::new(handle);

    let mut size = 260u32;
    loop {
        let mut buffer = vec![0u16; size as usize];
        let mut written = size;
        let result = unsafe {
            QueryFullProcessImageNameW(
                handle.raw(),
                PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut written,
            )
        };
        match result {
            Ok(()) => {
                let path = String::from_utf16_lossy(&buffer[..written as usize]);
                return Some(file_name(&path));
            }
            Err(error) => {
                if win32_code(&error) == ERROR_INSUFFICIENT_BUFFER.0 && size < MAX_IMAGE_PATH {
                    size = (size * 2).min(MAX_IMAGE_PATH);
                    continue;
                }
                return None;
            }
        }
    }
}

/// 从完整映像路径取文件名；取不到时退回完整路径。
fn file_name(path: &str) -> String {
    Path::new(path).file_name().map_or_else(
        || path.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}
