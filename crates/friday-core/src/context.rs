//! 上下文：按模型的上下文上限压一次请求的历史（== Swift `LLM/Engine/ContextFit.swift`、Qt `llm/engine/ContextFit.cpp`，本地大模型用），
//! 上下文由哪几部分组成、模型的上限（== `ContextUsage.swift`、`ContextUsage.cpp`，输入框下的上下文进度环），
//! 以及各模型的上限、估算校准、服务商报的上限（本次运行内记住）。
//!
//! 压法（从最早的开始，够了就停）：
//! 1. 摘要：工具结果、长参数、长正文换成摘要（有会话折叠的设置就用它的写法：子智能体的结果不截），思考去掉（echoesReasoning 的留着），
//!    较早用户消息里的图片换成一句说明；
//! 2. 还不够：整段去掉（用户消息自成一段，助手消息连同后面的工具结果一段），去掉的地方留一句说明。
//!
//! 最新的一步（最后一条助手消息或用户消息起）和最后一条用户消息一律不动：模型还没看过它们。只改引擎内存里的历史，不改存档。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use serde_json::{Map, Value, json};
use unicode_segmentation::UnicodeSegmentation;

use crate::folding::{self, Special};
use crate::prompts;

/// 留给回复的：上限的 1/8，至少 2048（不超过上限的一半）
pub fn reserve(limit: i64) -> i64 {
    (limit / 8).max(2_048).min(limit / 2)
}

/// 整段请求（估算 × 校准系数）超过它才压
pub fn threshold(limit: i64) -> i64 {
    limit - reserve(limit)
}

/// 一压就压到这里：threshold 的四分之三
pub fn target(limit: i64) -> i64 {
    threshold(limit) * 3 / 4
}

/// 整段请求不到这么多 token 就不问上限（再小的模型也放得下）
pub const SMALL_REQUEST: i64 = 2_048;

/// 摘要的写法（== 两端的 SessionFolding）：没有时用默认的（起标题这类旁路请求）
pub struct Summaries {
    pub echoes_reasoning: bool,
    pub special: Special,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Compacted {
    /// 换成摘要的条数
    pub shortened: usize,
    /// 整段去掉的条数
    pub dropped: usize,
    /// 历史压缩前、后的估算 token
    pub before: i64,
    pub after: i64,
}

/// 去掉的地方留的说明
pub fn omitted_note(count: usize) -> String {
    prompts::text("context-fit.omitted", &HashMap::from([("count".to_string(), count.to_string())]), None)
}

fn tokens(entry: &Value) -> i64 {
    folding::estimated_tokens(std::slice::from_ref(entry))
}

fn role(entry: &Value) -> &str {
    entry.get("role").and_then(Value::as_str).unwrap_or("")
}

/// 把 history 压到 budget（估算 token）以内；压不到也尽量压。folded：调用方记的「前多少条已经折叠过」，去掉前面的消息时跟着挪
pub fn compact(history: &mut Vec<Value>, folded: &mut usize, budget: i64, summaries: Option<&Summaries>) -> Compacted {
    let mut marks = [*folded];
    let result = compact_marks(history, &mut marks, budget, summaries);
    *folded = marks[0];
    result
}

/// 同 [`compact`]；marks：调用方记的几个位置（前多少条已经折叠过、v2.86 这一轮从哪条起……），去掉前面的消息时一起挪（== Swift / Qt 的 marks）
pub fn compact_marks(history: &mut Vec<Value>, marks: &mut [usize], budget: i64, summaries: Option<&Summaries>) -> Compacted {
    let mut result = Compacted { before: folding::estimated_tokens(history), ..Compacted::default() };
    let mut total = result.before;
    let count = history.len();
    // 不动的：最新的一步（最后一条助手消息或用户消息起，取靠后的）和最后一条用户消息
    let last_user = history.iter().rposition(|entry| role(entry) == "user");
    let keep_from = history.iter().rposition(|entry| matches!(role(entry), "user" | "assistant")).unwrap_or(count);

    // 1. 摘要：从最早的开始
    let mut index = 0;
    while index < keep_from && total > budget {
        if Some(index) != last_user {
            let shorter = summarized(&history[index], summaries);
            let saved = tokens(&history[index]) - tokens(&shorter);
            if saved > 0 {
                history[index] = shorter;
                total -= saved;
                result.shortened += 1;
            }
        }
        index += 1;
    }

    // 2. 整段去掉：从最早的开始。一段 = 一条用户消息，或一条助手消息连同后面的工具结果（工具调用和结果一起去，请求才合法）
    let mut drop = vec![false; count];
    index = 0;
    while index < keep_from && total > budget {
        let mut end = index + 1;
        while end < keep_from && role(&history[end]) == "tool" {
            end += 1;
        }
        if Some(index) != last_user && role(&history[index]) != "system" {
            for k in index..end {
                drop[k] = true;
                total -= tokens(&history[k]);
                result.dropped += 1;
            }
        }
        index = end;
    }
    if result.dropped > 0 {
        // 去掉的每一段连续的地方留一句说明：前面留下的是用户消息就接在它后面，后面是用户消息就放在它前面，
        // 都不是才单独插一条用户消息（有的聊天模板不认连着两条用户消息）
        let mut rebuilt: Vec<Value> = Vec::new();
        let mut new_marks: Vec<Option<usize>> = vec![None; marks.len()];
        index = 0;
        while index < count {
            for (mark, new) in marks.iter().zip(new_marks.iter_mut()) {
                if index >= *mark && new.is_none() {
                    *new = Some(rebuilt.len());
                }
            }
            if !drop[index] {
                rebuilt.push(history[index].clone());
                index += 1;
                continue;
            }
            let mut end = index;
            while end < count && drop[end] {
                end += 1;
            }
            let note = omitted_note(end - index);
            if let Some(last) = rebuilt.last_mut().filter(|last| role(last) == "user") {
                *last = with_note(last, &note, false);
            } else if end < count && role(&history[end]) == "user" {
                history[end] = with_note(&history[end], &note, true);
            } else {
                rebuilt.push(json!({ "role": "user", "content": note }));
            }
            index = end;
        }
        for (mark, new) in marks.iter_mut().zip(new_marks) {
            *mark = new.unwrap_or(rebuilt.len());
        }
        *history = rebuilt;
    }
    result.after = folding::estimated_tokens(history);
    result
}

/// 一条较早的消息换成摘要（不是更短就不换）
fn summarized(entry: &Value, summaries: Option<&Summaries>) -> Value {
    let mut entry = entry.clone();
    let Some(object) = entry.as_object_mut() else { return entry };
    match object.get("role").and_then(Value::as_str).unwrap_or("") {
        "tool" => {
            if let Some(content) = object.get("content").and_then(Value::as_str) {
                let name = object.get("name").and_then(Value::as_str).unwrap_or("");
                let shorter = match summaries {
                    Some(summaries) => folding::tool_result(name, content, &summaries.special),
                    None => default_tool_result(content),
                };
                object.insert("content".into(), json!(shorter));
            }
        }
        "assistant" => {
            if let Some(content) = object.get("content").and_then(Value::as_str) {
                let shorter = if summaries.is_some() { folding::text(content) } else { default_text(content) };
                object.insert("content".into(), json!(shorter));
            }
            if !summaries.is_some_and(|summaries| summaries.echoes_reasoning) {
                object.remove("reasoning_content");
            }
            if summaries.is_some()
                && let Some(Value::Array(calls)) = object.get_mut("tool_calls")
            {
                for call in calls.iter_mut() {
                    if let Some(function) = call.get_mut("function").and_then(Value::as_object_mut)
                        && let Some(json) = function.get("arguments").and_then(Value::as_str)
                    {
                        let shorter = folding::arguments_json(json);
                        function.insert("arguments".into(), json!(shorter));
                    }
                }
            }
        }
        "user" => {
            // 较早消息里的图片（视频抽的帧）：多模态模型每张要上千 token，换成一句说明
            if let Some(parts) = object.get("content").and_then(Value::as_array).filter(|parts| parts.iter().all(Value::is_object)) {
                let kept: Vec<Value> =
                    parts.iter().filter(|part| part.get("type").and_then(Value::as_str) != Some("image_url")).cloned().collect();
                let images = parts.len() - kept.len();
                if images > 0 {
                    let mut content = kept;
                    content.push(json!({ "type": "text", "text": format!("（这条消息附的 {images} 张图片已省略，以节省上下文）") }));
                    object.insert("content".into(), Value::Array(content));
                }
            }
        }
        _ => {}
    }
    entry
}

/// 前 head 个字 + 说明 + 后 tail 个字（按用户看到的字算）
fn clipped(text: &str, limit: usize, head: usize, tail: usize, note: impl Fn(usize) -> String) -> String {
    let parts: Vec<&str> = text.graphemes(true).collect();
    if parts.len() <= limit {
        return text.to_string();
    }
    parts[..head].concat() + &note(parts.len()) + &parts[parts.len() - tail..].concat()
}

/// 默认的摘要（没有会话折叠的设置时）：同 ContextFolding 的长度，不认工具名
fn default_tool_result(text: &str) -> String {
    clipped(text, 500, 300, 150, |count| format!("\n…（较早的工具结果，已省略，原 {count} 字；需要细节就重新调用工具）…\n"))
}

fn default_text(text: &str) -> String {
    clipped(text, 1_500, 1_000, 300, |count| format!("\n…（较早的回复，已省略，原 {count} 字）…\n"))
}

/// 用户消息的正文前面 / 后面接一句说明（多模态的接在文字部分上）
fn with_note(entry: &Value, note: &str, before: bool) -> Value {
    let mut entry = entry.clone();
    let Some(object) = entry.as_object_mut() else { return entry };
    let content = match object.get("content") {
        Some(Value::Array(parts)) if parts.iter().all(Value::is_object) => {
            let text = json!({ "type": "text", "text": note });
            let mut parts = parts.clone();
            if before {
                parts.insert(0, text)
            } else {
                parts.push(text)
            }
            Value::Array(parts)
        }
        other => {
            let body = other.and_then(Value::as_str).unwrap_or("");
            json!(if body.is_empty() {
                note.to_string()
            } else if before {
                format!("{note}\n\n{body}")
            } else {
                format!("{body}\n\n{note}")
            })
        }
    };
    object.insert("content".into(), content);
    entry
}

// MARK: - 上下文由哪几部分组成（输入框下的进度环点开看明细）

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Parts {
    /// 系统提示（框架、记忆、技能、项目说明……都在里面）
    pub system: i64,
    /// 工具定义
    pub tools: i64,
    /// 用户消息（含 Friday 插进去的检查、补充；图片按每张 1000 估）
    pub user: i64,
    /// 助手的正文和思考
    pub replies: i64,
    /// 工具调用的参数和工具结果
    pub tool_use: i64,
}

impl Parts {
    pub fn total(&self) -> i64 {
        self.system + self.tools + self.user + self.replies + self.tool_use
    }

    /// 按字数估算（同 fold.estimate 的算法：UTF-8 字节数 / 3.5）：instructions 系统提示，tool_bytes 工具定义的 JSON 字节数，history 引擎里的历史
    pub fn estimate(instructions: &str, tool_bytes: i64, history: &[Value]) -> Parts {
        let mut bytes = Parts { system: instructions.len() as i64, tools: tool_bytes, ..Parts::default() };
        let str_len = |value: Option<&Value>| value.and_then(Value::as_str).map_or(0, |text| text.len() as i64);
        for entry in history {
            let text = match entry.get("content") {
                Some(Value::String(content)) => content.len() as i64,
                Some(Value::Array(parts)) if parts.iter().all(Value::is_object) => {
                    parts.iter().map(|part| part.get("text").and_then(Value::as_str).map_or(1_000, |text| text.len() as i64)).sum()
                }
                _ => 0,
            };
            match role(entry) {
                "system" => bytes.system += text,
                "user" => bytes.user += text,
                "tool" => bytes.tool_use += text,
                _ => {
                    bytes.replies += text + str_len(entry.get("reasoning_content"));
                    let calls = entry.get("tool_calls").and_then(Value::as_array).filter(|calls| calls.iter().all(Value::is_object));
                    for call in calls.into_iter().flatten() {
                        let function = call.get("function");
                        bytes.tool_use +=
                            str_len(function.and_then(|f| f.get("name"))) + str_len(function.and_then(|f| f.get("arguments")));
                    }
                }
            }
        }
        let tokens = |bytes: i64| (bytes as f64 / 3.5) as i64;
        Parts {
            system: tokens(bytes.system),
            tools: tokens(bytes.tools),
            user: tokens(bytes.user),
            replies: tokens(bytes.replies),
            tool_use: tokens(bytes.tool_use),
        }
    }

    /// 等比例缩放到合计 total（服务端报的实际数）；四舍五入差的几个 token 记到最大的一项
    pub fn scaled(&self, total: i64) -> Parts {
        let sum = self.total();
        if sum <= 0 || total <= 0 {
            return *self;
        }
        let ratio = total as f64 / sum as f64;
        let scale = |value: i64| (value as f64 * ratio).round() as i64;
        let mut result = Parts {
            system: scale(self.system),
            tools: scale(self.tools),
            user: scale(self.user),
            replies: scale(self.replies),
            tool_use: scale(self.tool_use),
        };
        let rest = total - result.total();
        let largest = result.system.max(result.tools).max(result.user).max(result.replies).max(result.tool_use);
        if result.system == largest {
            result.system += rest;
        } else if result.tools == largest {
            result.tools += rest;
        } else if result.user == largest {
            result.user += rest;
        } else if result.replies == largest {
            result.replies += rest;
        } else {
            result.tool_use += rest;
        }
        result
    }

    pub fn to_json(self) -> Value {
        json!({ "system": self.system, "tools": self.tools, "user": self.user, "replies": self.replies, "toolUse": self.tool_use })
    }

    pub fn from_json(object: &Map<String, Value>) -> Parts {
        let field = |key: &str| object.get(key).and_then(Value::as_i64).unwrap_or(0);
        Parts { system: field("system"), tools: field("tools"), user: field("user"), replies: field("replies"), tool_use: field("toolUse") }
    }
}

/// 进度环、明细里的数：1234 → 1.2K，131072 → 131K，1000000 → 1M
pub fn compact_number(tokens: i64) -> String {
    let trimmed = |value: f64| if value >= 100.0 { (value.round() as i64).to_string() } else { format!("{value:.1}").replace(".0", "") };
    if tokens >= 1_000_000 {
        trimmed(tokens as f64 / 1_000_000.0) + "M"
    } else if tokens >= 1_000 {
        trimmed(tokens as f64 / 1_000.0) + "K"
    } else {
        tokens.to_string()
    }
}

// MARK: - 各模型的上限和估算校准、服务商报的上限（本次运行内记住）

#[derive(Default)]
struct Memory {
    /// 引擎 id|模型 → 上限（本地大模型问 friday-llm 的 n_ctx_seq、请求超长时服务端报错里的）
    limits: HashMap<String, i64>,
    /// 引擎 id|模型 → 实际 prompt_tokens ÷ 按字节估算的
    ratios: HashMap<String, f64>,
    /// 接口地址|模型名（都小写）→ 服务商在模型列表里报的上限
    provider: HashMap<String, i64>,
}

static MEMORY: LazyLock<Mutex<Memory>> = LazyLock::new(|| Mutex::new(Memory::default()));

pub fn known_limit(key: &str) -> Option<i64> {
    MEMORY.lock().unwrap().limits.get(key).copied()
}

pub fn remember_limit(key: &str, limit: i64) {
    if limit > 0 {
        MEMORY.lock().unwrap().limits.insert(key.to_string(), limit);
    }
}

/// 实际 prompt_tokens ÷ 按字节估算的（0.5–4）；没量过是 1
pub fn ratio(key: &str) -> f64 {
    MEMORY.lock().unwrap().ratios.get(key).copied().unwrap_or(1.0)
}

pub fn remember_ratio(key: &str, ratio: f64) {
    if ratio.is_finite() {
        MEMORY.lock().unwrap().ratios.insert(key.to_string(), ratio.clamp(0.5, 4.0));
    }
}

fn normalized(base_url: &str) -> String {
    base_url.trim().to_lowercase().trim_end_matches('/').to_string()
}

pub fn provider_limit(base_url: &str, model: &str) -> Option<i64> {
    MEMORY.lock().unwrap().provider.get(&format!("{}|{}", normalized(base_url), model.trim().to_lowercase())).copied()
}

/// v2.77 服务商在模型列表（GET {baseURL}/models）里报的上下文长度：（模型 id，上限）
pub fn remember_provider(base_url: &str, entries: &[(String, i64)]) {
    let url = normalized(base_url);
    let mut memory = MEMORY.lock().unwrap();
    for (id, tokens) in entries {
        memory.provider.insert(format!("{url}|{}", id.to_lowercase()), *tokens);
    }
}

/// 模型的上下文上限：来源按可信程度——本地大模型的、请求超长时报的；服务商在模型列表里报的；都没有时按设置里的最大上下文长度
/// （fallback，guessed = true）
pub fn limit(engine_id: &str, base_url: &str, model: &str, fallback: i64) -> (i64, bool) {
    if let Some(known) = known_limit(&format!("{engine_id}|{model}")) {
        return (known, false);
    }
    if let Some(reported) = provider_limit(base_url, model) {
        return (reported, false);
    }
    (fallback, true)
}

/// 测试用：fit 忘掉记住的上限和校准，provider 忘掉服务商报的
pub fn reset(fit: bool, provider: bool) {
    let mut memory = MEMORY.lock().unwrap();
    if fit {
        memory.limits.clear();
        memory.ratios.clear();
    }
    if provider {
        memory.provider.clear();
    }
}

#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets() {
        assert_eq!(reserve(16_384), 2_048);
        assert_eq!(reserve(131_072), 16_384);
        assert_eq!(reserve(3_000), 1_500);
        assert_eq!(threshold(16_384), 14_336);
        assert_eq!(target(16_384), 10_752);
    }

    /// 去掉的地方留的说明要用提示词：装一份只有这一段的（== prompts-framework.md 里的原文）
    fn prompts_installed() -> std::sync::MutexGuard<'static, ()> {
        let lock = prompts::TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = "@@ context-fit.omitted\n（为放进模型的上下文，这里省略了较早的 {{count}} 条消息（对话和工具记录）；需要时重新调用工具查看。）\n";
        prompts::setup(std::iter::once(("prompts-framework.md".to_string(), text.to_string())).collect(), None, Vec::new());
        lock
    }

    fn message(role: &str, content: &str) -> Value {
        json!({ "role": role, "content": content })
    }

    /// == Swift 自检「按上下文上限压历史」、Qt TestEngines::contextFit
    #[test]
    fn compacts() {
        let _prompts = prompts_installed();
        let long = "工具输出".repeat(400);
        let mut history = vec![
            message("user", "第一个问题"),
            json!({ "role": "assistant", "content": "", "reasoning_content": "想一想",
                    "tool_calls": [{ "id": "a", "type": "function", "function": { "name": "read_file", "arguments": "{}" } }] }),
            json!({ "role": "tool", "tool_call_id": "a", "name": "read_file", "content": long }),
            message("assistant", "回答一"),
            message("user", "第二个问题"),
            message("assistant", "最新的回答"),
        ];
        let mut folded = 3;
        // 预算够：不动
        let mut same = history.clone();
        let result = compact(&mut same, &mut folded, 100_000, None);
        assert_eq!((result.shortened, result.dropped), (0, 0));
        assert_eq!(same, history);
        // 摘要就够：工具结果换成摘要、思考去掉
        let budget = folding::estimated_tokens(&history) - 200;
        let result = compact(&mut history, &mut folded, budget, None);
        assert_eq!((result.shortened, result.dropped), (2, 0));
        assert!(history[2]["content"].as_str().unwrap().contains("较早的工具结果，已省略，原 1600 字"));
        assert!(history[1].get("reasoning_content").is_none());
        assert!(result.after < result.before);
        // 还不够：从最早的整段去掉（用户消息、助手消息连同工具结果），说明放在后面的用户消息前面；最后一条用户消息、最新一步不动
        let result = compact(&mut history, &mut folded, 1, None);
        assert_eq!(result.dropped, 4);
        assert_eq!(history.len(), 2);
        let first = history[0]["content"].as_str().unwrap();
        assert!(first.contains("省略了较早的 4 条消息") && first.ends_with("\n\n第二个问题"), "{first}");
        assert_eq!(history[1], message("assistant", "最新的回答"));
        assert_eq!(folded, 0);
        // 系统消息不去掉；前面留下的不是用户消息、后面是：说明放在后面那条前面
        let mut history = vec![
            message("system", "规则"),
            message("user", "问"),
            message("assistant", &"长".repeat(2_000)),
            message("user", "再问"),
            message("assistant", "答"),
        ];
        let mut folded = 5;
        let result = compact(&mut history, &mut folded, 1, None);
        assert_eq!((result.shortened, result.dropped), (1, 2));
        assert_eq!(history.len(), 3);
        assert_eq!(history[0], message("system", "规则"));
        let second = history[1]["content"].as_str().unwrap();
        assert!(second.contains("省略了较早的 2 条消息") && second.ends_with("\n\n再问"));
        assert_eq!(folded, 3);
        // 前面留下的是用户消息（最后一条用户消息不动）：说明接在它后面
        let mut history = vec![
            message("user", "问"),
            json!({ "role": "assistant", "content": "", "tool_calls": [{ "id": "a", "function": { "name": "x", "arguments": "{}" } }] }),
            json!({ "role": "tool", "tool_call_id": "a", "content": "结果".repeat(1_000) }),
            message("assistant", "答"),
        ];
        let mut folded = 3;
        assert_eq!(compact(&mut history, &mut folded, 1, None).dropped, 2);
        assert_eq!(history.len(), 2);
        assert!(history[0]["content"].as_str().unwrap().starts_with("问\n\n（为放进模型的上下文"));
        assert_eq!(folded, 1);
    }

    #[test]
    fn notes_and_images() {
        let _prompts = prompts_installed();
        // 前面没有留下的用户消息：说明放在后面的用户消息前面
        let mut history = vec![message("assistant", &"长".repeat(2_000)), message("user", "问题"), message("assistant", "答")];
        let mut folded = 0;
        compact(&mut history, &mut folded, 1, None);
        assert_eq!(history.len(), 2);
        assert!(history[0]["content"].as_str().unwrap().ends_with("\n\n问题"));
        // 较早的图片换成说明；会话折叠的设置：子智能体的结果不截、思考留着
        let image = json!({ "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } });
        let user = json!({ "role": "user", "content": [image, { "type": "text", "text": "看图" }] });
        let summaries =
            Summaries { echoes_reasoning: true, special: Special { task_state: String::new(), whole: vec!["delegate_task".into()] } };
        let shorter = summarized(&user, Some(&summaries));
        assert_eq!(shorter["content"][1]["text"], "（这条消息附的 1 张图片已省略，以节省上下文）");
        let reply = json!({ "role": "assistant", "content": "x", "reasoning_content": "想" });
        assert_eq!(summarized(&reply, Some(&summaries))["reasoning_content"], "想");
        let whole = json!({ "role": "tool", "name": "delegate_task", "content": "结".repeat(3_000) });
        assert_eq!(summarized(&whole, Some(&summaries)), whole);
    }

    /// == Swift 自检「上下文进度环」、Qt TestEngines::contextUsage
    #[test]
    fn parts_and_numbers() {
        let history = vec![
            message("user", &"a".repeat(35)),
            json!({ "role": "user", "content": [{ "type": "image_url" }, { "type": "text", "text": "b".repeat(7) }] }),
            json!({ "role": "assistant", "content": "c".repeat(70), "reasoning_content": "d".repeat(35),
                    "tool_calls": [{ "function": { "name": "abc", "arguments": "e".repeat(32) } }] }),
            message("tool", &"f".repeat(350)),
        ];
        let parts = Parts::estimate(&"s".repeat(350), 700, &history);
        assert_eq!(parts, Parts { system: 100, tools: 200, user: 297, replies: 30, tool_use: 110 });
        let scaled = parts.scaled(1_000);
        assert_eq!(scaled.total(), 1_000);
        assert_eq!(Parts::default().scaled(10), Parts::default());
        assert_eq!(Parts::from_json(parts.to_json().as_object().unwrap()), parts);
        for (tokens, text) in [(1_234, "1.2K"), (131_072, "131K"), (1_000_000, "1M"), (48_000, "48K"), (512, "512"), (2_500_000, "2.5M")] {
            assert_eq!(compact_number(tokens), text);
        }
    }

    #[test]
    fn limits() {
        let _lock = TEST_LOCK.lock().unwrap();
        reset(true, true);
        assert_eq!(limit("e", "https://Example.com/v1/", "M", 1_000_000), (1_000_000, true));
        remember_provider("https://example.com/v1", &[("m".into(), 131_072)]);
        assert_eq!(limit("e", "https://Example.com/v1/", " M ", 1_000_000), (131_072, false));
        remember_limit("e|M", 16_384);
        remember_limit("e|M", 0);
        assert_eq!(limit("e", "https://example.com/v1", "M", 1_000_000), (16_384, false));
        assert_eq!(ratio("e|M"), 1.0);
        remember_ratio("e|M", 9.0);
        assert_eq!(ratio("e|M"), 4.0);
        reset(true, false);
        assert_eq!((known_limit("e|M"), ratio("e|M")), (None, 1.0));
        assert_eq!(provider_limit("https://example.com/v1", "m"), Some(131_072));
        reset(false, true);
        assert_eq!(provider_limit("https://example.com/v1", "m"), None);
    }

    /// v2.86 按上限压时调用方记的几个位置一起挪（这一轮从哪条起、折到哪；== Swift / Qt 的 marks 用例）
    #[test]
    fn compact_moves_marks() {
        let mut history: Vec<Value> = (0..10)
            .flat_map(|n| {
                [json!({ "role": "user", "content": format!("问 {n}") }), json!({ "role": "assistant", "content": "答".repeat(3_000) })]
            })
            .collect();
        let mut marks = [14, 12];
        let result = compact_marks(&mut history, &mut marks, 8_000, None);
        assert!(result.dropped > 0);
        assert!(marks[1] < 12 && marks[0] - marks[1] == 2, "{marks:?}");
        assert_eq!(history[marks[0]]["content"], "问 7");
    }
}
