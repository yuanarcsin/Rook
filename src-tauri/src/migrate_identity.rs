//! 一次性身份迁移：CampusSentry → Rook（2026-11-15，ADR-037）。
//!
//! 显示名与磁盘名统一为 Rook 后，老版本留下的目录/文件名需要搬到新名字下。
//! 这里只做**可重复执行**的收尾，任何失败都降级为 warn，不阻止启动：
//!
//! - `%APPDATA%\CampusSentry` → `%APPDATA%\Rook`（配置目录）；
//! - `%LOCALAPPDATA%\CampusSentry` → `%LOCALAPPDATA%\Rook`（数据、状态、日志）；
//! - 便携目录 `CampusSentry.toml` → `Rook.toml`（仅当新文件不存在且能改名）；
//! - HKCU Run 旧值 `CampusSentry` 删除（新值由 `autostart` 按配置重写）。
//!
//! 旧 WebView2 数据目录 `%LOCALAPPDATA%\com.campussentry.app` 不搬也不删：
//! 它只是浏览器缓存，新 identifier 会新建 `com.rook.app`；旧目录留给用户手动清。
//!
//! 迁移必须在 `logging::init` **之前**跑：日志与配置都按新目录解析，
//! 否则会先创建空的新目录，导致旧数据无法整体改名。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::brand;

/// 迁移结果：成功搬动的项与需要用户知道的降级项。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// 已完成的搬动（面向日志的短句）。
    pub moved: Vec<String>,
    /// 未完成或冲突（面向日志的短句）。
    pub warnings: Vec<String>,
}

impl Report {
    fn moved(&mut self, message: String) {
        self.moved.push(message);
    }

    fn warn(&mut self, message: String) {
        self.warnings.push(message);
    }

    /// 是否什么都没做。
    pub fn is_empty(&self) -> bool {
        self.moved.is_empty() && self.warnings.is_empty()
    }
}

/// 从环境变量取 `%APPDATA%` / `%LOCALAPPDATA%`，取不到就跳过对应项。
pub fn system_roots() -> (Option<PathBuf>, Option<PathBuf>) {
    let read = |name: &str| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
    };
    (read("APPDATA"), read("LOCALAPPDATA"))
}

/// 启动早期执行迁移；`is_dev` 时不动注册表，避免开发实例污染生产自启项。
pub fn run(is_dev: bool) -> Report {
    let (appdata, local_appdata) = system_roots();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    migrate(
        appdata.as_deref(),
        local_appdata.as_deref(),
        exe_dir.as_deref(),
        is_dev,
    )
}

/// 迁移主体（测试入口）。
pub fn migrate(
    appdata: Option<&Path>,
    local_appdata: Option<&Path>,
    exe_dir: Option<&Path>,
    is_dev: bool,
) -> Report {
    let mut report = Report::default();

    if let Some(base) = appdata {
        move_app_dir(base, &mut report);
    }
    if let Some(base) = local_appdata {
        move_app_dir(base, &mut report);
    }
    if let Some(dir) = exe_dir {
        move_portable_config(dir, &mut report);
    }
    if !is_dev {
        remove_legacy_autostart(&mut report);
    }
    report
}

/// 把一个 `<base>/CampusSentry` 目录改名为 `<base>/Rook`。
fn move_app_dir(base: &Path, report: &mut Report) {
    let old = base.join(brand::LEGACY_NAME);
    let new = base.join(brand::APP_DIR_NAME);
    if !old.is_dir() {
        return;
    }
    if new.exists() {
        report.warn(format!(
            "{} 与 {} 同时存在，跳过改名（请手动合并或删除旧目录）",
            old.display(),
            new.display()
        ));
        return;
    }
    match fs::rename(&old, &new) {
        Ok(()) => report.moved(format!("{} → {}", old.display(), new.display())),
        Err(error) => {
            // 跨卷/占用时退化为复制：先拷到新目录，再保留旧目录由用户清理。
            match copy_dir(&old, &new) {
                Ok(()) => report.moved(format!(
                    "{} 复制到 {}（原目录保留，可手动删除：{error}）",
                    old.display(),
                    new.display()
                )),
                Err(copy_error) => report.warn(format!(
                    "改名 {} 失败：{error}；复制也失败：{copy_error}",
                    old.display()
                )),
            }
        }
    }
}

/// 便携配置改名；失败时 `resolve_config_path` 仍会回退读旧文件。
fn move_portable_config(dir: &Path, report: &mut Report) {
    let old = dir.join(brand::LEGACY_PORTABLE_CONFIG_FILE);
    let new = dir.join(brand::PORTABLE_CONFIG_FILE);
    if !old.is_file() || new.exists() {
        return;
    }
    match fs::rename(&old, &new) {
        Ok(()) => report.moved(format!("{} → {}", old.display(), new.display())),
        Err(error) => report.warn(format!(
            "便携配置改名失败（继续读旧文件）：{}：{error}",
            old.display()
        )),
    }
}

/// 删除旧版的 HKCU Run 值；失败只记录，不影响启动。
fn remove_legacy_autostart(report: &mut Report) {
    #[cfg(windows)]
    {
        use crate::autostart::{Desired, LEGACY_VALUE_NAME, Outcome, WindowsRunKey, reconcile};

        // 只有真的删掉了才记一条；值原本不存在时保持安静，避免每次启动都写
        // 一条 "identity.migrated" 的假事件。
        match reconcile(&WindowsRunKey::legacy(), &Desired::Remove) {
            Ok(Outcome::Removed) => report.moved(format!("清理旧开机自启值 {LEGACY_VALUE_NAME}")),
            Ok(_) => {}
            Err(error) => report.warn(format!("清理旧开机自启值失败：{error}")),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = report;
    }
}

/// 递归复制目录（跨卷改名失败时使用）。
fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().expect("临时目录")
    }

    #[test]
    fn renames_app_dir_only_when_new_missing() {
        let base = temp();
        let old = base.path().join(brand::LEGACY_NAME);
        fs::create_dir_all(old.join("core")).expect("建旧目录");
        fs::write(old.join("state.json"), b"{}").expect("写文件");

        let mut report = Report::default();
        move_app_dir(base.path(), &mut report);
        assert!(
            base.path()
                .join(brand::APP_DIR_NAME)
                .join("state.json")
                .is_file()
        );
        assert!(!old.exists());
        assert_eq!(report.moved.len(), 1);

        // 幂等：再跑一次什么都不做。
        let mut again = Report::default();
        move_app_dir(base.path(), &mut again);
        assert!(again.is_empty());
    }

    #[test]
    fn keeps_both_when_target_exists() {
        let base = temp();
        fs::create_dir_all(base.path().join(brand::LEGACY_NAME)).expect("旧目录");
        fs::create_dir_all(base.path().join(brand::APP_DIR_NAME)).expect("新目录");

        let mut report = Report::default();
        move_app_dir(base.path(), &mut report);
        assert!(report.moved.is_empty());
        assert_eq!(report.warnings.len(), 1);
    }

    #[test]
    fn renames_portable_config() {
        let dir = temp();
        let old = dir.path().join(brand::LEGACY_PORTABLE_CONFIG_FILE);
        fs::write(&old, b"version = 1").expect("写旧配置");

        let mut report = Report::default();
        move_portable_config(dir.path(), &mut report);
        assert!(dir.path().join(brand::PORTABLE_CONFIG_FILE).is_file());
        assert!(!old.exists());
        assert_eq!(report.moved.len(), 1);
    }

    #[test]
    fn does_not_overwrite_existing_portable_config() {
        let dir = temp();
        fs::write(dir.path().join(brand::LEGACY_PORTABLE_CONFIG_FILE), b"old").expect("旧");
        fs::write(dir.path().join(brand::PORTABLE_CONFIG_FILE), b"new").expect("新");

        let mut report = Report::default();
        move_portable_config(dir.path(), &mut report);
        assert!(report.is_empty());
        assert_eq!(
            fs::read_to_string(dir.path().join(brand::PORTABLE_CONFIG_FILE)).unwrap(),
            "new"
        );
    }

    #[test]
    fn copies_when_rename_is_impossible() {
        // 直接调 copy_dir 分支：目标不存在时复制成功。
        let base = temp();
        let from = base.path().join("from");
        fs::create_dir_all(from.join("logs")).expect("建目录");
        fs::write(from.join("logs").join("a.log"), b"x").expect("写文件");
        let to = base.path().join("to");
        copy_dir(&from, &to).expect("复制");
        assert!(to.join("logs").join("a.log").is_file());
    }
}
