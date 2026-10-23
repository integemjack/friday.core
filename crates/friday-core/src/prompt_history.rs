//! v2.11 提示词的版本历史（数据目录里的 Prompts 是一个 git 仓库，每次变化自动提交一次；设置 › 智能体 里看历史、回滚、恢复成自带的）
//! 里两端都要的规则：每次调 git 带的设置、git log 解成版本列表、各种提交说明、自动回滚的判断。
//! 跑 git（后台一次一个）、每秒看文件变没变、停一会儿再提交、界面上的状态还在两端的 PromptStore 里——核心还不能主动通知两端
//! （段变了、git 跑完了），等第 4 阶段有了事件回调再整个搬。
//!
//! 「上次提交时实际用的段」记在核心里（[`mark_committed`]），算改了哪几段（[`describe_changes`]）时和现在的比，两端不用把整套段传来传去。
//!
//! 原来是 Swift `Prompts/PromptStore.swift` 和 Qt `models/PromptStore.cpp` 各写一份，文案逐字照搬。

use std::sync::{LazyLock, RwLock};

use chrono::{DateTime, Local};

use crate::prompts::{self, MergeReport, Sections};

/// 每次都带的 git 设置：不依赖用户的全局配置（没设过名字也能提交；全局要求签名也不弹框）
pub const GIT_BASE: &[&str] = &[
    "-c",
    "user.name=Friday",
    "-c",
    "user.email=friday@localhost",
    "-c",
    "commit.gpgsign=false",
    "-c",
    "core.autocrlf=false",
    "-c",
    "core.quotepath=false",
    "-c",
    "i18n.logOutputEncoding=UTF-8",
];

/// 读历史用的 git log 参数（解析见 [`parse_log`]）
pub const LOG_ARGUMENTS: &[&str] = &["log", "-n", "200", "--format=%H%x1f%aI%x1f%s"];

/// 自动回滚：两边都至少这么多件事才比
pub const MIN_TASKS: i64 = 8;
/// 一次交付率低了、或追问率高了这么多以上算明显差
pub const WORSE_BY: f64 = 0.15;

/// 一个版本（新的在前）
#[derive(Debug, Clone, PartialEq)]
pub struct Version {
    pub id: String,
    pub short_id: String,
    /// 本地时间文字（yyyy-MM-dd HH:mm）
    pub time: String,
    pub message: String,
}

/// 「a、b、c」，超过 5 个只列前 5 个加「等」
pub fn join_keys(keys: &[String]) -> String {
    if keys.len() <= 5 { keys.join("、") } else { keys[..5].join("、") + " 等" }
}

/// 启动时那一次提交的说明：第一次写入、合并了新版本自带的，写清楚；其余（Friday 没开时改过文件）
pub fn startup_message(version: &str, merge: &MergeReport) -> String {
    if merge.installed {
        return format!("Friday {version} 自带的提示词");
    }
    let mut parts = Vec::new();
    if !merge.added.is_empty() {
        parts.push(format!("新增 {} 段（{}）", merge.added.len(), join_keys(&merge.added)));
    }
    if !merge.updated.is_empty() {
        parts.push(format!("更新 {} 段（{}）", merge.updated.len(), join_keys(&merge.updated)));
    }
    if !merge.kept_local.is_empty() {
        parts.push(format!("{} 段两边都改了、留了本地的（{}）", merge.kept_local.len(), join_keys(&merge.kept_local)));
    }
    if parts.is_empty() {
        "Friday 没开时改过的提示词".to_string()
    } else {
        format!("合并 Friday {version} 自带的提示词：{}", parts.join("；"))
    }
}

static COMMITTED: LazyLock<RwLock<Sections>> = LazyLock::new(|| RwLock::new(Sections::new()));

/// 记下现在实际用的段（每次提交完、没装 git 时也记）
pub fn mark_committed() {
    *COMMITTED.write().unwrap() = prompts::sections(None);
}

/// 上次提交以来改了哪几段（提交说明）
pub fn describe_changes() -> String {
    let now = prompts::sections(None);
    let committed = COMMITTED.read().unwrap();
    let mut keys: Vec<String> = now.keys().chain(committed.keys()).filter(|key| now.get(*key) != committed.get(*key)).cloned().collect();
    keys.sort();
    keys.dedup();
    if keys.is_empty() {
        "改了提示词文件（注释或格式）".to_string()
    } else {
        format!("改了 {} 段：{}", keys.len(), join_keys(&keys))
    }
}

/// git log（LOG_ARGUMENTS 的格式：完整 id、作者时间、说明，用 0x1f 分开）解成版本列表；时间换成本地的 yyyy-MM-dd HH:mm，解不出照原样
pub fn parse_log(output: &str) -> Vec<Version> {
    output
        .split('\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\u{1f}').collect();
            if fields.len() < 3 {
                return None;
            }
            let time = DateTime::parse_from_rfc3339(fields[1])
                .map(|date| date.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|_| fields[1].to_string());
            Some(Version {
                id: fields[0].to_string(),
                short_id: fields[0].chars().take(7).collect(),
                time,
                message: fields[2..].join("\u{1f}"),
            })
        })
        .collect()
}

/// 给人看的版本：「2026-10-23 14:07 的版本（说明）」；历史里找不到就是短编号。按完整 id 或短编号找
pub fn version_label(history: &[Version], id: &str) -> String {
    history
        .iter()
        .find(|version| version.id == id || version.short_id == id)
        .map_or_else(|| id.chars().take(7).collect(), |version| format!("{} 的版本（{}）", version.time, version.message))
}

/// 一个版本的指标（== 两端的 MemoryMetrics）
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Metrics {
    pub tasks: i64,
    pub first_pass: i64,
    pub deliveries: i64,
    pub followups: i64,
}

impl Metrics {
    /// 没有数据为 -1
    pub fn first_pass_rate(&self) -> f64 {
        if self.tasks > 0 { self.first_pass as f64 / self.tasks as f64 } else { -1.0 }
    }

    pub fn followup_rate(&self) -> f64 {
        if self.deliveries > 0 { self.followups as f64 / self.deliveries as f64 } else { -1.0 }
    }
}

/// 某个版本比上一版明显差了：bad 是现在的版本、good 是要回到的（短编号和完整 id）、reason 给人看；
/// note 记进记忆库给设置页看，message 是回滚那次的提交说明
#[derive(Debug, Clone, PartialEq)]
pub struct Regression {
    pub bad: String,
    pub good: String,
    pub good_id: String,
    pub reason: String,
    pub note: String,
    pub message: String,
}

/// 现在的版本（history 第一个）是不是比上一个有足够数据的版本明显差（两边都至少 MIN_TASKS 件事；一次交付率低了
/// WORSE_BY 以上，或追问率高了 WORSE_BY 以上）；rejected 里的（自动回滚过的）不当「上一版」
pub fn regression(history: &[Version], by_version: &[(String, Metrics)], rejected: &[String]) -> Option<Regression> {
    if history.len() < 2 {
        return None;
    }
    let metrics = |version: &str| by_version.iter().find(|(name, _)| name == version).map(|(_, metrics)| *metrics);
    let current = &history[0].short_id;
    let mine = metrics(current).filter(|mine| mine.tasks >= MIN_TASKS)?;
    for entry in &history[1..] {
        if rejected.contains(&entry.short_id) {
            continue;
        }
        let Some(before) = metrics(&entry.short_id).filter(|before| before.tasks >= MIN_TASKS) else { continue };
        let first_pass_drop = before.first_pass_rate() - mine.first_pass_rate();
        let followup_rise = if mine.deliveries > 0 && before.deliveries > 0 { mine.followup_rate() - before.followup_rate() } else { 0.0 };
        if first_pass_drop < WORSE_BY && followup_rise < WORSE_BY {
            return None;
        }
        let percent = |rate: f64| format!("{}%", (rate * 100.0).round() as i64);
        let reason = format!(
            "一次交付率 {} → {}、追问率 {} → {}（{} 件事 vs {} 件事）",
            percent(before.first_pass_rate()),
            percent(mine.first_pass_rate()),
            percent(before.followup_rate()),
            percent(mine.followup_rate()),
            before.tasks,
            mine.tasks
        );
        let label = version_label(history, &entry.short_id);
        return Some(Regression {
            bad: current.clone(),
            good: entry.short_id.clone(),
            good_id: entry.id.clone(),
            note: format!("提示词 {current} 比上一版差：{reason}。已自动回滚到 {label}"),
            message: format!("自动回滚到 {label}：{current} {reason}"),
            reason,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> Vec<Version> {
        ["ccccccc", "bbbbbbb", "aaaaaaa"]
            .map(|id| Version { id: format!("{id}000000000"), short_id: id.to_string(), time: String::new(), message: String::new() })
            .to_vec()
    }

    fn metrics(tasks: i64, first_pass: i64, deliveries: i64, followups: i64) -> Metrics {
        Metrics { tasks, first_pass, deliveries, followups }
    }

    fn by(pairs: &[(&str, Metrics)]) -> Vec<(String, Metrics)> {
        pairs.iter().map(|(name, metrics)| (name.to_string(), *metrics)).collect()
    }

    /// == Swift 自检 promptRegressionRule 和 Qt TestUtil 的同一组
    #[test]
    fn regressions() {
        let history = history();
        assert!(
            regression(&history, &by(&[("ccccccc", metrics(5, 0, 5, 9)), ("bbbbbbb", metrics(10, 9, 10, 1))]), &[]).is_none(),
            "现在的不够 8 件不判"
        );
        assert!(
            regression(&history, &by(&[("ccccccc", metrics(10, 8, 12, 3)), ("bbbbbbb", metrics(10, 9, 12, 2))]), &[]).is_none(),
            "差不多不回滚"
        );
        let found = regression(&history, &by(&[("ccccccc", metrics(10, 5, 14, 5)), ("bbbbbbb", metrics(10, 9, 11, 1))]), &[]).unwrap();
        assert_eq!((found.bad.as_str(), found.good.as_str(), found.good_id.as_str()), ("ccccccc", "bbbbbbb", "bbbbbbb000000000"));
        assert!(found.reason.contains("90% → 50%"), "{}", found.reason);
        assert_eq!(found.note, format!("提示词 ccccccc 比上一版差：{}。已自动回滚到  的版本（）", found.reason));
        let three = by(&[("ccccccc", metrics(10, 5, 10, 6)), ("bbbbbbb", metrics(3, 3, 3, 0)), ("aaaaaaa", metrics(9, 8, 9, 1))]);
        assert_eq!(regression(&history, &three, &[]).unwrap().good, "aaaaaaa", "上一版数据不够再往前");
        let three = by(&[("ccccccc", metrics(10, 5, 10, 6)), ("bbbbbbb", metrics(10, 9, 10, 1)), ("aaaaaaa", metrics(9, 8, 9, 1))]);
        assert_eq!(regression(&history, &three, &["bbbbbbb".to_string()]).unwrap().good, "aaaaaaa", "回滚掉过的跳过");
        assert!(
            regression(&history, &by(&[("ccccccc", metrics(10, 8, 10, 6)), ("bbbbbbb", metrics(10, 8, 10, 2))]), &[]).is_some(),
            "只有追问率高了也算"
        );
        assert!(regression(&history[..1], &[], &[]).is_none());
    }

    #[test]
    fn messages_and_log() {
        let keys = |names: &[&str]| names.iter().map(|name| name.to_string()).collect::<Vec<_>>();
        assert_eq!(join_keys(&keys(&["a", "b"])), "a、b");
        assert_eq!(join_keys(&keys(&["1", "2", "3", "4", "5", "6"])), "1、2、3、4、5 等");
        assert_eq!(startup_message("2.79.0", &MergeReport { installed: true, ..Default::default() }), "Friday 2.79.0 自带的提示词");
        assert_eq!(startup_message("2.79.0", &MergeReport::default()), "Friday 没开时改过的提示词");
        let merge = MergeReport { added: keys(&["c"]), updated: keys(&["a", "b"]), kept_local: keys(&["d"]), ..Default::default() };
        assert_eq!(
            startup_message("2.79.0", &merge),
            "合并 Friday 2.79.0 自带的提示词：新增 1 段（c）；更新 2 段（a、b）；1 段两边都改了、留了本地的（d）"
        );
        let output =
            "abcdef0123456789\u{1f}2026-10-23T06:07:00+00:00\u{1f}改了 1 段：a\u{1f}b\nbad line\n0000000999\u{1f}不是时间\u{1f}x\n";
        let versions = parse_log(output);
        assert_eq!(versions.len(), 2);
        assert_eq!((versions[0].short_id.as_str(), versions[0].message.as_str()), ("abcdef0", "改了 1 段：a\u{1f}b"));
        let local =
            DateTime::parse_from_rfc3339("2026-10-23T06:07:00+00:00").unwrap().with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string();
        assert_eq!(versions[0].time, local);
        assert_eq!(versions[1].time, "不是时间");
        assert_eq!(version_label(&versions, "abcdef0123456789"), format!("{local} 的版本（改了 1 段：a\u{1f}b）"));
        assert_eq!(version_label(&versions, "abcdef0"), format!("{local} 的版本（改了 1 段：a\u{1f}b）"));
        assert_eq!(version_label(&versions, "fffffff123"), "fffffff");
    }

    #[test]
    fn changes() {
        let _lock = prompts::TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let files = |content: &str| std::iter::once(("prompts-main.md".to_string(), content.to_string())).collect();
        prompts::setup(files("@@ a\nA\n\n@@ b\nB\n"), None, Vec::new());
        mark_committed();
        assert_eq!(describe_changes(), "改了提示词文件（注释或格式）");
        prompts::setup(files("@@ a\nA2\n\n@@ c\nC\n"), None, Vec::new());
        assert_eq!(describe_changes(), "改了 3 段：a、b、c");
        mark_committed();
        assert_eq!(describe_changes(), "改了提示词文件（注释或格式）");
    }
}
