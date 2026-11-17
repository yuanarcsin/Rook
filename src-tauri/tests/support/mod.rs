//! 集成测试公共辅助：定位夹具、解析 Node、有界等待、按 PID 检查进程存活。
//!
//! 这里只依赖公开 crate API 与标准库；不接触 Tauri/Engine/UI。
//! 所有等待都带超时，失败信息包含"等什么"，避免 CI 上无限挂起。

#![allow(dead_code)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use rook_lib::logs::LogParams;
use rook_lib::runtime::{TaskRuntime, TaskStatus};

/// 单次等待上限：夹具都是毫秒级启动，15s 覆盖慢机与杀软扫描。
pub const WAIT: Duration = Duration::from_secs(15);

/// 仓库根（`src-tauri` 的上一级）。
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("src-tauri 应有上级目录")
        .to_path_buf()
}

/// `scripts/fixtures/<name>` 的绝对路径。
pub fn fixture(name: &str) -> PathBuf {
    repo_root().join("scripts").join("fixtures").join(name)
}

/// 夹具参数（String）形式的绝对路径。
pub fn fixture_arg(name: &str) -> String {
    fixture(name).display().to_string()
}

/// 在 PATH + PATHEXT 中查找可执行文件；找不到返回 `None`。
pub fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<OsString> = std::env::var_os("PATHEXT")
        .map(|value| {
            std::env::split_paths(&value)
                .map(|ext| ext.into_os_string())
                .collect()
        })
        .unwrap_or_else(|| vec![OsString::from(".EXE")]);

    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let candidate = dir.join(format!("{name}{}", ext.to_string_lossy()));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Node 可执行文件；缺失时打印跳过原因并返回 `None`（用例直接 `return`）。
pub fn node() -> Option<PathBuf> {
    match find_program("node") {
        Some(path) => Some(path),
        None => {
            eprintln!("跳过：PATH 中找不到 node（Node.js），该用例需要 Node 夹具");
            None
        }
    }
}

/// 有界轮询，直到条件成立；超时则 panic（包含等待内容）。
pub fn wait_for(what: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    loop {
        if predicate() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("等待超时（{}ms）：{what}", WAIT.as_millis());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// 等待运行时进入指定状态。
pub fn wait_status(runtime: &TaskRuntime, expected: TaskStatus) {
    wait_for(&format!("状态变为 {}", expected.as_str()), || {
        runtime.status() == expected
    });
}

/// 等待日志文件出现满足条件的字节（运行中读取；写入端允许共享读）。
pub fn wait_log(path: &Path, what: &str, mut predicate: impl FnMut(&[u8]) -> bool) {
    wait_for(&format!("日志满足 {what}"), || {
        fs::read(path).is_ok_and(|bytes| predicate(&bytes))
    });
}

/// 读取日志文件；失败时给出路径。
pub fn read_log(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|error| panic!("读取日志 {} 失败：{error}", path.display()))
}

/// 字节子串查找。
pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// 测试用日志参数：快速轮询 + 较短的停止收尾超时。
pub fn logs_params(dir: &Path) -> LogParams {
    let mut params = LogParams::new(dir);
    params.poll_interval = Duration::from_millis(5);
    params.stop_timeout = Duration::from_secs(5);
    params
}

/// 读 PID 文件（十进制 + 换行）。
pub fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// 有界等待 PID 文件出现并返回其中的 PID。
pub fn wait_pid_file(path: &Path) -> u32 {
    let mut found = None;
    wait_for(&format!("PID 文件 {}", path.display()), || {
        found = read_pid(path);
        found.is_some()
    });
    found.expect("wait_for 返回时 PID 应已存在")
}

/// 按 PID 判断进程是否仍在运行。
///
/// 只查询（`PROCESS_QUERY_LIMITED_INFORMATION`），不终止、不按进程名匹配；
/// 退出码不是 `STILL_ACTIVE` 时视为已结束（进程对象可能因残留句柄仍可打开）。
pub fn process_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    if pid == 0 {
        return false;
    }
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut code = 0u32;
        let alive = GetExitCodeProcess(handle, &mut code).is_ok() && code == STILL_ACTIVE.0 as u32;
        let _ = CloseHandle(handle);
        alive
    }
}

/// 有界等待 PID 消失（用于孤儿检测）；超时则 panic 并报告 PID。
pub fn wait_gone(pid: u32, timeout: Duration, what: &str) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_alive(pid) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !process_alive(pid),
        "{what}：PID {pid} 在 {}ms 后仍然存活",
        timeout.as_millis()
    );
}

/// 取一个当前空闲的本机端口（先绑定再释放）。
pub fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("绑定临时端口");
    listener.local_addr().expect("读取临时端口").port()
}
