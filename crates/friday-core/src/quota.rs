//! 在线服务的限流和额度：报错属于哪一类、服务端说多久后恢复。按通用的写法认，不针对某一家：
//! 状态码（402 = 要付费）、报错里的字样（rate limit / quota / balance / billing / 余额 / 欠费 / 额度……）、
//! 响应头（Retry-After、x-ratelimit-reset*、anthropic-ratelimit-*-reset）、正文里的「try again in 20s」「将在 … 重置」。
//! 智谱的业务码写法最明确，单独认（1302 并发过高、1308 几小时的使用上限……）。
//!
//! 原来是 Swift `LLM/Engine/QuotaPolicy.swift` 和 Qt `llm/engine/QuotaPolicy.cpp` 各写一份，规则逐条照搬；
//! 两边现在都只是调这里的薄包装。

use std::sync::LazyLock;

use chrono::{DateTime, FixedOffset, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// 限流（每分钟请求数、并发……）：等几秒到几分钟就好，在请求里等
    RateLimit,
    /// 周期额度用完（每日 / 几小时 / 每周 / 每月上限、免费额度）：到点自己恢复
    Usage,
    /// 余额不足、欠费、套餐到期：充值或续费后才恢复
    Balance,
    /// 没有权限、套餐不含这个模型：等也没用
    Denied,
}

impl Limit {
    /// C 接口里的写法（== Swift 的 case 名）
    pub fn name(self) -> &'static str {
        match self {
            Limit::RateLimit => "rateLimit",
            Limit::Usage => "usage",
            Limit::Balance => "balance",
            Limit::Denied => "denied",
        }
    }
}

/// 等额度时在等什么（== Swift QuotaWait.Kind）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitKind {
    Usage,
    Balance,
}

/// 4xx 报错（或流里的报错，status 传 0）属于哪一类；None = 和限流、额度无关（参数错误、密钥不对……按原来的规则）
pub fn classify(status: i64, body: &str) -> Option<Limit> {
    if status != 0 && !(400..500).contains(&status) {
        return None;
    }
    let text = body.to_lowercase();
    if let Some(code) = zhipu_code(&text) {
        match code {
            "1302" | "1303" | "1305" => return Some(Limit::RateLimit),
            "1113" | "1309" => return Some(Limit::Balance),
            "1304" | "1308" | "1310" => return Some(Limit::Usage),
            "1311" => return Some(Limit::Denied),
            _ => {}
        }
    }
    let any = |markers: &[&str]| markers.iter().any(|marker| text.contains(marker));
    if status == 402 || any(BALANCE_MARKERS) || (text.contains("credit") && any(CREDIT_WORDS)) {
        return Some(Limit::Balance);
    }
    if any(DENIED_MARKERS) {
        return Some(Limit::Denied);
    }
    let rate = any(RATE_MARKERS);
    if any(USAGE_MARKERS) {
        return Some(if rate { Limit::RateLimit } else { Limit::Usage });
    }
    if rate || status == 429 { Some(Limit::RateLimit) } else { None }
}

/// 第一个 `"code": "dddd"`（正好四位数字；== 正则 `"code"\s*:\s*"?(\d{4})(?!\d)` 的第一处）
fn zhipu_code(text: &str) -> Option<&str> {
    static CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""code"\s*:\s*"?([0-9]+)"#).unwrap());
    CODE.captures_iter(text).filter_map(|captures| captures.get(1)).find(|digits| digits.len() == 4).map(|digits| digits.as_str())
}

const BALANCE_MARKERS: &[&str] = &[
    "insufficient_quota",
    "insufficient quota",
    "exceeded_current_quota",
    "exceeded your current quota",
    "insufficient balance",
    "insufficient_balance",
    "balance is insufficient",
    "balance not enough",
    "not enough balance",
    "balance is too low",
    "arrearage",
    "arrears",
    "overdue",
    "billing",
    "payment",
    "spending limit",
    "余额不足",
    "余额已用完",
    "欠费",
    "充值",
    "已到期",
    "已过期",
];
const CREDIT_WORDS: &[&str] =
    &["insufficient", "too low", "not enough", "out of", "no credit", "exhausted", "exceeded", "used all", "limit"];
const DENIED_MARKERS: &[&str] =
    &["permission", "not allowed", "not authorized", "unauthorized", "access denied", "无权", "没有权限", "权限不足", "未开放"];
const RATE_MARKERS: &[&str] = &[
    "rate limit",
    "rate_limit",
    "ratelimit",
    "ratequota",
    "rate quota",
    "throttl",
    "too many requests",
    "requests per",
    "tokens per",
    "per min",
    "rpm",
    "tpm",
    "qps",
    "concurren",
    "too frequent",
    "并发",
    "频率",
    "频繁",
];
const USAGE_MARKERS: &[&str] = &[
    "quota",
    "usage limit",
    "usage_limit",
    "resource_exhausted",
    "resource exhausted",
    "free tier",
    "daily limit",
    "weekly limit",
    "monthly limit",
    "使用上限",
    "调用次数",
    "次数已用完",
    "限额",
    "额度",
    "配额",
    "用量",
];

// MARK: - 多久后恢复

/// 会看的响应头（Retry-After 之后按顺序；x-ratelimit-reset* 取最晚的那个）。包装层按这张表取头再交进来
pub const RESET_HEADERS: &[&str] = &[
    "x-ratelimit-reset-requests",
    "x-ratelimit-reset-tokens",
    "x-ratelimit-reset",
    "anthropic-ratelimit-requests-reset",
    "anthropic-ratelimit-tokens-reset",
    "anthropic-ratelimit-input-tokens-reset",
    "anthropic-ratelimit-output-tokens-reset",
];

/// 服务端说多久后恢复（秒）：响应头优先（Retry-After；x-ratelimit-reset* 取最晚的那个），其次正文
///（「try again in 14m25s」「retryDelay": "37s"」「将在 2026-10-05 14:07:26 重置」「今日」的限额到下一个零点）。None = 没说。
/// header：按名字取响应头（不区分大小写；没有返回 None）
pub fn reset_delay(header: impl Fn(&str) -> Option<String>, body: &str, now: DateTime<Utc>) -> Option<f64> {
    if let Some(value) = header("Retry-After").map(|value| value.trim().to_string()).filter(|value| !value.is_empty()) {
        if let Ok(seconds) = value.parse::<f64>() {
            return Some(seconds.max(0.0));
        }
        if let Some(date) = http_date(&value) {
            return Some(seconds_until(date, now).max(0.0));
        }
    }
    let latest = RESET_HEADERS.iter().filter_map(|name| header(name).and_then(|value| reset_value(&value, now))).reduce(f64::max);
    if latest.is_some() {
        return latest;
    }
    body_delay(body, now)
}

/// 响应头里的重置值：时长（「6m0s」「20ms」）、秒数、Unix 时间戳（秒或毫秒）或 ISO 8601 时间
pub fn reset_value(raw: &str, now: DateTime<Utc>) -> Option<f64> {
    let value = raw.trim();
    if let Ok(number) = value.parse::<f64>() {
        let epoch = epoch_seconds(now);
        if number > 1e12 {
            return Some((number / 1000.0 - epoch).max(0.0));
        }
        if number > 1e9 {
            return Some((number - epoch).max(0.0));
        }
        return Some(number.max(0.0));
    }
    if let Ok(date) = DateTime::parse_from_rfc3339(value) {
        return Some(seconds_until(date.with_timezone(&Utc), now).max(0.0));
    }
    duration(value)
}

pub fn body_delay(body: &str, now: DateTime<Utc>) -> Option<f64> {
    static RETRY_DELAY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""retrydelay"\s*:\s*"([0-9]+(?:\.[0-9]+)?)s""#).unwrap());
    static CHINESE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"((?:[0-9]+(?:\.[0-9]+)?\s*(?:小时|分钟|分|秒)\s*)+)后").unwrap());
    static STAMP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"([0-9]{4}-[0-9]{2}-[0-9]{2})[ T]([0-9]{2}:[0-9]{2})(:[0-9]{2})?(?:\.[0-9]+)?(Z|[+-][0-9]{2}:?[0-9]{2})?").unwrap()
    });

    let text = body.to_lowercase();
    if let Some(seconds) = RETRY_DELAY.captures(&text).and_then(|captures| captures[1].parse::<f64>().ok()) {
        return Some(seconds);
    }
    if let Some(seconds) = retry_phrase(&text).and_then(duration) {
        return Some(seconds);
    }
    if let Some(seconds) = CHINESE.captures(&text).and_then(|captures| duration(&captures[1])) {
        return Some(seconds);
    }
    // 写明了恢复时间：取第一个在将来的（不带时区的：中文报错按北京时间——国内的服务都是；其余按 UTC）
    if RESET_WORDS.iter().any(|word| text.contains(word)) {
        let chinese = body.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c));
        let default_zone = FixedOffset::east_opt(if chinese { 8 * 3600 } else { 0 }).unwrap();
        for captures in STAMP.captures_iter(body) {
            let stamp = format!("{}T{}{}", &captures[1], &captures[2], captures.get(3).map_or(":00", |m| m.as_str()));
            let Some(date) = date_time(&stamp, captures.get(4).map(|m| m.as_str()), default_zone) else { continue };
            let delay = seconds_until(date, now);
            if delay > 0.0 && delay < 31.0 * 86400.0 {
                return Some(delay);
            }
        }
    }
    // 当日的限额：到下一个零点（按本机时区）
    if DAILY_WORDS.iter().any(|word| text.contains(word)) {
        let tomorrow = now.with_timezone(&Local).date_naive().succ_opt()?;
        let midnight = Local.from_local_datetime(&tomorrow.and_hms_opt(0, 0, 0)?).earliest()?;
        return Some(seconds_until(midnight.with_timezone(&Utc), now) + 60.0);
    }
    None
}

const RESET_WORDS: &[&str] = &["重置", "恢复", "reset", "available", "resume", "refresh", "flush"];
const DAILY_WORDS: &[&str] = &["今日", "今天", "当日", "当天", "每日", "daily", "per day", "per-day", "today"];

/// 「try again in 14m25s」这类说法里的时长那一段
/// （== 正则 `(?:try again|retry|please wait)\s+(?:in\s+|after\s+)?((?:<时长>\s*)+)` 的第一处的第 1 组）
fn retry_phrase(text: &str) -> Option<&str> {
    static LEAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:try again|retry|please wait)\s+").unwrap());
    static CONNECTOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:in\s+|after\s+)").unwrap());
    for (start, _) in text.char_indices() {
        let Some(lead) = LEAD.find(&text[start..]) else { continue };
        let rest = &text[start + lead.end()..];
        // 先试带 in / after 的，再试不带的（== 正则里 `?` 先贪婪后回退）
        let connector = CONNECTOR.find(rest).map_or(0, |m| m.end());
        for skip in [connector, 0] {
            let length = duration_run(&rest[skip..], false);
            if length > 0 {
                return Some(&rest[skip..skip + length]);
            }
        }
    }
    None
}

/// 从开头起连着的「数字 + 单位 + 空白」有多长（字节；0 = 开头不是时长）
fn duration_run(text: &str, chinese_units: bool) -> usize {
    let mut length = 0;
    while let Some((consumed, _)) = duration_part(&text[length..], chinese_units) {
        length += consumed;
        length += leading_whitespace(&text[length..]);
    }
    length
}

const UNITS: &[&str] = &[
    "milliseconds",
    "millisecond",
    "ms",
    "hours",
    "hour",
    "hrs",
    "hr",
    "h",
    "minutes",
    "minute",
    "mins",
    "min",
    "m",
    "seconds",
    "second",
    "secs",
    "sec",
    "s",
];
const CHINESE_UNITS: &[&str] = &["小时", "分钟", "分", "秒"];

/// 开头的一段时长：`[0-9]+(\.[0-9]+)?\s*单位`，单位后面不能紧跟字母（== `(?![a-z])`）。返回（字节数，秒数）
fn duration_part(text: &str, chinese_units: bool) -> Option<(usize, f64)> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let mut number_end = digits;
    let bytes = text.as_bytes();
    if bytes.get(digits) == Some(&b'.') {
        let fraction = text[digits + 1..].bytes().take_while(u8::is_ascii_digit).count();
        if fraction > 0 {
            number_end = digits + 1 + fraction;
        }
    }
    let value: f64 = text[..number_end].parse().ok()?;
    let unit_start = number_end + leading_whitespace(&text[number_end..]);
    let rest = &text[unit_start..];
    let chinese: &[&str] = if chinese_units { CHINESE_UNITS } else { &[] };
    let unit = UNITS
        .iter()
        .chain(chinese)
        .find(|unit| rest.starts_with(**unit) && !rest[unit.len()..].chars().next().is_some_and(|c| c.is_ascii_lowercase()))?;
    let seconds = match *unit {
        "ms" | "millisecond" | "milliseconds" => value / 1000.0,
        "h" | "hr" | "hrs" | "hour" | "hours" | "小时" => value * 3600.0,
        "m" | "min" | "mins" | "minute" | "minutes" | "分钟" | "分" => value * 60.0,
        _ => value,
    };
    Some((unit_start + unit.len(), seconds))
}

fn leading_whitespace(text: &str) -> usize {
    text.char_indices().find(|(_, c)| !c.is_whitespace()).map_or(text.len(), |(index, _)| index)
}

/// 「14m25.5s」「6m0s」「20ms」「1 seconds」「5 分钟」这样的时长（秒）：把里面所有的段加起来
pub fn duration(text: &str) -> Option<f64> {
    let text = text.to_lowercase();
    let mut total = 0.0;
    let mut found = false;
    let mut index = 0;
    while index < text.len() {
        if let Some((consumed, seconds)) = duration_part(&text[index..], true) {
            total += seconds;
            found = true;
            index += consumed;
        } else {
            index += text[index..].chars().next().map_or(1, char::len_utf8);
        }
    }
    found.then_some(total)
}

/// 第 attempt 次（1 起）等额度、服务端没说什么时候恢复时，多久后再试（秒）：用量类 10、20、30 分钟，之后每小时；余额类每 30 分钟
pub fn probe_delay(kind: WaitKind, attempt: i64) -> f64 {
    const STEPS: [f64; 4] = [600.0, 1200.0, 1800.0, 3600.0];
    match kind {
        WaitKind::Balance => 1800.0,
        WaitKind::Usage => STEPS[(attempt.clamp(1, STEPS.len() as i64) - 1) as usize],
    }
}

// MARK: - 等额度（两端的 AgentRunner 停下这一轮、到点接着做；这里是等到什么时候和消息上的文案）

/// 这一次等到什么时候（== 两端的 QuotaWait）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wait {
    pub kind: WaitKind,
    /// 下次自动接着做的时间
    pub resume_at: DateTime<Utc>,
    /// resume_at 是服务端说的恢复时间（否则是没说、按间隔定时试）
    pub known: bool,
    /// 第几次等（没说什么时候恢复时，再试的间隔按它拉长）
    pub attempts: i64,
}

/// 服务端说了恢复时间（且在将来）就到点后再过 30 秒（两边的钟可能差一点）；没说、或说的时间已经过了，按间隔定时试
pub fn wait_for(kind: WaitKind, resume_at: Option<DateTime<Utc>>, attempts: i64, now: DateTime<Utc>) -> Wait {
    match resume_at {
        Some(resume_at) if resume_at > now => Wait { kind, resume_at: resume_at + chrono::Duration::seconds(30), known: true, attempts },
        _ => Wait {
            kind,
            resume_at: now + chrono::Duration::milliseconds((probe_delay(kind, attempts) * 1000.0).round() as i64),
            known: false,
            attempts,
        },
    }
}

/// 消息上「在等额度」那一行：在等什么、什么时候接着做、还有多久
pub fn wait_text(kind: WaitKind, resume_at: DateTime<Utc>, known: bool, now: DateTime<Utc>) -> String {
    let remaining = (resume_at - now).num_milliseconds() as f64 / 1000.0;
    if remaining <= 0.0 {
        return "额度应该恢复了，马上接着做".to_string();
    }
    let time = clock_text(resume_at, now);
    let left = remaining_text(remaining);
    match (kind, known) {
        (WaitKind::Usage, true) => format!("额度用完了，预计 {time} 恢复（还有 {left}），到时自动接着做"),
        (WaitKind::Usage, false) => format!("额度用完了，{time}（{left}后）再试一次，恢复了就自动接着做"),
        (WaitKind::Balance, _) => format!("余额不足或欠费，充值后会自动接着做：{time}（{left}后）再试一次"),
    }
}

/// 「14:07」；不是今天的带上日期「10月5日 14:07」（本机时区）
pub fn clock_text(date: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let local = date.with_timezone(&Local);
    if local.date_naive() == now.with_timezone(&Local).date_naive() {
        local.format("%H:%M").to_string()
    } else {
        local.format("%-m月%-d日 %H:%M").to_string()
    }
}

/// 还有多久：按分钟往上取整（至少 1 分钟）——「25 分钟」「3 小时 5 分」「2 天 4 小时」
pub fn remaining_text(seconds: f64) -> String {
    let minutes = ((seconds / 60.0).ceil() as i64).max(1);
    if minutes < 60 {
        return format!("{minutes} 分钟");
    }
    let (hours, rest) = (minutes / 60, minutes % 60);
    if hours < 24 {
        return if rest > 0 { format!("{hours} 小时 {rest} 分") } else { format!("{hours} 小时") };
    }
    if hours % 24 > 0 { format!("{} 天 {} 小时", hours / 24, hours % 24) } else { format!("{} 天", hours / 24) }
}

fn epoch_seconds(date: DateTime<Utc>) -> f64 {
    date.timestamp_millis() as f64 / 1000.0
}

fn seconds_until(date: DateTime<Utc>, now: DateTime<Utc>) -> f64 {
    (date - now).num_milliseconds() as f64 / 1000.0
}

/// HTTP 日期（RFC 1123：「Sun, 04 Oct 2026 06:05:00 GMT」）
fn http_date(text: &str) -> Option<DateTime<Utc>> {
    let stamp = text.trim().strip_suffix(" GMT")?;
    NaiveDateTime::parse_from_str(stamp, "%a, %d %b %Y %H:%M:%S").ok().map(|date| date.and_utc())
}

/// 「yyyy-MM-ddTHH:mm:ss」加上时区（「Z」「+08:00」「+0800」；没有就用 default_zone）
fn date_time(stamp: &str, zone: Option<&str>, default_zone: FixedOffset) -> Option<DateTime<Utc>> {
    let date = NaiveDateTime::parse_from_str(stamp, "%Y-%m-%dT%H:%M:%S").ok()?;
    let offset = match zone {
        None => default_zone,
        Some("Z") => FixedOffset::east_opt(0)?,
        Some(zone) => {
            let digits: String = zone.chars().filter(char::is_ascii_digit).collect();
            if digits.len() != 4 {
                return None;
            }
            let hours: i32 = digits[..2].parse().ok()?;
            let minutes: i32 = digits[2..].parse().ok()?;
            FixedOffset::east_opt((hours * 3600 + minutes * 60) * if zone.starts_with('-') { -1 } else { 1 })?
        }
    };
    offset.from_local_datetime(&date).single().map(|date| date.with_timezone(&Utc))
}

/// 2026-10-04 06:00:00 UTC 这样的固定时间（测试用）
#[doc(hidden)]
pub fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> DateTime<Utc> {
    NaiveDate::from_ymd_opt(year, month, day).unwrap().and_hms_opt(hour, minute, second).unwrap().and_utc()
}

/// 毫秒时间戳 → 时间（C 接口里 now 用毫秒传）
pub fn from_epoch_millis(millis: i64) -> Option<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp_millis(millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// == Swift 自检 quotaPolicy 和 Qt TestEngines::quotaClassification 的同一张表
    #[test]
    fn classification() {
        use Limit::*;
        let samples: &[(&str, i64, &str, Option<Limit>)] = &[
            (
                "OpenAI 额度",
                429,
                r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota","code":"insufficient_quota"}}"#,
                Some(Balance),
            ),
            (
                "OpenAI 限流",
                429,
                r#"{"error":{"message":"Rate limit reached for gpt-4o on tokens per min (TPM): Limit 30000. Please try again in 2s.","type":"tokens","code":"rate_limit_exceeded"}}"#,
                Some(RateLimit),
            ),
            ("DeepSeek 余额", 402, r#"{"error":{"message":"Insufficient Balance","type":"unknown_error"}}"#, Some(Balance)),
            ("智谱 1113", 429, r#"{"error":{"code":"1113","message":"余额不足或无可用资源包,请充值。"}}"#, Some(Balance)),
            (
                "智谱 1308",
                429,
                r#"{"error":{"code":"1308","message":"已达到 5 小时的使用上限。您的限额将在 2026-10-04 19:07:26 重置。"}}"#,
                Some(Usage),
            ),
            (
                "智谱 1310",
                429,
                r#"{"error":{"code":"1310","message":"您已达到每周/每月使用上限，您的限额将在 2026-10-06 00:00:00 重置"}}"#,
                Some(Usage),
            ),
            (
                "智谱 1304",
                429,
                r#"{"error":{"code":"1304","message":"该 API 已达今日调用次数限额，如有更多需求，请联系客服购买"}}"#,
                Some(Usage),
            ),
            (
                "智谱 1302",
                429,
                r#"{"error":{"code":"1302","message":"您当前使用该API的并发数过高，请降低并发，或联系客服增加限额"}}"#,
                Some(RateLimit),
            ),
            ("智谱 1311", 429, r#"{"error":{"code":"1311","message":"当前订阅套餐暂未开放该模型"}}"#, Some(Denied)),
            ("智谱 1309", 429, r#"{"error":{"code":"1309","message":"您的套餐已到期，请续费后再使用"}}"#, Some(Balance)),
            (
                "Kimi 余额",
                429,
                r#"{"error":{"message":"Your account org-x is suspended, please check your plan and billing details","type":"exceeded_current_quota_error"}}"#,
                Some(Balance),
            ),
            (
                "Kimi 限流",
                429,
                r#"{"error":{"message":"Your account org-x request reached organization max RPM: 3, please try again after 1 seconds","type":"rate_limit_reached_error"}}"#,
                Some(RateLimit),
            ),
            (
                "百炼欠费",
                400,
                r#"{"error":{"code":"Arrearage","message":"Access denied, please make sure your account is in good standing.","type":"Arrearage"}}"#,
                Some(Balance),
            ),
            (
                "百炼限流",
                429,
                r#"{"error":{"code":"Throttling.RateQuota","message":"Requests rate limit exceeded, please try again later."}}"#,
                Some(RateLimit),
            ),
            (
                "百炼免费额度",
                403,
                r#"{"error":{"code":"AllocationQuota.FreeTierOnly","message":"The free tier of the model has been exhausted."}}"#,
                Some(Usage),
            ),
            (
                "火山方舟欠费",
                403,
                r#"{"error":{"code":"AccountOverdueError","message":"The request failed because your account has an overdue balance.","type":"Forbidden"}}"#,
                Some(Balance),
            ),
            (
                "火山方舟限流",
                429,
                r#"{"error":{"code":"RateLimitExceeded.EndpointRPMExceeded","message":"The request failed because the endpoint has exceeded the RPM limit.","type":"TooManyRequests"}}"#,
                Some(RateLimit),
            ),
            ("硅基流动余额", 403, r#"{"code":30001,"message":"Sorry, your account balance is insufficient","data":null}"#, Some(Balance)),
            (
                "OpenRouter 余额",
                402,
                r#"{"error":{"message":"Insufficient credits. Add more in your account settings.","code":402}}"#,
                Some(Balance),
            ),
            (
                "Anthropic 余额",
                400,
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API."}}"#,
                Some(Balance),
            ),
            (
                "Anthropic 限流",
                429,
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"This request would exceed the rate limit for your organization."}}"#,
                Some(RateLimit),
            ),
            (
                "xAI 余额",
                403,
                r#"{"error":"Your team has either used all available credits or reached its monthly spending limit."}"#,
                Some(Balance),
            ),
            (
                "Groq 每日",
                429,
                r#"{"error":{"message":"Rate limit reached for model on tokens per day (TPD): Limit 100000. Please try again in 14m25.5s.","type":"tokens"}}"#,
                Some(RateLimit),
            ),
            ("上下文超长", 400, r#"{"error":{"message":"This model has a maximum context length of 128000 tokens."}}"#, None),
            ("密钥不对", 401, r#"{"error":{"message":"Incorrect API key provided"}}"#, None),
            ("服务繁忙", 503, r#"{"error":{"message":"quota service unavailable"}}"#, None),
        ];
        for (name, status, body, expected) in samples {
            assert_eq!(classify(*status, body), *expected, "{name}");
        }
        assert_eq!(classify(0, "insufficient balance"), Some(Balance), "流里报的余额不足");
        // 五位数的 code 不是智谱的业务码，接着找后面的
        assert_eq!(classify(429, r#"{"code":"13080","x":{"code":"1302"}}"#), Some(RateLimit));
    }

    /// == Swift 自检和 Qt TestEngines::quotaResetDelay 的同一组：2026-10-04 06:00:00 UTC = 北京时间 14:00
    #[test]
    fn reset_delays() {
        let now = utc(2026, 10, 4, 6, 0, 0);
        assert_eq!(now.timestamp(), 1_791_093_600);
        let delay = |headers: &[(&str, &str)], body: &str| {
            reset_delay(
                |name| headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.to_string()),
                body,
                now,
            )
        };
        assert_eq!(delay(&[("Retry-After", "120")], ""), Some(120.0));
        assert_eq!(delay(&[("Retry-After", "Sun, 04 Oct 2026 06:05:00 GMT")], ""), Some(300.0));
        assert_eq!(delay(&[("x-ratelimit-reset-requests", "1s"), ("x-ratelimit-reset-tokens", "6m0s")], ""), Some(360.0));
        assert_eq!(delay(&[("X-RateLimit-Reset", "1791097200000")], ""), Some(3600.0));
        assert_eq!(delay(&[("anthropic-ratelimit-tokens-reset", "2026-10-04T06:01:00Z")], ""), Some(60.0));
        assert_eq!(delay(&[], "Please try again in 14m25.5s."), Some(865.5));
        assert_eq!(delay(&[], r#"Please retry in 37.18s. "retryDelay": "37s""#), Some(37.0));
        assert_eq!(delay(&[], "please try again after 1 seconds"), Some(1.0));
        assert_eq!(delay(&[], "请 30 秒后重试"), Some(30.0));
        assert_eq!(delay(&[], "已达到 5 小时的使用上限。您的限额将在 2026-10-04 19:07:26 重置。"), Some(18446.0));
        assert_eq!(delay(&[], "limit resets at 2026-10-04T07:00:00Z"), Some(3600.0));
        assert_eq!(delay(&[], "您的限额将在 2026-10-01 00:00:00 重置"), None);
        let daily = delay(&[], "该 API 已达今日调用次数限额").unwrap_or(-1.0);
        assert!(daily > 60.0 && daily <= 86460.0, "{daily}");
        assert_eq!(delay(&[], "You exceeded your current quota"), None);
        assert_eq!(duration("1h2m3.5s"), Some(3723.5));
        assert_eq!(duration("20ms"), Some(0.02));
        assert_eq!(duration("5 分钟"), Some(300.0));
        assert_eq!(duration("5 mins 3secs"), Some(303.0));
        assert_eq!(duration("abc"), None);
        // 单位后面紧跟字母的不算（「5 mixed」不是 5 分钟）
        assert_eq!(duration("5 mixed"), None);
        // 「retry again in 3s」：retry 后面不是时长，再往后找到 try again
        assert_eq!(delay(&[], "please retry again in 3s"), Some(3.0));
        // Retry-After 是空白时看后面的
        assert_eq!(delay(&[("Retry-After", "  "), ("x-ratelimit-reset", "7")], ""), Some(7.0));
    }

    /// == Swift 自检 quotaPolicy 后半和 Qt TestRunner::quotaWaitTexts
    #[test]
    fn waits() {
        let now = utc(2026, 10, 4, 6, 0, 0);
        let known = wait_for(WaitKind::Usage, Some(now + chrono::Duration::seconds(3600)), 1, now);
        assert!(known.known);
        assert_eq!(known.resume_at, now + chrono::Duration::seconds(3630));
        let unknown = wait_for(WaitKind::Balance, None, 2, now);
        assert!(!unknown.known && unknown.attempts == 2);
        assert_eq!(unknown.resume_at, now + chrono::Duration::seconds(1800));
        // 说的恢复时间已经过了：按间隔
        let past = wait_for(WaitKind::Usage, Some(now - chrono::Duration::seconds(10)), 3, now);
        assert!(!past.known);
        assert_eq!(past.resume_at, now + chrono::Duration::seconds(1800));

        assert_eq!(remaining_text(30.0), "1 分钟");
        assert_eq!(remaining_text(-5.0), "1 分钟");
        assert_eq!(remaining_text(3_600.0), "1 小时");
        assert_eq!(remaining_text(3_720.0), "1 小时 2 分");
        assert_eq!(remaining_text(3_661.0), "1 小时 2 分");
        assert_eq!(remaining_text(2.0 * 86_400.0 + 4.0 * 3_600.0), "2 天 4 小时");
        assert_eq!(remaining_text(86_400.0), "1 天");

        let later = now + chrono::Duration::seconds(3_720);
        let text = wait_text(WaitKind::Usage, later, true, now);
        assert!(text.starts_with("额度用完了，预计 ") && text.ends_with("恢复（还有 1 小时 2 分），到时自动接着做"), "{text}");
        assert!(wait_text(WaitKind::Usage, later, false, now).ends_with("（1 小时 2 分后）再试一次，恢复了就自动接着做"));
        assert!(wait_text(WaitKind::Balance, later, false, now).starts_with("余额不足或欠费，充值后会自动接着做："));
        assert_eq!(wait_text(WaitKind::Balance, now, false, now), "额度应该恢复了，马上接着做");

        assert_eq!(clock_text(now, now), now.with_timezone(&Local).format("%H:%M").to_string());
        let other_day = now + chrono::Duration::days(3);
        let local = other_day.with_timezone(&Local);
        assert_eq!(clock_text(other_day, now), format!("{}月{}日 {}", local.format("%-m"), local.format("%-d"), local.format("%H:%M")));
    }

    #[test]
    fn probe_delays() {
        let usage: Vec<f64> = (1..=5).map(|attempt| probe_delay(WaitKind::Usage, attempt)).collect();
        assert_eq!(usage, [600.0, 1200.0, 1800.0, 3600.0, 3600.0]);
        assert_eq!(probe_delay(WaitKind::Usage, 0), 600.0);
        assert_eq!(probe_delay(WaitKind::Balance, 3), 1800.0);
    }
}
