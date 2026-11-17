//! state.json 的加载与原子保存。
//!
//! 路径完全由调用方传入（通常是 `<data_dir>/state.json`）；本模块不解析
//! 用户目录、不依赖 Tauri、不读时钟。
//!
//! # 读取策略
//!
//! - 文件缺失：返回默认状态 + [`LoadStatus::Missing`]（首次运行/用户删除
//!   state.json 都是正常情况）；
//! - JSON 损坏、结构类型不对、`desired_override` 非法：返回默认状态 +
//!   [`LoadStatus::Corrupted`]，**绝不改写**原文件，由 Engine 决定提示/隔离；
//! - schema 不是当前版本：返回默认状态 + [`LoadStatus::UnsupportedSchema`]；
//!   未知版本的文件可能包含未来字段/语义，不能按旧结构解释；
//! - 其他 I/O 错误（权限、被占用）：返回默认状态 + [`LoadStatus::Unreadable`]，
//!   监督器不应因为状态缓存读不出来而停止工作。
//!
//! 恢复语义（docs/03 §10）：Engine 必须把 [`PersistedState`] 当作“上次意图与
//! 失败预算的快照”，而不是运行中事实。缺失/损坏/未知版本时是否允许
//! `auto_start` 由 Engine 按报告决定；本模块不做该决策。
//!
//! # 写入策略（原子性、去抖之外的并发约定）
//!
//! - **内容不变不写**：保存前先比较磁盘现有字节与本次序列化结果，相同则返回
//!   [`SaveOutcome::Unchanged`]，不触碰文件；这既省 I/O，也减少多写者互相覆盖。
//! - **原子替换**：同目录临时文件 → `write_all` → `sync_all` → `rename`。
//!   同卷 rename 是原子的，读者只会看到完整旧文件或完整新文件，不会读到半截
//!   JSON。临时文件名带当前 PID 与进程内序号（`.<文件名>.<pid>.<seq>.tmp`），
//!   并发写者（含同进程多线程）不会截断彼此的临时文件；失败路径
//!   （写入/刷盘/rename）都会删除本次临时文件。
//! - **单写者假设**：应用用 single-instance 插件保证只有一个监督器；Engine 必须
//!   把 load→修改→save 串行化（单个 actor），不要多个线程各自保存。
//! - **多写者冲突**：不做跨进程锁、不做字段合并，最后完成 rename 的写者胜出。
//!   state 是可重建的派生缓存，合并旧的失败计数/旧意图可能把用户停止的任务重新
//!   拉起，比“最后写入胜出 + 报告”更危险。若未来确实需要多进程共享状态，应由
//!   更高层加文件锁并定义冲突解决策略，而不是在本模块偷偷合并。
//! - **崩溃残留**：进程在 rename 前崩溃可能留下 `.<文件名>.<pid>.<seq>.tmp`；
//!   加载忽略任意 `*.tmp`，后续保存使用自己的临时文件名，不会复用别人的残留文件。
//! - 保存失败（磁盘满/权限/目标被占用）原样返回带路径与动作的错误；Engine 只记
//!   日志并保持监督，不应让状态落盘失败影响任务运行。

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::model::{PersistedState, SCHEMA_VERSION};

/// 数据目录下的状态文件名。
pub const STATE_FILE_NAME: &str = "state.json";

/// 进程内临时文件序号：即使同一进程里两个线程同时保存，也不会共用临时文件。
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 数据目录下的状态文件路径（路径仍由调用方决定数据目录）。
pub fn path_in(data_dir: &Path) -> PathBuf {
    data_dir.join(STATE_FILE_NAME)
}

/// 一次加载的完整结果：状态 + 报告。
///
/// 无论成功与否都会给出可用的 [`PersistedState`]（失败时为默认状态），
/// Engine 通过 [`LoadStatus`] 决定是否信任它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedState {
    /// 加载到的状态；失败时为 [`PersistedState::default`]。
    pub state: PersistedState,
    /// 加载结果与原因。
    pub status: LoadStatus,
}

impl LoadedState {
    /// 正常读取或文件缺失：可以按“正常恢复”使用。
    pub fn is_usable(&self) -> bool {
        matches!(self.status, LoadStatus::Loaded | LoadStatus::Missing)
    }

    /// 需要记录/提示（损坏、未知版本、不可读）。
    pub fn needs_attention(&self) -> bool {
        !self.is_usable()
    }
}

/// 加载结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadStatus {
    /// 成功按当前 schema 解析。
    Loaded,
    /// 文件不存在（首次运行或用户删除了 state.json）。
    Missing,
    /// 内容损坏/结构不合法/安全关键枚举非法；原文件未被修改。
    Corrupted {
        /// 面向日志的细节（serde/JSON 错误）。
        message: String,
    },
    /// schema 不是 [`SCHEMA_VERSION`]；文件未被修改。
    UnsupportedSchema {
        /// 文件中的 schema 值。
        found: i64,
    },
    /// 其他读取失败（权限、被占用等）。
    Unreadable {
        /// 面向日志的细节。
        message: String,
    },
}

impl fmt::Display for LoadStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Loaded => f.write_str("状态文件已加载"),
            Self::Missing => f.write_str("状态文件不存在，使用默认状态"),
            Self::Corrupted { message } => {
                write!(f, "状态文件损坏，使用默认状态：{message}")
            }
            Self::UnsupportedSchema { found } => write!(
                f,
                "状态文件 schema={found} 不受支持（当前 {SCHEMA_VERSION}），使用默认状态"
            ),
            Self::Unreadable { message } => {
                write!(f, "状态文件读取失败，使用默认状态：{message}")
            }
        }
    }
}

/// 保存结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveOutcome {
    /// 已原子替换写入。
    Written,
    /// 序列化结果与磁盘内容完全相同，未写盘。
    Unchanged,
}

/// 保存失败。
#[derive(Debug, thiserror::Error)]
pub enum StateStoreError {
    /// 序列化失败（当前模型理论上不会发生，保留以便诊断）。
    #[error("state.json 序列化失败：{0}")]
    Serialize(#[from] serde_json::Error),
    /// 文件系统错误；`action` 说明失败阶段（建目录/写临时文件/rename）。
    #[error("state.json I/O 失败（{action}）：{path}：{source}")]
    Io {
        path: PathBuf,
        action: &'static str,
        #[source]
        source: io::Error,
    },
    /// 内存状态的 schema 与当前支持版本不一致：拒绝把未知结构写盘。
    #[error("拒绝写入不支持的 schema 版本 {found}（当前支持 {SCHEMA_VERSION}）")]
    UnsupportedSchema {
        /// 内存状态携带的 schema。
        found: i64,
    },
}

impl StateStoreError {
    fn io(action: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            action,
            source,
        }
    }
}

/// 读取状态文件。
///
/// 从不 panic、从不写文件；缺失/损坏/未知版本都返回默认状态 + 报告。
pub fn load(path: &Path) -> LoadedState {
    match fs::read(path) {
        Ok(bytes) => parse(&bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => LoadedState {
            state: PersistedState::default(),
            status: LoadStatus::Missing,
        },
        Err(error) => LoadedState {
            state: PersistedState::default(),
            status: LoadStatus::Unreadable {
                message: error.to_string(),
            },
        },
    }
}

/// 解析字节内容。
fn parse(bytes: &[u8]) -> LoadedState {
    let value: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(error) => return corrupted(format!("JSON 解析失败：{error}")),
    };

    if !value.is_object() {
        return corrupted("根节点不是 JSON 对象".to_owned());
    }

    // 先看版本再按结构解析：未知版本可能改了字段类型，直接强解只会误报损坏。
    if let Some(schema) = value.get("schema") {
        match schema.as_i64() {
            Some(found) if found != SCHEMA_VERSION => {
                return LoadedState {
                    state: PersistedState::default(),
                    status: LoadStatus::UnsupportedSchema { found },
                };
            }
            Some(_) => {}
            None => return corrupted(format!("schema 字段不是整数：{schema}")),
        }
    }

    match serde_json::from_value::<PersistedState>(value) {
        Ok(mut state) => {
            // schema 缺失时由 serde 默认补当前版本；这里显式兜底，保证对外一致。
            state.schema = SCHEMA_VERSION;
            LoadedState {
                state,
                status: LoadStatus::Loaded,
            }
        }
        Err(error) => corrupted(format!("状态结构无效：{error}")),
    }
}

fn corrupted(message: String) -> LoadedState {
    LoadedState {
        state: PersistedState::default(),
        status: LoadStatus::Corrupted { message },
    }
}

/// 保存状态文件。
///
/// - 序列化为 2 空格缩进的 UTF-8 JSON + 末尾换行，键顺序稳定（`BTreeMap`）；
/// - 内容未变返回 [`SaveOutcome::Unchanged`]，不写盘；
/// - 否则按“临时文件 → sync_all → rename”原子替换，失败清理临时文件；
/// - 父目录不存在时自动创建；
/// - `state.schema` 必须是 [`SCHEMA_VERSION`]，否则返回
///   [`StateStoreError::UnsupportedSchema`]。
pub fn save(path: &Path, state: &PersistedState) -> Result<SaveOutcome, StateStoreError> {
    if state.schema != SCHEMA_VERSION {
        return Err(StateStoreError::UnsupportedSchema {
            found: state.schema,
        });
    }
    let mut bytes = serde_json::to_vec_pretty(state)?;
    bytes.push(b'\n');
    write_atomic(path, &bytes)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<SaveOutcome, StateStoreError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|error| StateStoreError::io("创建父目录", parent, error))?;
    }

    // 内容不变不写；读不到旧文件（首次写/损坏/被占用）时继续尝试原子替换。
    if fs::read(path).is_ok_and(|existing| existing == bytes) {
        return Ok(SaveOutcome::Unchanged);
    }

    let tmp = temp_path(path)?;
    if let Err(error) = write_and_sync(&tmp, bytes) {
        // 不留半截临时文件；remove 失败也只能忽略（原错误更有诊断价值）。
        let _ = fs::remove_file(&tmp);
        return Err(StateStoreError::io("写入临时文件", &tmp, error));
    }
    if let Err(error) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(StateStoreError::io("替换状态文件", path, error));
    }
    Ok(SaveOutcome::Written)
}

/// 写入并刷盘；返回时文件句柄已关闭（Windows 上 rename 前必须关闭）。
fn write_and_sync(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// 同目录、带 PID 的唯一临时文件名。
fn temp_path(path: &Path) -> Result<PathBuf, StateStoreError> {
    let file_name = path.file_name().ok_or_else(|| {
        StateStoreError::io(
            "生成临时文件名",
            path,
            io::Error::new(io::ErrorKind::InvalidInput, "状态文件路径缺少文件名"),
        )
    })?;
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut name = OsString::from(".");
    name.push(file_name);
    name.push(format!(".{}.{seq}.tmp", std::process::id()));
    Ok(path.with_file_name(name))
}

/// 绑定某个路径的状态仓库（便捷封装；读写在内部仍是无状态函数）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    /// 指定状态文件路径。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// 指定数据目录（自动拼接 [`STATE_FILE_NAME`]）。
    pub fn in_dir(data_dir: &Path) -> Self {
        Self::new(path_in(data_dir))
    }

    /// 状态文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读取状态；见 [`load`]。
    pub fn load(&self) -> LoadedState {
        load(&self.path)
    }

    /// 保存状态；见 [`save`]。
    pub fn save(&self, state: &PersistedState) -> Result<SaveOutcome, StateStoreError> {
        save(&self.path, state)
    }
}
