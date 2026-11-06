//! OpenAI Responses 接口（`/v1/responses`）。GPT-5.4 往后的模型在 chat/completions 上不让工具和思考同时开（报错叫改用 /v1/responses），
//! 所以历史、请求体照旧按 chat/completions 的写法存和拼，发之前这里换一遍；流里的事件在 `chat_stream` 翻回 chat/completions 的 chunk。
//!
//! 什么时候走它：接口报错点名要 `/v1/responses` 时，`chat_request::refused` 记在这个引擎上（本次运行内），两端换地址重发。
//! 不存服务端（`store: false`）、历史每次整份发，和 chat/completions 一样无状态。思考要带回才能接着想：服务端给的加密思考
//! （`reasoning.encrypted_content`）按工具调用 id 记在这里，换请求体时插回对应的调用前面；存档里没有它，重启后的旧调用就不带。

use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};

use serde_json::{Map, Value, json};

use crate::chat_request;

/// call_id 的长度上限（接口的限制；别家模型留在历史里的 id 可能更长）
const CALL_ID_LIMIT: usize = 64;

/// 最多记这么多个工具调用的思考：一个几 KB 到几十 KB，更早的步骤多半已经折叠掉了
const KEPT_LIMIT: usize = 128;

#[derive(Default)]
struct Kept {
    items: HashMap<String, Vec<Value>>,
    order: VecDeque<String>,
}

static KEPT: LazyLock<Mutex<Kept>> = LazyLock::new(|| Mutex::new(Kept::default()));

fn kept_key(engine_id: &str, call_id: &str) -> String {
    format!("{engine_id}\n{call_id}")
}

/// 记下这个工具调用前面的加密思考。按引擎记：换了模型解不开，不带回去
pub fn keep_reasoning(engine_id: &str, call_id: &str, items: Vec<Value>) {
    if items.is_empty() {
        return;
    }
    let key = kept_key(engine_id, call_id);
    let mut kept = KEPT.lock().unwrap();
    if kept.items.insert(key.clone(), items).is_none() {
        kept.order.push_back(key);
    }
    while kept.order.len() > KEPT_LIMIT {
        if let Some(oldest) = kept.order.pop_front() {
            kept.items.remove(&oldest);
        }
    }
}

fn kept_reasoning(engine_id: &str, call_id: &str) -> Vec<Value> {
    KEPT.lock().unwrap().items.get(&kept_key(engine_id, call_id)).cloned().unwrap_or_default()
}

/// 流里的 reasoning 项里要留下带回的部分（没有加密内容的带回去也没用：store 是 false，服务端查不到）
pub fn reasoning_to_keep(item: &Map<String, Value>) -> Option<Value> {
    let encrypted = item.get("encrypted_content").and_then(Value::as_str).filter(|text| !text.is_empty())?;
    Some(json!({ "type": "reasoning", "summary": item.get("summary").cloned().unwrap_or(json!([])), "encrypted_content": encrypted }))
}

/// 这是不是 Responses 流里的事件；是的话返回事件名。「error」也是事件名，chat/completions 的 chunk 没有 type
pub fn event_type(chunk: &Value) -> Option<&str> {
    chunk.get("type").and_then(Value::as_str).filter(|kind| kind.starts_with("response.") || *kind == "error")
}

/// 把 chat/completions 的请求体换成 Responses 的
pub fn body(chat: Value, engine_id: &str) -> Value {
    let Value::Object(mut body) = chat else { return chat };
    let unsupported = chat_request::unsupported_for(engine_id);
    let keeps_reasoning = !unsupported.iter().any(|name| name == "include" || name == "store");
    let messages = match body.remove("messages") {
        Some(Value::Array(messages)) => messages,
        _ => Vec::new(),
    };
    for name in ["stream_options", "thinking", "enable_thinking"] {
        body.remove(name);
    }
    if let Some(limit) = body.remove("max_tokens") {
        body.insert("max_output_tokens".into(), limit);
    }
    if let Some(effort) = body.remove("reasoning_effort") {
        // 要摘要才有思考过程可看（原文不给）；none 不思考，没有摘要
        let reasoning = if effort == "none" { json!({ "effort": effort }) } else { json!({ "effort": effort, "summary": "auto" }) };
        body.insert("reasoning".into(), reasoning);
    }
    if let Some(Value::Array(tools)) = body.get_mut("tools") {
        for tool in tools {
            *tool = flat_tool(std::mem::take(tool));
        }
    }
    body.insert("store".into(), json!(false));
    if keeps_reasoning && body.contains_key("reasoning") {
        body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    }
    let (instructions, input) = input_items(messages, engine_id, keeps_reasoning);
    if let Some(instructions) = instructions {
        body.insert("instructions".into(), instructions);
    }
    body.insert("input".into(), Value::Array(input));
    let mut body = Value::Object(body);
    for name in unsupported {
        remove_path(&mut body, &name);
    }
    body
}

/// 去掉一个参数（`reasoning.summary` 这样的是里层的）；返回去掉了没有
pub fn remove_path(body: &mut Value, path: &str) -> bool {
    match path.split_once('.') {
        Some((outer, inner)) => body.get_mut(outer).and_then(Value::as_object_mut).is_some_and(|object| object.remove(inner).is_some()),
        None => body.as_object_mut().is_some_and(|object| object.remove(path).is_some()),
    }
}

/// 去掉带回的加密思考（服务端说解不开）
pub fn remove_reasoning_items(body: &mut Value) {
    if let Some(Value::Array(input)) = body.get_mut("input") {
        input.retain(|item| item.get("type").and_then(Value::as_str) != Some("reasoning"));
    }
}

/// `{type: function, function: {name, description, parameters}}` 拍平。strict 这里默认是开的，Friday 的工具定义不按严格模式写
fn flat_tool(tool: Value) -> Value {
    let Some(function) = tool.get("function").and_then(Value::as_object) else { return tool };
    let mut flat = function.clone();
    flat.insert("type".into(), json!("function"));
    flat.insert("strict".into(), json!(false));
    Value::Object(flat)
}

fn call_id(id: &str) -> String {
    id.chars().take(CALL_ID_LIMIT).collect()
}

/// 消息换成输入项：开头的 system 进 instructions；工具调用、工具结果是独立的项
fn input_items(messages: Vec<Value>, engine_id: &str, keeps_reasoning: bool) -> (Option<Value>, Vec<Value>) {
    let mut instructions = None;
    let mut input = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = message.get("content");
        match role {
            "system" if index == 0 => instructions = content.cloned(),
            "assistant" => {
                let calls = message.get("tool_calls").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
                let first = calls.first().and_then(|call| call.get("id")).and_then(Value::as_str);
                // 思考排在这一步的正文前面，和模型说的时候的顺序一样
                if keeps_reasoning && let Some(first) = first {
                    input.extend(kept_reasoning(engine_id, first));
                }
                if let Some(text) = assistant_text(content) {
                    input.push(json!({ "role": "assistant", "content": text }));
                }
                for (position, call) in calls.iter().enumerate() {
                    let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
                    if keeps_reasoning && position > 0 {
                        input.extend(kept_reasoning(engine_id, id));
                    }
                    let function = call.get("function");
                    let field = |key: &str| function.and_then(|function| function.get(key)).and_then(Value::as_str).unwrap_or_default();
                    input.push(
                        json!({ "type": "function_call", "call_id": call_id(id), "name": field("name"), "arguments": field("arguments") }),
                    );
                }
            }
            "tool" => {
                let id = message.get("tool_call_id").and_then(Value::as_str).unwrap_or_default();
                input.push(json!({ "type": "function_call_output", "call_id": call_id(id), "output": parts(content) }));
            }
            _ => input.push(json!({ "role": role, "content": parts(content) })),
        }
    }
    (instructions, input)
}

fn assistant_text(content: Option<&Value>) -> Option<String> {
    let text = match content? {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts.iter().filter_map(|part| part.get("text").and_then(Value::as_str)).collect(),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

/// 内容：纯文字原样；分段的 text → input_text、image_url → input_image（地址拍平），别的不动
fn parts(content: Option<&Value>) -> Value {
    let Some(Value::Array(parts)) = content else { return content.cloned().unwrap_or(json!("")) };
    Value::Array(
        parts
            .iter()
            .map(|part| match part.get("type").and_then(Value::as_str) {
                Some("text") => json!({ "type": "input_text", "text": part.get("text").cloned().unwrap_or(json!("")) }),
                Some("image_url") => {
                    let url = part.get("image_url").and_then(|image| image.get("url")).cloned().unwrap_or(json!(""));
                    json!({ "type": "input_image", "image_url": url })
                }
                _ => part.clone(),
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat() -> Value {
        json!({
            "model": "gpt-6.1-terra",
            "messages": [
                { "role": "system", "content": "你是 Friday。" },
                { "role": "user", "content": [{ "type": "text", "text": "看图" }, { "type": "image_url", "image_url": { "url": "data:x" } }] },
                { "role": "assistant", "content": "先读", "reasoning_content": "想", "tool_calls": [
                    { "id": "call_a", "type": "function", "function": { "name": "read_file", "arguments": "{\"path\":\"a\"}" } },
                    { "id": "call_b", "type": "function", "function": { "name": "list", "arguments": "{}" } },
                ] },
                { "role": "tool", "tool_call_id": "call_a", "name": "read_file", "content": "内容" },
                { "role": "tool", "tool_call_id": "call_b", "name": "list", "content": "表" },
                { "role": "assistant", "content": "" },
                { "role": "system", "content": "中途的提醒" },
            ],
            "stream": true,
            "stream_options": { "include_usage": true },
            "temperature": 0.6,
            "max_tokens": 200,
            "reasoning_effort": "high",
            "enable_thinking": true,
            "top_p": 0.9,
            "tools": [{ "type": "function", "function": { "name": "read_file", "description": "读", "parameters": { "type": "object" } } }],
        })
    }

    #[test]
    fn converts_request() {
        let body = body(chat(), "t-plain");
        assert_eq!(body["instructions"], "你是 Friday。");
        assert_eq!(body["reasoning"], json!({ "effort": "high", "summary": "auto" }));
        assert_eq!(body["max_output_tokens"], 200);
        assert_eq!((body["store"].clone(), body["include"].clone()), (json!(false), json!(["reasoning.encrypted_content"])));
        assert_eq!(
            body["tools"],
            json!([{ "type": "function", "name": "read_file", "description": "读", "parameters": { "type": "object" }, "strict": false }])
        );
        // 请求自己的、额外请求参数里的照带；chat/completions 专有的不带
        assert_eq!((body["top_p"].clone(), body["stream"].clone(), body["temperature"].clone()), (json!(0.9), json!(true), json!(0.6)));
        for name in ["messages", "stream_options", "max_tokens", "reasoning_effort", "enable_thinking"] {
            assert!(body.get(name).is_none(), "{name}");
        }
        assert_eq!(
            body["input"],
            json!([
                { "role": "user", "content": [{ "type": "input_text", "text": "看图" }, { "type": "input_image", "image_url": "data:x" }] },
                { "role": "assistant", "content": "先读" },
                { "type": "function_call", "call_id": "call_a", "name": "read_file", "arguments": "{\"path\":\"a\"}" },
                { "type": "function_call", "call_id": "call_b", "name": "list", "arguments": "{}" },
                { "type": "function_call_output", "call_id": "call_a", "output": "内容" },
                { "type": "function_call_output", "call_id": "call_b", "output": "表" },
                { "role": "system", "content": "中途的提醒" },
            ])
        );
    }

    #[test]
    fn no_effort_no_reasoning() {
        let mut request = chat();
        request.as_object_mut().unwrap().remove("reasoning_effort");
        let body = body(request, "t-off");
        assert!(body.get("reasoning").is_none() && body.get("include").is_none());
    }

    #[test]
    fn none_has_no_summary() {
        let mut request = chat();
        request["reasoning_effort"] = json!("none");
        assert_eq!(body(request, "t-none")["reasoning"], json!({ "effort": "none" }));
    }

    #[test]
    fn long_call_id_is_cut() {
        let id = "x".repeat(80);
        let request = json!({ "messages": [
            { "role": "assistant", "content": "", "tool_calls": [{ "id": id, "function": { "name": "a", "arguments": "{}" } }] },
            { "role": "tool", "tool_call_id": id, "content": "好" },
        ] });
        let input = &body(request, "t-long")["input"];
        assert_eq!(input[0]["call_id"].as_str().unwrap().len(), CALL_ID_LIMIT);
        assert_eq!(input[0]["call_id"], input[1]["call_id"]);
    }

    #[test]
    fn reasoning_goes_back_in_place() {
        keep_reasoning("t-keep", "call_a", vec![json!({ "type": "reasoning", "summary": [], "encrypted_content": "甲" })]);
        keep_reasoning("t-keep", "call_b", vec![json!({ "type": "reasoning", "summary": [], "encrypted_content": "乙" })]);
        let kinds = |body: &Value| -> Vec<String> {
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["type"].as_str().or(item["role"].as_str()).unwrap().to_string())
                .collect()
        };
        let mut sent = body(chat(), "t-keep");
        assert_eq!(kinds(&sent)[1..6], ["reasoning", "assistant", "function_call", "reasoning", "function_call"].map(String::from),);
        // 换了引擎的解不开，不带
        assert!(!kinds(&body(chat(), "t-other")).contains(&"reasoning".to_string()));
        remove_reasoning_items(&mut sent);
        assert!(!kinds(&sent).contains(&"reasoning".to_string()));
    }

    #[test]
    fn keeps_a_bounded_number() {
        for index in 0..(KEPT_LIMIT + 5) {
            keep_reasoning("t-bound", &format!("c{index}"), vec![json!({ "n": index })]);
        }
        assert!(kept_reasoning("t-bound", "c0").is_empty());
        assert_eq!(kept_reasoning("t-bound", &format!("c{}", KEPT_LIMIT + 4)).len(), 1);
    }

    #[test]
    fn drops_what_the_engine_refused() {
        chat_request::remember_unsupported("t-drop", &["reasoning.summary".into(), "include".into(), "temperature".into()]);
        let body = body(chat(), "t-drop");
        assert_eq!(body["reasoning"], json!({ "effort": "high" }));
        assert!(body.get("include").is_none() && body.get("temperature").is_none());
    }

    #[test]
    fn events() {
        assert_eq!(event_type(&json!({ "type": "response.output_text.delta" })), Some("response.output_text.delta"));
        assert_eq!(event_type(&json!({ "type": "error", "message": "x" })), Some("error"));
        assert_eq!(event_type(&json!({ "type": "message_delta", "choices": [] })), None);
        assert_eq!(event_type(&json!({ "choices": [] })), None);
        assert_eq!(reasoning_to_keep(json!({ "summary": [] }).as_object().unwrap()), None);
        assert_eq!(
            reasoning_to_keep(json!({ "id": "rs_1", "type": "reasoning", "encrypted_content": "密" }).as_object().unwrap()),
            Some(json!({ "type": "reasoning", "summary": [], "encrypted_content": "密" }))
        );
    }
}
