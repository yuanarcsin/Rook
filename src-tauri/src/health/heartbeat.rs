//! 心跳探针：JSON 或单行 ISO-8601，按内容变化 + `max_age` 判新鲜。
//!
//! 核心不相信心跳里的 `ts`：新鲜度以"核心观察到内容变化"的时刻为准，
//! `ts` 只做时钟合理性检查。`instance` 不匹配、文件缺失/不可读、内容
//! 无法解析都算本轮失败；启动宽限内"从未见过"返回 `Pending`，由评估器
//! 决定是否算启动失败。

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::state::{BusinessSignal, FailureKind, HealthProbe, HealthWarning, ProbeReport};

/// 心跳文件最大读取字节数；超过即按损坏内容失败并告警。
pub const HEARTBEAT_LIMIT: usize = 64 * 1024;

/// 心跳文件读取接口（默认读真实文件；测试可注入）。
pub trait HeartbeatSource: Send {
    fn read(&mut self, path: &Path) -> io::Result<Vec<u8>>;
}

/// 默认心跳源：有界读取真实文件（最多 `HEARTBEAT_LIMIT` + 1 字节）。
#[derive(Debug, Default, Clone, Copy)]
pub struct FsHeartbeatSource;

impl HeartbeatSource for FsHeartbeatSource {
    fn read(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        let file = std::fs::File::open(path)?;
        let mut bytes = Vec::new();
        // 最多读上限 +1 字节：既能判定超限，又不会把超大/异常文件整个读进内存。
        file.take((HEARTBEAT_LIMIT as u64) + 1)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

/// 心跳探针配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatSpec {
    /// 心跳文件绝对路径（由配置 `health.path` 或 `<data>/heartbeats/<id>.hb` 提供）。
    pub path: PathBuf,
    /// 当前实例 id（`CAMPUS_SENTRY_INSTANCE_ID`）。
    pub instance_id: String,
    /// 内容多久没有变化算过期。
    pub max_age: Duration,
    /// 命中即置位 needs_human 的 status 列表。
    pub needs_human_status: Vec<String>,
}

/// 一次心跳读取的结构化细节（用于快照/诊断）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatReport {
    /// 是否已经成功观察到过心跳。
    pub seen: bool,
    /// 本轮内容是否发生变化。
    pub changed: bool,
    pub status: Option<String>,
    pub seq: Option<i64>,
    pub detail: Option<String>,
    /// 距最近一次内容变化的时间；从未见过为 `None`。
    pub age: Option<Duration>,
}

/// 心跳探针。
pub struct HeartbeatProbe {
    spec: HeartbeatSpec,
    source: Box<dyn HeartbeatSource>,
    last_change: Instant,
    have_seen: bool,
    last_hash: Option<u64>,
    last_seq: Option<i64>,
}

impl HeartbeatProbe {
    /// 创建探针；`now` 是实例启动时刻，作为初始 `last_change`。
    pub fn new(spec: HeartbeatSpec, now: Instant) -> Self {
        Self::with_source(spec, now, Box::new(FsHeartbeatSource))
    }

    /// 注入自定义读取源。
    pub fn with_source(
        spec: HeartbeatSpec,
        now: Instant,
        source: Box<dyn HeartbeatSource>,
    ) -> Self {
        Self {
            spec,
            source,
            last_change: now,
            have_seen: false,
            last_hash: None,
            last_seq: None,
        }
    }

    pub fn spec(&self) -> &HeartbeatSpec {
        &self.spec
    }

    fn observation(&self, now: Instant, changed: bool) -> HeartbeatReport {
        HeartbeatReport {
            seen: self.have_seen,
            changed,
            status: None,
            seq: None,
            detail: None,
            age: if self.have_seen {
                Some(now.saturating_duration_since(self.last_change))
            } else {
                None
            },
        }
    }
}

impl HealthProbe for HeartbeatProbe {
    fn probe(&mut self, now: Instant) -> ProbeReport {
        let started = Instant::now();
        let bytes = match self.source.read(&self.spec.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let latency = started.elapsed();
                let heartbeat = self.observation(now, false);
                if !self.have_seen {
                    return ProbeReport::pending(latency).with_heartbeat(heartbeat);
                }
                return ProbeReport::failed(
                    FailureKind::HeartbeatMissing,
                    format!("心跳文件不存在：{}", self.spec.path.display()),
                    latency,
                )
                .with_heartbeat(heartbeat);
            }
            Err(error) => {
                return ProbeReport::failed(
                    FailureKind::HeartbeatRead,
                    format!("读取心跳文件 {} 失败：{error}", self.spec.path.display()),
                    started.elapsed(),
                )
                .with_heartbeat(self.observation(now, false));
            }
        };

        if bytes.len() > HEARTBEAT_LIMIT {
            return ProbeReport::failed(
                FailureKind::HeartbeatParse,
                format!(
                    "心跳文件 {} 超过 {HEARTBEAT_LIMIT} 字节上限（已读取 {} 字节），按损坏内容处理",
                    self.spec.path.display(),
                    bytes.len()
                ),
                started.elapsed(),
            )
            .with_heartbeat(self.observation(now, false))
            .with_warning(HealthWarning::HeartbeatTooLarge {
                limit: HEARTBEAT_LIMIT,
            });
        }

        let hash = content_hash(&bytes);
        let changed = self.last_hash != Some(hash);

        let parsed = match parse_heartbeat(&bytes) {
            Ok(parsed) => parsed,
            Err(message) => {
                return ProbeReport::failed(
                    FailureKind::HeartbeatParse,
                    format!("心跳内容无法解析：{message}"),
                    started.elapsed(),
                )
                .with_heartbeat(self.observation(now, false));
            }
        };

        if let Some(instance) = &parsed.instance
            && instance != &self.spec.instance_id
        {
            return ProbeReport::failed(
                FailureKind::HeartbeatInstanceMismatch,
                format!(
                    "心跳 instance = {instance:?}，当前实例 = {:?}（旧实例残留）",
                    self.spec.instance_id
                ),
                started.elapsed(),
            )
            .with_heartbeat(self.observation(now, false));
        }

        if changed {
            self.last_change = now;
            self.have_seen = true;
        }
        self.last_hash = Some(hash);

        let mut warnings = Vec::new();
        if changed
            && let (Some(previous), Some(current)) = (self.last_seq, parsed.seq)
            && current <= previous
        {
            warnings.push(HealthWarning::SequenceNotIncreasing { previous, current });
        }
        if let Some(seq) = parsed.seq {
            self.last_seq = Some(seq);
        }

        match &parsed.ts {
            Some(ts) => match DateTime::parse_from_rfc3339(ts) {
                Ok(timestamp) => {
                    let skew = Utc::now().signed_duration_since(timestamp.with_timezone(&Utc));
                    let seconds = skew.num_seconds();
                    if seconds.saturating_abs() > 600 {
                        warnings.push(HealthWarning::ClockSkew { seconds });
                    }
                }
                Err(_) => warnings.push(HealthWarning::TimestampInvalid),
            },
            None => warnings.push(HealthWarning::TimestampMissing),
        }

        let age = if self.have_seen {
            Some(now.saturating_duration_since(self.last_change))
        } else {
            None
        };
        let fresh = age.is_some_and(|age| age <= self.spec.max_age);
        let business = if fresh {
            match &parsed.status {
                Some(status)
                    if self
                        .spec
                        .needs_human_status
                        .iter()
                        .any(|expected| expected == status) =>
                {
                    BusinessSignal::NeedsHuman(status.clone())
                }
                Some(_) => BusinessSignal::Clear,
                None => BusinessSignal::NotReported,
            }
        } else {
            // 过期/从未见过的心跳 status 不可信，保持现有 needs_human 不变。
            BusinessSignal::NotReported
        };

        let heartbeat = HeartbeatReport {
            seen: self.have_seen,
            changed,
            status: parsed.status.clone(),
            seq: parsed.seq,
            detail: parsed.detail.clone(),
            age,
        };

        let latency = started.elapsed();
        let mut report = if fresh {
            ProbeReport::healthy(latency)
        } else {
            ProbeReport::failed(
                FailureKind::HeartbeatStale,
                format!(
                    "心跳内容超过 {:?} 未变化（最近变化 {:?} 前）",
                    self.spec.max_age,
                    age.unwrap_or_default()
                ),
                latency,
            )
        };
        report.heartbeat = Some(heartbeat);
        report.business = business;
        report.warnings = warnings;
        report
    }

    fn reset_observation(&mut self, now: Instant) {
        // 唤醒/时钟跳变后重新起算，避免把休眠时长算成心跳过期。
        self.last_change = now;
    }
}

/// FNV-1a：稳定的内容哈希，不依赖随机化 hasher。
fn content_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[derive(Debug, Deserialize)]
struct JsonHeartbeat {
    #[serde(default)]
    instance: Option<String>,
    #[serde(default)]
    seq: Option<i64>,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    detail: Option<String>,
}

struct ParsedHeartbeat {
    instance: Option<String>,
    seq: Option<i64>,
    ts: Option<String>,
    status: Option<String>,
    detail: Option<String>,
}

fn parse_heartbeat(bytes: &[u8]) -> Result<ParsedHeartbeat, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "内容不是 UTF-8".to_owned())?;
    let text = text.trim();
    if text.is_empty() {
        return Err("内容为空".to_owned());
    }

    if text.starts_with('{') {
        let payload: JsonHeartbeat =
            serde_json::from_str(text).map_err(|error| format!("JSON 无效：{error}"))?;
        return Ok(ParsedHeartbeat {
            instance: payload.instance,
            seq: payload.seq,
            ts: payload.ts,
            status: payload.status,
            detail: payload.detail,
        });
    }

    if text.starts_with('[') {
        return Err("JSON 顶层必须是对象".to_owned());
    }
    if text.contains('\n') || text.contains('\r') {
        return Err("时间戳格式只接受单行".to_owned());
    }
    DateTime::parse_from_rfc3339(text)
        .map_err(|error| format!("不是合法 ISO-8601 时间戳：{error}"))?;

    Ok(ParsedHeartbeat {
        instance: None,
        seq: None,
        ts: Some(text.to_owned()),
        status: None,
        detail: None,
    })
}
