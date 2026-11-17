//! 默认配置模板与"只创建、不覆盖"的落盘逻辑。

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use super::error::LoadError;

/// 默认配置模板：注释齐全、`version = 2`、不含任何启用的任务。
// 显示名改动时同步这一行（raw string 无法引用 brand::DISPLAY_NAME）。
pub const DEFAULT_CONFIG_TEMPLATE: &str = r#"# Rook 配置文件（schema version 1）
#
# 这个文件描述"你希望脚本怎么跑"，是唯一事实源：
# 可以手动编辑、进版本库、备份、diff；界面只是可选的编辑器。
# 修改保存后无需重启，应用会监听文件并重新加载。
#
# 位置优先级：
#   1. --config <path>
#   2. <程序目录>/Rook.toml（便携版）
#   3. %APPDATA%/Rook/config.toml（本文件）
# 相对路径一律相对本文件所在目录解析；%VAR% 按环境变量展开，未知变量视为错误。
# 完整字段说明见 docs/02-task-model-and-config.md。

version = 2

[app]
# 窗口行为（2026-11-04）：双击/手动启动显示主窗口；--autostart 静默驻留托盘。
# language = "zh-CN"            # zh-CN | en-US（预留，当前版本不生效）
# theme = "system"              # system | light | dark（预留，当前版本不生效）
confirm_exit = true             # 有任务在运行时，托盘退出需要确认
# data_dir = ""                 # 空 = %LOCALAPPDATA%/Rook

[supervisor]
# start_stagger_ms = 800        # 多个 auto_start 任务的错峰启动间隔
# shutdown_timeout_ms = 8000    # 退出时停止全部任务的总预算
# config_poll_interval_ms = 1000  # 监听不可用时回退轮询的周期（下限 500ms）
# watch_config = true            # 监听配置文件自动热重载；false = 关闭（需重启生效）
# apply_changes = "restart"     # restart | next_start
# resume_grace_ms = 20000       # 休眠唤醒后的健康宽限

[autostart]
enabled = true
mode = "run_key"                # run_key | scheduled_task | disabled

[notifications]
# enabled = true
# crash_loop = true             # 预留，当前版本不生效（熔断通知固定发送）
# health_degraded = true        # 预留，当前版本不生效（健康降级通知固定发送）
# recovered = false             # 预留，当前版本不生效（恢复通知固定不发送）

[logs]
# core_level = "info"           # error | warn | info | debug | trace
# retention_days = 7             # 归档保留天数（核心日志 + 事件 + 任务日志）
# max_total_mb = 500

# ---------------------------------------------------------------------------
# 任务：每个 [[tasks]] 描述一个被监督的脚本，取消注释后按需修改。
# id / name / command / working_dir 四项必填。
# ---------------------------------------------------------------------------
# [[tasks]]
# id = "ykt-gateway"            # 小写字母/数字/-/_，全局唯一；改名等于删旧建新
# name = "雨课堂网关"
# command = "node"              # 可执行文件名（走 PATH）或绝对路径；参数放 args
# args = ["server/index.js"]
# working_dir = "D:/scripts/ykt"  # 必须存在；相对路径相对本文件
# auto_start = true
# elevated = false              # 预留，当前版本不生效：未实现按任务提权，true 不会改变权限
# notify = true
# ports = [3300]                # 启动前端口预检；冲突只报错，不会杀占用者
# env_proxy = "bypass_localhost"  # inherit | strip | bypass_localhost
# panel_url = "http://127.0.0.1:3300/"
# env = { PORT = "3300" }       # 键不能以 CAMPUS_SENTRY_ 开头（核心保留）
#                             # 核心注入 START_REASON / WINDOW_END / WORK_MODE 等，
#                             # 任务 env 同名项不会生效（保留变量最后写入）
#
#   [tasks.health]              # 省略 = type = "process"，只做进程级监督
#   type = "http"               # http | heartbeat | process
#   interval_ms = 10000
#   timeout_ms = 2000
#   failure_threshold = 3
#   startup_grace_ms = 20000
#   url = "http://127.0.0.1:3300/api/sites"   # v1 只支持 http://
#   expect_status = [200]
#   expect_body_contains = '{"ok":true'       # 防止端口被别的服务占用造成假 200
#
#   [tasks.restart]
#   mode = "on-failure"         # on-failure | always | never
#   initial_delay_ms = 3000
#   max_delay_ms = 60000        # 必须 >= initial_delay_ms
#   multiplier = 2.0            # 1.0 - 10.0
#   jitter = 0.2                # 0 - 0.5
#   max_consecutive_failures = 5  # 0 = 不熔断
#   stable_after_ms = 60000
#   window_ms = 600000            # 启动窗口（0 = 不限）；窗口内超限 → 暂停自动重启
#   max_starts_in_window = 6      # 窗口内最多启动次数（0 = 不限）
#   pause_on_exit_codes = []      # 命中即 paused(needs_human)，不再重启
#
#   [tasks.stop]
#   # graceful：先向进程组发 CTRL_BREAK，脚本可清理后自行退出；
#   #           宽限内未退出则强制结束整棵进程树（默认，推荐）。
#   # terminate：立即强制结束（旧行为；脚本不配合 SIGBREAK 时可用它）。
#   method = "graceful"         # graceful | terminate
#   grace_ms = 2500             # 优雅宽限，100-60000；terminate 时忽略
#   timeout_ms = 8000           # 停止总预算（含宽限 + 兜底确认），100-60000
#
#   [tasks.logs]
#   dir = ""                    # 空 = <数据目录>/logs/<id>
#   rotation = "size"           # size | daily | none
#   max_file_mb = 10
#   max_files = 5
#   max_days = 7                # 归档保留天数（current.log 永不删除）
#   encoding = "auto"           # auto | utf-8 | gb18030
#
#   [tasks.power]
#   resume_grace_ms = 20000     # 覆盖全局；唤醒后健康宽限
#
#   [tasks.limits]              # C6：0 = 不限制；连续 3 次采样超限才停止
#   max_rss_mb = 0              # 工作集上限（MB），0–1048576
#   max_cpu_percent = 0         # 单核百分比上限，0–10000
#
#   [tasks.schedule]            # 每周运行时段（唯一时钟）：enabled 是唯一开关
#   enabled = false             # 开启后：进入窗口自动启动，离开窗口无条件停止
#   # start_in_window / stop_outside 已废弃（2026-11-05）：出现会被忽略并告警
#
#   [[tasks.schedule.windows]]
#   days = ["mon", "tue", "wed", "thu", "fri"]   # 开始日；start > end 表示跨午夜
#   start = "07:50"
#   end = "18:10"
"#;

/// 返回默认配置模板。
pub fn default_config_template() -> &'static str {
    DEFAULT_CONFIG_TEMPLATE
}

/// 写入默认模板。
///
/// - 目标已存在时返回 `Ok(false)`，**绝不覆盖**（含并发创建）；
/// - 返回 `Ok(true)` 表示本次新建；
/// - 父目录不存在则创建；权限/磁盘错误原样返回，并清理写了一半的文件。
pub fn write_default_config(path: &Path) -> Result<bool, LoadError> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|error| LoadError::io(parent, error))?;
    }

    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => return Err(LoadError::io(path, error)),
    };

    if let Err(error) = file
        .write_all(DEFAULT_CONFIG_TEMPLATE.as_bytes())
        .and_then(|()| file.sync_all())
    {
        // 写失败时不要留下半截文件，否则下次启动会把它当成用户配置加载。
        drop(file);
        let _ = fs::remove_file(path);
        return Err(LoadError::io(path, error));
    }

    Ok(true)
}
