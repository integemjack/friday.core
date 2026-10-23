//! 在线大脑（OpenAI 兼容 chat/completions）的流式解析（== Swift `OpenAIChatEngine.readStream`、Qt `OpenAIChatEngine::processChunk`）。
//!
//! 网络和 SSE 分行留在两端（URLSession / Qt 走系统代理）：两端认出 `data:` 行、`[DONE]`，把每个 chunk 的 JSON 交给这里的句柄，
//! 拿回这一块要交给界面的文字（思考包成 `<think>…</think>`）和该不该停；读完再取整段的结果（正文、思考、拼好的工具调用、用量、
//! 结束原因、复读）。流里的报错（额度用完、限流、其他）也在这里认，两端照原样抛各自的错误。

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};

use crate::quota::{self, Limit};
use crate::repetition::{self, Loop};

/// 一个拼到现在的工具调用
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Call {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// 流里来的报错
#[derive(Debug, Clone, PartialEq)]
pub enum StreamError {
    /// 额度用完、余额不足（先回 200、再在流里报错的服务）：reset 是服务端说的恢复秒数
    Quota { limit: Limit, reset: Option<f64>, message: String },
    /// 限流（friday-llm 的 KV 池满了，把这个请求选作牺牲者）：两端按临时断开重发这一段
    RateLimit { message: String },
    /// 其他报错：原文
    Failed { message: String },
}

/// 喂一个 chunk 的结果
#[derive(Debug, Default, PartialEq)]
pub struct Feed {
    /// 要按顺序交给界面的文字
    pub chunks: Vec<String>,
    /// 这一块里工具调用参数的增量（Qt 拿它让统计栏的 token 数跟着涨）
    pub arguments_delta: String,
    /// 这是第一个带内容（正文 / 思考 / 工具调用）的 chunk
    pub first_token: bool,
    /// 读到复读了，不再往下读
    pub looped: bool,
    pub error: Option<StreamError>,
}

/// 一段请求的解析状态
#[derive(Debug, Default)]
pub struct Parser {
    /// 思考内容要不要交给界面（截断后续写的那一段不显示，免得把正文切碎）
    show_reasoning: bool,
    /// 流里的报错要不要按额度用完认（在线引擎要；Qt 的本地引擎没有额度）
    quota: bool,
    /// 正在输出思考（已经交出了 <think>）
    in_reasoning: bool,
    content: Vec<u16>,
    reasoning: Vec<u16>,
    content_checked: usize,
    reasoning_checked: usize,
    pending: BTreeMap<i64, Call>,
    prompt_tokens: i64,
    completion_tokens: i64,
    cached_tokens: Option<i64>,
    finish_reason: Option<String>,
    first_token: bool,
    found: Option<(Loop, bool)>,
}

impl Parser {
    pub fn new(show_reasoning: bool, quota: bool) -> Self {
        Parser { show_reasoning, quota, ..Parser::default() }
    }

    /// 处理一个 chunk（SSE data: 的负载）：usage → error.message → choices[0] 的 finish_reason 与 delta
    pub fn feed(&mut self, chunk: &Value, now: DateTime<Utc>) -> Feed {
        let mut feed = Feed::default();
        let Some(object) = chunk.as_object() else { return feed };
        if let Some(usage) = object.get("usage").and_then(Value::as_object) {
            if let Some(tokens) = int_at(usage, "prompt_tokens") {
                self.prompt_tokens = tokens;
            }
            if let Some(tokens) = int_at(usage, "completion_tokens") {
                self.completion_tokens = tokens;
            }
            if let Some(cached) = cached_tokens(usage) {
                self.cached_tokens = Some(cached);
            }
        }
        if let Some(message) = object.get("error").and_then(|error| error.get("message")).and_then(Value::as_str) {
            feed.error = Some(self.stream_error(object, message, now));
            return feed;
        }
        let Some(choice) = objects(object.get("choices")).and_then(|choices| choices.first().copied()) else { return feed };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str).filter(|reason| !reason.is_empty()) {
            self.finish_reason = Some(reason.to_string());
        }
        let Some(delta) = choice.get("delta").and_then(Value::as_object) else { return feed };
        let text = |key: &str| delta.get(key).and_then(Value::as_str).filter(|text| !text.is_empty());
        let (reasoning, content) = (text("reasoning_content"), text("content"));
        if !self.first_token && (reasoning.is_some() || content.is_some() || delta.contains_key("tool_calls")) {
            self.first_token = true;
            feed.first_token = true;
        }
        // DeepSeek / Qwen 的思考内容：用 <think> 标签包起来，界面按思考块显示
        if let Some(reasoning) = reasoning {
            self.reasoning.extend(reasoning.encode_utf16());
            if self.show_reasoning {
                if !self.in_reasoning {
                    self.in_reasoning = true;
                    feed.chunks.push("<think>".into());
                }
                feed.chunks.push(reasoning.into());
            }
        }
        if let Some(content) = content {
            if self.in_reasoning {
                self.in_reasoning = false;
                feed.chunks.push("</think>".into());
            }
            self.content.extend(content.encode_utf16());
            feed.chunks.push(content.into());
        }
        // v2.11 复读：思考或正文末尾同一段连着重复，不再往下读——两端截掉重复的部分、提醒模型换做法
        if self.reasoning.len() - self.reasoning_checked >= repetition::CHECK_EVERY {
            self.reasoning_checked = self.reasoning.len();
            if let Some(found) = repetition::trailing_loop(&self.reasoning, repetition::REASONING_PERIOD, repetition::REASONING_COPIES) {
                self.found = Some((found, true));
                feed.looped = true;
                return feed;
            }
        }
        if self.content.len() - self.content_checked >= repetition::CHECK_EVERY {
            self.content_checked = self.content.len();
            if let Some(found) = repetition::trailing_loop(&self.content, repetition::CONTENT_PERIOD, repetition::CONTENT_COPIES) {
                self.found = Some((found, false));
                feed.looped = true;
                return feed;
            }
        }
        if let Some(calls) = objects(delta.get("tool_calls")) {
            for call in calls {
                let index = int_at(call, "index").unwrap_or(self.pending.len() as i64);
                let entry = self.pending.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) {
                    entry.id = id.to_string();
                }
                if let Some(function) = call.get("function").and_then(Value::as_object) {
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        entry.name += name;
                    }
                    if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                        entry.arguments += arguments;
                        feed.arguments_delta += arguments;
                    }
                }
            }
        }
        feed
    }

    fn stream_error(&self, object: &Map<String, Value>, message: &str, now: DateTime<Utc>) -> StreamError {
        // 流里报额度用完：和 HTTP 报错一样，这一轮停下等恢复
        if self.quota {
            let raw = Value::Object(object.clone()).to_string();
            if let Some(limit @ (Limit::Usage | Limit::Balance)) = quota::classify(0, &raw) {
                return StreamError::Quota { limit, reset: quota::reset_delay(|_| None, &raw, now), message: message.to_string() };
            }
        }
        // v2.11 流里报限流：按临时断开重发这一段，闸门减并发
        if object.get("error").and_then(Value::as_object).is_some_and(is_rate_limit) {
            return StreamError::RateLimit { message: message.to_string() };
        }
        StreamError::Failed { message: message.to_string() }
    }

    /// 整段的结果（读完、停下、出错都能取）。复读时读到一半就停了：没写完的工具调用不要
    pub fn result(&self) -> Value {
        let tool_calls: Vec<Value> = if self.found.is_some() {
            Vec::new()
        } else {
            self.pending
                .iter()
                .filter(|(_, call)| !call.name.is_empty())
                .map(|(index, call)| {
                    json!({
                        "id": if call.id.is_empty() { format!("call_{index}") } else { call.id.clone() },
                        "name": call.name,
                        "arguments": if call.arguments.is_empty() { "{}" } else { call.arguments.as_str() },
                    })
                })
                .collect()
        };
        json!({
            "content": String::from_utf16_lossy(&self.content),
            "reasoning": String::from_utf16_lossy(&self.reasoning),
            "toolCalls": tool_calls,
            "promptTokens": self.prompt_tokens,
            "completionTokens": self.completion_tokens,
            "cachedTokens": self.cached_tokens,
            "finishReason": self.finish_reason,
            "loop": self.found.as_ref().map(|(found, in_reasoning)| json!({
                "start": found.start, "unit": found.unit, "inReasoning": in_reasoning,
            })),
            "inReasoning": self.in_reasoning,
        })
    }
}

impl Feed {
    /// C 接口里的写法：空的字段不写（每个 chunk 都过一次接口，越短越好）
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        if !self.chunks.is_empty() {
            out.insert("chunks".into(), json!(self.chunks));
        }
        if !self.arguments_delta.is_empty() {
            out.insert("argumentsDelta".into(), json!(self.arguments_delta));
        }
        if self.first_token {
            out.insert("firstToken".into(), json!(true));
        }
        if self.looped {
            out.insert("loop".into(), json!(true));
        }
        if let Some(error) = &self.error {
            out.insert(
                "error".into(),
                match error {
                    StreamError::Quota { limit, reset, message } => {
                        json!({ "kind": "quota", "limit": limit.name(), "resetSeconds": reset, "message": message })
                    }
                    StreamError::RateLimit { message } => json!({ "kind": "rateLimit", "message": message }),
                    StreamError::Failed { message } => json!({ "kind": "failed", "message": message }),
                },
            );
        }
        Value::Object(out)
    }
}

/// v2.4 usage 里命中服务端缓存的输入 token（含在 prompt_tokens 里）：prompt_tokens_details.cached_tokens（OpenAI、百炼、智谱、
/// OpenRouter）、prompt_cache_hit_tokens（DeepSeek）、cached_tokens（Kimi），按这个顺序取第一个有的
pub fn cached_tokens(usage: &Map<String, Value>) -> Option<i64> {
    usage
        .get("prompt_tokens_details")
        .and_then(Value::as_object)
        .and_then(|details| int_at(details, "cached_tokens"))
        .or_else(|| int_at(usage, "prompt_cache_hit_tokens"))
        .or_else(|| int_at(usage, "cached_tokens"))
}

/// 流里的 error 对象是不是限流：type 含 rate_limit，或 code 是 429（数字或字符串）
pub fn is_rate_limit(error: &Map<String, Value>) -> bool {
    error.get("type").and_then(Value::as_str).is_some_and(|kind| kind.contains("rate_limit"))
        || match error.get("code") {
            Some(Value::String(code)) => code == "429",
            Some(code) => as_int(code) == Some(429),
            None => false,
        }
}

/// v2.69 流读完（200、连接没报错）之后看这一段是不是其实被服务端掐断了：结束原因是 network_error（智谱：推理中途出错），
/// 或者既没有结束原因、也没收到 [DONE]——临时故障（transient，两端按生成到一半断网处理）；sensitive / content_filter（被内容审核拦下）如实报错
pub fn interrupted(finish_reason: Option<&str>, saw_done: bool) -> Option<(bool, String)> {
    match finish_reason.filter(|reason| !reason.is_empty()) {
        Some("network_error") => Some((true, "模型生成到一半中断了（服务端报 network_error）".into())),
        Some(reason @ ("sensitive" | "content_filter")) => Some((false, format!("模型的输出被服务方的内容审核拦下了（{reason}）"))),
        None if !saw_done => Some((true, "模型的回复没收完连接就结束了（没有结束标记）".into())),
        _ => None,
    }
}

/// == Swift `as? Int`：整数（或没有小数部分的浮点数）
fn as_int(value: &Value) -> Option<i64> {
    let number = value.as_number()?;
    number.as_i64().or_else(|| number.as_f64().filter(|value| value.fract() == 0.0).map(|value| value as i64))
}

fn int_at(object: &Map<String, Value>, key: &str) -> Option<i64> {
    object.get(key).and_then(as_int)
}

/// == Swift `as? [[String: Any]]`：数组里全是对象才算
fn objects(value: Option<&Value>) -> Option<Vec<&Map<String, Value>>> {
    value?.as_array()?.iter().map(Value::as_object).collect()
}

// MARK: - 句柄（每段请求一个；各自一把锁，几个子智能体同时读流不互相等）

type Shared = Arc<Mutex<Parser>>;

static PARSERS: LazyLock<Mutex<HashMap<u64, Shared>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT: AtomicU64 = AtomicU64::new(1);

pub fn open(show_reasoning: bool, quota: bool) -> u64 {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    PARSERS.lock().unwrap().insert(id, Arc::new(Mutex::new(Parser::new(show_reasoning, quota))));
    id
}

pub fn close(id: u64) {
    PARSERS.lock().unwrap().remove(&id);
}

pub fn with<T>(id: u64, body: impl FnOnce(&mut Parser) -> T) -> Option<T> {
    let parser = PARSERS.lock().unwrap().get(&id).cloned()?;
    let mut parser = parser.lock().unwrap();
    Some(body(&mut parser))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(parser: &mut Parser, chunk: &str) -> Feed {
        parser.feed(&serde_json::from_str(chunk).unwrap(), Utc::now())
    }

    #[test]
    fn reasoning_content_and_tools() {
        let mut parser = Parser::new(true, true);
        let first = feed(&mut parser, r#"{"choices":[{"delta":{"reasoning_content":"想一想"}}]}"#);
        assert_eq!(first.chunks, ["<think>", "想一想"]);
        assert!(first.first_token);
        let second = feed(&mut parser, r#"{"choices":[{"delta":{"content":"好的"}}]}"#);
        assert_eq!(second.chunks, ["</think>", "好的"]);
        assert!(!second.first_token);
        let calls = feed(
            &mut parser,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"read_","arguments":"{\"pa"}}]}}]}"#,
        );
        assert_eq!(calls.arguments_delta, "{\"pa");
        feed(&mut parser, r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"file","arguments":"th\":1}"}}]}}]}"#);
        feed(&mut parser, r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"name":"get_time"}}]}}]}"#);
        feed(&mut parser, r#"{"choices":[{"delta":{"tool_calls":[{"index":2,"function":{"arguments":"{}"}}]}}]}"#);
        feed(&mut parser, r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#);
        feed(
            &mut parser,
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":80}}}"#,
        );
        let result = parser.result();
        assert_eq!(result["content"], "好的");
        assert_eq!(result["reasoning"], "想一想");
        assert_eq!(
            result["toolCalls"],
            json!([{"id": "call_a", "name": "read_file", "arguments": "{\"path\":1}"}, {"id": "call_1", "name": "get_time", "arguments": "{}"}])
        );
        assert_eq!(
            (result["promptTokens"].clone(), result["completionTokens"].clone(), result["cachedTokens"].clone()),
            (json!(100), json!(7), json!(80))
        );
        assert_eq!(result["finishReason"], "tool_calls");
        assert_eq!(result["loop"], Value::Null);
        assert_eq!(result["inReasoning"], false);
    }

    #[test]
    fn hidden_reasoning_and_odd_chunks() {
        let mut parser = Parser::new(false, true);
        assert_eq!(feed(&mut parser, r#"{"choices":[{"delta":{"reasoning_content":"不显示"}}]}"#).chunks, Vec::<String>::new());
        assert_eq!(feed(&mut parser, r#"{"choices":[{"delta":{"content":"正文"}}]}"#).chunks, ["正文"]);
        // 不是对象、choices 里混了别的、delta 不是对象、content 是 null：都不管
        for chunk in [
            r#"[1]"#,
            r#"{"choices":[1,{"delta":{"content":"x"}}]}"#,
            r#"{"choices":[{"delta":"x"}]}"#,
            r#"{"choices":[{"delta":{"content":null}}]}"#,
        ] {
            assert_eq!(feed(&mut parser, chunk), Feed::default());
        }
        // 没有 index 的工具调用按已有的个数排
        feed(&mut parser, r#"{"choices":[{"delta":{"tool_calls":[{"id":"a","function":{"name":"x"}}]}}]}"#);
        feed(&mut parser, r#"{"choices":[{"delta":{"tool_calls":[{"id":"b","function":{"name":"y"}}]}}]}"#);
        assert_eq!(parser.result()["toolCalls"].as_array().unwrap().len(), 2);
        assert_eq!(parser.result()["reasoning"], "不显示");
        // 用量是浮点数但没有小数也认（== `as? Int`），有小数的不认
        feed(&mut parser, r#"{"usage":{"prompt_tokens":12.0,"completion_tokens":3.5}}"#);
        assert_eq!((parser.result()["promptTokens"].clone(), parser.result()["completionTokens"].clone()), (json!(12), json!(0)));
    }

    #[test]
    fn errors_in_stream() {
        let mut parser = Parser::new(true, true);
        let quota = feed(
            &mut parser,
            r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota"}}"#,
        );
        assert!(matches!(quota.error, Some(StreamError::Quota { .. })), "{quota:?}");
        let limited = feed(&mut parser, r#"{"error":{"message":"KV 池满了","type":"rate_limit_error","code":429}}"#);
        assert_eq!(limited.error, Some(StreamError::RateLimit { message: "KV 池满了".into() }));
        let failed = feed(&mut parser, r#"{"error":{"message":"Context size has been exceeded."}}"#);
        assert_eq!(failed.error, Some(StreamError::Failed { message: "Context size has been exceeded.".into() }));
        // 本地引擎没有额度：额度的报错原文照报
        let mut local = Parser::new(true, false);
        let quota = feed(&mut local, r#"{"error":{"message":"You exceeded your current quota","type":"insufficient_quota"}}"#);
        assert!(matches!(quota.error, Some(StreamError::Failed { .. })));
        // error 不是对象、没有 message：不算报错
        assert_eq!(feed(&mut parser, r#"{"error":"boom"}"#), Feed::default());
    }

    #[test]
    fn stops_on_repetition() {
        let unit = repetition::tests::UNIT;
        let mut parser = Parser::new(true, true);
        let mut looped = false;
        for _ in 0..6 {
            let chunk = json!({ "choices": [{ "delta": { "reasoning_content": unit } }] });
            if parser.feed(&chunk, Utc::now()).looped {
                looped = true;
                break;
            }
        }
        assert!(looped);
        parser.feed(&json!({ "choices": [{ "delta": { "tool_calls": [{ "index": 0, "function": { "name": "x" } }] } }] }), Utc::now());
        let result = parser.result();
        assert_eq!(result["loop"]["unit"], unit);
        assert_eq!(result["loop"]["start"], unit.encode_utf16().count());
        assert_eq!(result["loop"]["inReasoning"], true);
        assert_eq!(result["toolCalls"], json!([]));
        assert_eq!(result["inReasoning"], true);
    }

    #[test]
    fn helpers() {
        let usage = |text: &str| serde_json::from_str::<Map<String, Value>>(text).unwrap();
        assert_eq!(cached_tokens(&usage(r#"{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":80}}"#)), Some(80));
        assert_eq!(cached_tokens(&usage(r#"{"prompt_tokens":100,"prompt_cache_hit_tokens":64}"#)), Some(64));
        assert_eq!(cached_tokens(&usage(r#"{"cached_tokens":50}"#)), Some(50));
        assert_eq!(cached_tokens(&usage(r#"{"prompt_tokens_details":{"audio_tokens":0},"prompt_cache_hit_tokens":7}"#)), Some(7));
        assert_eq!(cached_tokens(&usage(r#"{"prompt_tokens":100,"completion_tokens":5}"#)), None);
        assert!(is_rate_limit(&usage(r#"{"type":"rate_limit_error","code":429}"#)));
        assert!(is_rate_limit(&usage(r#"{"code":"429"}"#)));
        assert!(is_rate_limit(&usage(r#"{"type":"rate_limit_exceeded"}"#)));
        assert!(!is_rate_limit(&usage(r#"{"message":"Context size has been exceeded."}"#)));
        assert_eq!(interrupted(Some("network_error"), true).map(|(transient, _)| transient), Some(true));
        assert_eq!(interrupted(Some("content_filter"), true).map(|(transient, _)| transient), Some(false));
        assert_eq!(interrupted(None, false).map(|(transient, _)| transient), Some(true));
        assert_eq!(interrupted(Some(""), false).map(|(transient, _)| transient), Some(true));
        assert_eq!(interrupted(None, true), None);
        assert_eq!(interrupted(Some("stop"), false), None);
        assert_eq!(interrupted(Some("length"), false), None);
    }
}
