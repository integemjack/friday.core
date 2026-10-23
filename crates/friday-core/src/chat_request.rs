//! 在线大脑（OpenAI 兼容 chat/completions）的请求一侧（== Swift `OpenAIChatEngine` 的 requestPass / streamPass、Qt 的
//! requestBody / transport）：拼请求体、接口说不接受的可选参数（认报错、按引擎记住）、重试和限流的等待、上下文超长的报错。
//! 发请求、读流、闸门、退避的计时留在两端（URLSession / Qt 走系统代理）。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::{engines, secret_guard};

/// v1.20：接口说不接受时可以去掉的：都是 Friday 自己加的可选参数，去掉后按模型的默认值
pub const DROPPABLE: [&str; 11] = [
    "temperature",
    "top_p",
    "max_tokens",
    "stream_options",
    "reasoning_effort",
    "enable_thinking",
    "thinking",
    "parallel_tool_calls",
    "presence_penalty",
    "frequency_penalty",
    "seed",
];

/// v2.76 思考参数（ThinkingSupport 的几种开关）
pub const THINKING: [&str; 3] = ["reasoning_effort", "enable_thinking", "thinking"];

/// 一累计被限流这么久（秒）就放弃这次请求，当成额度用完
pub const RATE_LIMIT_GIVE_UP: f64 = 600.0;

/// 拼请求体要的东西（== 两端引擎上的设置）
#[derive(Debug, Default)]
pub struct BodyOptions<'a> {
    pub model: String,
    /// 系统提示（空的不带）
    pub instructions: String,
    /// OpenAI 格式的历史（不含 system）
    pub history: &'a [Value],
    pub temperature: f64,
    /// 个别请求要限长才给（在线引擎不传设置里的「最大输出」）
    pub max_tokens: Option<i64>,
    /// 要带的工具定义（收尾请求、不支持工具的引擎两端传空）
    pub tools: &'a [Value],
    pub base_url: String,
    /// 思考方式（ThinkingSupport 的原始值：none / toggle / thinkingType / effort；用户设了的按用户的）
    pub thinking: String,
    pub enable_thinking: bool,
    pub reasoning_effort: Option<String>,
    /// 高级设置里的「额外请求参数」（JSON 对象的文本，盖掉同名的）
    pub extra_body: String,
    /// 引擎 id：它说过不接受的可选参数不再带
    pub engine_id: String,
    /// v2.7 本地大模型的调度提示（power://，两端按模型文件、会话、优先级拼好）
    pub x_friday: Option<Value>,
}

/// 拼一段请求的请求体
pub fn request_body(options: &BodyOptions) -> Value {
    let mut messages = Vec::with_capacity(options.history.len() + 1);
    if !options.instructions.is_empty() {
        messages.push(json!({ "role": "system", "content": options.instructions }));
    }
    messages.extend(options.history.iter().cloned());
    let mut body = Map::new();
    body.insert("model".into(), json!(options.model));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), json!(true));
    body.insert("stream_options".into(), json!({ "include_usage": true }));
    body.insert("temperature".into(), json!(options.temperature));
    if let Some(max_tokens) = options.max_tokens {
        body.insert("max_tokens".into(), json!(max_tokens));
    }
    if !options.tools.is_empty() {
        body.insert("tools".into(), Value::Array(options.tools.to_vec()));
        if engines::asks_parallel_tool_calls(&options.base_url) {
            body.insert("parallel_tool_calls".into(), json!(true));
        }
    }
    // v2.76 分档时接口自己的开关也带上（智谱、方舟的 thinking.type，百炼、硅基流动的 enable_thinking）
    let effort = options.thinking == "effort";
    let switch_style = if effort { engines::native_switch(&options.base_url) } else { Some(options.thinking.as_str()) };
    if effort
        && options.enable_thinking
        && let Some(reasoning_effort) = &options.reasoning_effort
    {
        body.insert("reasoning_effort".into(), json!(reasoning_effort));
    }
    match switch_style {
        Some("toggle") => {
            body.insert("enable_thinking".into(), json!(options.enable_thinking));
        }
        Some("thinkingType") => {
            body.insert("thinking".into(), json!({ "type": if options.enable_thinking { "enabled" } else { "disabled" } }));
        }
        _ => {}
    }
    if let Ok(Value::Object(extra)) = serde_json::from_str::<Value>(&options.extra_body) {
        body.extend(extra);
    }
    // v1.20：这个引擎说过不接受的可选参数不再带
    for name in unsupported_for(&options.engine_id) {
        body.remove(&name);
    }
    if let Some(x_friday) = &options.x_friday {
        body.insert("x_friday".into(), x_friday.clone());
    }
    Value::Object(body)
}

/// 发出去之前按密钥库里的值最后遮一遍（入口处都换过了，这里拦漏网的：旧对话里的、换名字之前存下的）
pub fn masked(body: Value) -> Value {
    secret_guard::shared().mask_json(body)
}

/// v1.20：400 / 422 的报错说这个模型不接受哪些可选参数（通义千问 auto 不认 temperature、OpenAI 推理模型不认 temperature /
/// max_tokens、Azure 不认 stream_options…）。只认 DROPPABLE，model、messages、tools、stream 不算；没有就返回空
pub fn unsupported_parameters(status: i64, text: &str) -> Vec<String> {
    if status != 400 && status != 422 {
        return Vec::new();
    }
    // 通义千问：「auto does not support parameter(s): temperature; please remove them」；
    // OpenAI：「Unsupported parameter: 'temperature' is not supported with this model.」「Unsupported value: 'temperature' does not support 0.6…」；
    // Azure：「Unrecognized request argument supplied: stream_options」；
    // v2.8 Kimi k2.6 / k2.7-code：「invalid temperature: only 1 is allowed for this model」（去掉了按模型默认值）
    static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        [
            r"(?i)does not support parameter\(s\):\s*([A-Za-z_][\w, ]*)",
            r"(?i)unsupported (?:parameter|value):\s*'([\w.]+)'",
            r"(?i)unrecognized request arguments? supplied:\s*([A-Za-z_][\w, ]*)",
            r"(?i)invalid ([A-Za-z_]+):\s*only\b",
        ]
        .iter()
        .map(|pattern| Regex::new(pattern).unwrap())
        .collect()
    });
    let mut names: Vec<String> = Vec::new();
    for pattern in PATTERNS.iter() {
        for captures in pattern.captures_iter(text) {
            for name in captures[1].split(|c: char| c == ',' || c.is_whitespace()).filter(|name| !name.is_empty()) {
                if DROPPABLE.contains(&name) && !names.iter().any(|known| known == name) {
                    names.push(name.to_string());
                }
            }
        }
    }
    // v2.76 在线模型默认分档后，没验证过的服务也会收到思考参数：说法各家不一（Mistral「extra_forbidden … reasoning_effort」、
    // Groq「`reasoning_effort` is not supported with this model」），报错里点了名又是在拒绝的就去掉
    if names.is_empty() {
        let lowered = text.to_lowercase();
        let refusals =
            ["not support", "unsupported", "not permitted", "extra_forbidden", "not allowed", "unrecognized", "unknown", "invalid"];
        if refusals.iter().any(|word| lowered.contains(word)) {
            names.extend(THINKING.iter().filter(|name| mentions(&lowered, name)).map(|name| name.to_string()));
        }
    }
    names
}

/// 单独点了这个名字（== `(?<![\w.])name(?![\w])`：前面不是字或点，后面不是字）
fn mentions(text: &str, name: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(name).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + name.len()..].chars().next();
        !before.is_some_and(|c| word(c) || c == '.') && !after.is_some_and(word)
    })
}

static UNSUPPORTED: LazyLock<Mutex<HashMap<String, Vec<String>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// 这个引擎说过不接受的可选参数（本次运行内记住；之后的请求、新派的子智能体都不再带）
pub fn unsupported_for(engine_id: &str) -> Vec<String> {
    UNSUPPORTED.lock().unwrap().get(engine_id).cloned().unwrap_or_default()
}

pub fn remember_unsupported(engine_id: &str, names: &[String]) {
    let mut store = UNSUPPORTED.lock().unwrap();
    let known = store.entry(engine_id.to_string()).or_default();
    for name in names {
        if !known.contains(name) {
            known.push(name.clone());
        }
    }
}

/// 服务繁忙、连不上时第 attempt 次（1 起）重试前等多久（秒）：Retry-After 能按秒数解析就用它（最多等 60 秒），
/// 否则 2、4、8、16 秒，之后每次 30 秒
pub fn retry_delay(attempt: i64, retry_after: Option<f64>) -> f64 {
    retry_after.map_or_else(|| 30f64.min(2f64.powi(attempt.min(5) as i32)), |seconds| seconds.clamp(0.0, 60.0))
}

/// 可重试的 429 的等待（秒）：服务端说了多久就按它，否则 2、4、8、16、30、30… 秒另加抖动（jitter_ms：0–999，测试传 0）
pub fn rate_limit_delay(rate_limits: i64, retry_after: Option<f64>, jitter_ms: i64) -> f64 {
    retry_after.map_or_else(|| 30f64.min(2f64.powi(rate_limits.min(5) as i32)) + jitter_ms as f64 / 1000.0, |seconds| seconds.max(0.0))
}

/// Retry-After 头按秒数解析（HTTP 日期格式的解析不了，按没给）
pub fn header_seconds(value: &str) -> Option<f64> {
    value.trim().parse::<f64>().ok().filter(|seconds| seconds.is_finite())
}

/// v2.11 看 HTTP 错误的状态码与正文是不是「上下文超长」；是的话返回（这次请求的 token 数，上限），拿不到的是 0
pub fn context_overflow(status: i64, body: &str) -> Option<(i64, i64)> {
    if ![400, 413, 422].contains(&status) {
        return None;
    }
    let object: Option<Value> = serde_json::from_str(body).ok();
    let error = object.as_ref().and_then(|object| object.get("error")).and_then(Value::as_object);
    let field = |key: &str| error.and_then(|error| error.get(key)).and_then(Value::as_str).unwrap_or("");
    let (kind, code) = (field("type"), field("code"));
    let message = match error {
        Some(error) => error.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
        None => body.chars().take(2_000).collect(),
    };
    // llama-server / friday-llm：{"error":{"code":400,"message":"request (16620 tokens) exceeds the available context size (16384 tokens), …",
    //                                     "type":"exceed_context_size_error","n_prompt_tokens":16620,"n_ctx":16384}}
    if kind == "exceed_context_size_error" {
        let number = |key: &str| error.and_then(|error| error.get(key)).and_then(Value::as_i64).unwrap_or(0);
        return Some((number("n_prompt_tokens"), number("n_ctx")));
    }
    // OpenAI 兼容服务：code context_length_exceeded，或常见的报错原文
    static WORDING: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)maximum context length|context[_ ]length[_ ]exceeded|exceeds? the (available |maximum )?context|context window|prompt is too long|input is too long|too many tokens").unwrap()
    });
    if code != "context_length_exceeded" && kind != "context_length_exceeded" && !WORDING.is_match(&message) {
        return None;
    }
    // 「maximum context length is 128000 tokens. However, your messages resulted in 130000 tokens」
    static REQUESTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)resulted in (\d+)|requested (\d+)").unwrap());
    static LIMIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)maximum context length is (\d+)").unwrap());
    let number = |pattern: &Regex| {
        pattern
            .captures(&message)
            .and_then(|captures| captures.iter().skip(1).flatten().find_map(|group| group.as_str().parse::<i64>().ok()))
            .unwrap_or(0)
    };
    Some((number(&REQUESTED), number(&LIMIT)))
}

#[cfg(test)]
mod tests {
    use super::*;

    static HISTORY: LazyLock<Vec<Value>> = LazyLock::new(|| vec![json!({ "role": "user", "content": "你好" })]);
    static TOOLS: LazyLock<Vec<Value>> = LazyLock::new(|| vec![json!({ "type": "function", "function": { "name": "get_time" } })]);

    fn options() -> BodyOptions<'static> {
        BodyOptions {
            model: "test-model".into(),
            instructions: "你是 Friday。".into(),
            history: &HISTORY,
            temperature: 0.6,
            base_url: "https://example.com/v1".into(),
            thinking: "toggle".into(),
            enable_thinking: true,
            engine_id: "test-body".into(),
            ..BodyOptions::default()
        }
    }

    /// == Qt TestEngines::requestBodyThinking / parallelToolCalls、Swift 自检的请求体
    #[test]
    fn bodies() {
        let mut toggle = options();
        toggle.tools = &TOOLS;
        let body = request_body(&toggle);
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["temperature"], 0.6);
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["enable_thinking"], true);
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
        assert!(body.get("parallel_tool_calls").is_none());
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        assert_eq!(body["messages"][0], json!({ "role": "system", "content": "你是 Friday。" }));
        toggle.max_tokens = Some(200);
        assert_eq!(request_body(&toggle)["max_tokens"], 200);

        let mut effort = options();
        effort.thinking = "effort".into();
        effort.instructions = String::new();
        effort.history = &[];
        effort.reasoning_effort = Some("high".into());
        let body = request_body(&effort);
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("enable_thinking").is_none() && body.get("tools").is_none());
        assert_eq!(body["messages"], json!([]));
        effort.enable_thinking = false;
        assert!(request_body(&effort).get("reasoning_effort").is_none());

        let mut zhipu = options();
        zhipu.base_url = "https://open.bigmodel.cn/api/coding/paas/v4".into();
        zhipu.thinking = "effort".into();
        zhipu.reasoning_effort = Some("low".into());
        assert_eq!(request_body(&zhipu)["thinking"], json!({ "type": "enabled" }));
        zhipu.enable_thinking = false;
        let body = request_body(&zhipu);
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["thinking"], json!({ "type": "disabled" }));

        let mut qwen = options();
        qwen.base_url = "https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1".into();
        qwen.thinking = "effort".into();
        qwen.reasoning_effort = Some("high".into());
        qwen.tools = &TOOLS;
        let body = request_body(&qwen);
        assert_eq!((body["reasoning_effort"].clone(), body["enable_thinking"].clone()), (json!("high"), json!(true)));
        assert!(body.get("thinking").is_none());
        assert_eq!(body["parallel_tool_calls"], true);

        let mut none = options();
        none.thinking = "none".into();
        none.extra_body = r#"{"top_p": 0.9, "temperature": 0.1}"#.into();
        let body = request_body(&none);
        assert!(["enable_thinking", "thinking", "reasoning_effort"].iter().all(|key| body.get(key).is_none()));
        assert_eq!((body["top_p"].clone(), body["temperature"].clone()), (json!(0.9), json!(0.1)));

        // 记住了不接受的就不带；本地大模型的调度提示原样带上
        let mut remembered = options();
        remembered.engine_id = "test-body-noparam".into();
        remembered.x_friday = Some(json!({ "priority": 2 }));
        remember_unsupported("test-body-noparam", &["temperature".to_string(), "enable_thinking".to_string()]);
        let body = request_body(&remembered);
        assert!(body.get("temperature").is_none() && body.get("enable_thinking").is_none());
        assert_eq!(body["x_friday"], json!({ "priority": 2 }));
        assert_eq!(unsupported_for("test-body-noparam"), ["temperature", "enable_thinking"]);
        assert!(unsupported_for("test-body").is_empty());
    }

    /// == Swift 自检「模型不接受的可选参数」「思考参数被拒」、Qt TestEngines::unsupportedParameters
    #[test]
    fn unsupported() {
        let names = |status: i64, text: &str| unsupported_parameters(status, text);
        assert_eq!(names(400, "auto does not support parameter(s): temperature; please remove them"), ["temperature"]);
        assert_eq!(names(400, "Unsupported parameter: 'max_tokens' is not supported with this model."), ["max_tokens"]);
        assert_eq!(names(400, "Unsupported value: 'temperature' does not support 0.6 with this model."), ["temperature"]);
        assert_eq!(names(400, "Unrecognized request argument supplied: stream_options"), ["stream_options"]);
        assert_eq!(names(400, "invalid temperature: only 1 is allowed for this model"), ["temperature"]);
        assert_eq!(names(400, "deepseek-v4 does not support parameter(s): parallel_tool_calls"), ["parallel_tool_calls"]);
        assert_eq!(names(400, "x does not support parameter(s): temperature, top_p"), ["temperature", "top_p"]);
        assert!(names(400, "does not support parameter(s): messages, tools").is_empty());
        assert!(names(500, "does not support parameter(s): temperature").is_empty());
        assert_eq!(
            names(
                422,
                r#"{"detail":[{"type":"extra_forbidden","loc":["body","reasoning_effort"],"msg":"Extra inputs are not permitted"}]}"#
            ),
            ["reasoning_effort"]
        );
        assert_eq!(names(400, "`reasoning_effort` is not supported with this model"), ["reasoning_effort"]);
        assert_eq!(names(400, "Unknown parameter: thinking"), ["thinking"]);
        assert!(names(400, "reasoning_effort must be low, medium or high").is_empty());
        assert!(names(400, "Invalid API key").is_empty());
        // 名字是别的词的一部分不算
        assert!(names(400, "unknown parameter: thinking_budget").is_empty());
        assert!(names(400, "unknown field: x.thinking").is_empty());
    }

    #[test]
    fn delays() {
        let retries: Vec<f64> = (1..=10).map(|attempt| retry_delay(attempt, None)).collect();
        assert_eq!(retries, [2.0, 4.0, 8.0, 16.0, 30.0, 30.0, 30.0, 30.0, 30.0, 30.0]);
        assert_eq!(retry_delay(3, Some(5.0)), 5.0);
        assert_eq!(retry_delay(1, Some(3600.0)), 60.0);
        assert_eq!(retry_delay(1, Some(-3.0)), 0.0);
        let limits: Vec<f64> = (1..=6).map(|attempt| rate_limit_delay(attempt, None, 0)).collect();
        assert_eq!(limits, [2.0, 4.0, 8.0, 16.0, 30.0, 30.0]);
        assert_eq!(rate_limit_delay(1, None, 500), 2.5);
        assert_eq!(rate_limit_delay(3, Some(5.0), 500), 5.0);
        assert_eq!(rate_limit_delay(3, Some(900.0), 0), 900.0);
        assert_eq!(header_seconds(" 7 "), Some(7.0));
        assert_eq!(header_seconds("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(header_seconds(""), None);
    }

    #[test]
    fn overflow() {
        let llama = r#"{"error":{"code":400,"message":"request (16620 tokens) exceeds the available context size (16384 tokens), try increasing it","type":"exceed_context_size_error","n_prompt_tokens":16620,"n_ctx":16384}}"#;
        assert_eq!(context_overflow(400, llama), Some((16_620, 16_384)));
        let openai = r#"{"error":{"message":"This model's maximum context length is 128000 tokens. However, your messages resulted in 130500 tokens.","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
        assert_eq!(context_overflow(400, openai), Some((130_500, 128_000)));
        assert_eq!(context_overflow(400, r#"{"error":{"message":"bad"}}"#), None);
        assert_eq!(context_overflow(500, llama), None);
        assert_eq!(context_overflow(413, "Prompt is too long"), Some((0, 0)));
        assert_eq!(context_overflow(400, r#"{"error":{"message":"You requested 9000 tokens, too many tokens"}}"#), Some((9_000, 0)));
    }
}
