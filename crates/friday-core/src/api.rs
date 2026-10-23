//! 两端调用的统一入口：`call(方法名, JSON 参数) -> JSON`。
//!
//! 结果总是一个对象：成功 `{"result": …}`，失败 `{"error": {"code": "…", "message": "…"}}`。
//! code：`unknownMethod`（没有这个方法）、`invalidParams`（参数不对）、`internal`（核心里出错，C 接口层兜住的 panic 也报这个）。
//! 时间一律用 Unix 毫秒（`now` 不传 = 现在），时长一律用秒（浮点）。
//!
//! 方法按模块分组（`quota.*`、`tools.*`），每个方法的参数和结果写在下面各自的函数上。

use serde_json::{Map, Value, json};

use crate::{quota, tool_aliases};

#[derive(Debug)]
pub struct ApiError {
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    fn invalid(message: impl Into<String>) -> Self {
        ApiError { code: "invalidParams", message: message.into() }
    }
}

type Params = Map<String, Value>;

/// 执行一个方法，返回结果对象的 JSON 文本（见模块说明）
pub fn call(method: &str, params: &str) -> String {
    let outcome = if params.trim().is_empty() {
        Ok(Map::new())
    } else {
        match serde_json::from_str::<Value>(params) {
            Ok(Value::Object(object)) => Ok(object),
            Ok(Value::Null) => Ok(Map::new()),
            Ok(_) => Err(ApiError::invalid("参数要是 JSON 对象")),
            Err(error) => Err(ApiError::invalid(format!("参数不是合法的 JSON：{error}"))),
        }
    }
    .and_then(|params| dispatch(method, &params));
    envelope(outcome).to_string()
}

pub fn envelope(outcome: Result<Value, ApiError>) -> Value {
    match outcome {
        Ok(result) => json!({ "result": result }),
        Err(error) => json!({ "error": { "code": error.code, "message": error.message } }),
    }
}

fn dispatch(method: &str, params: &Params) -> Result<Value, ApiError> {
    match method {
        // {} → {"version": "0.1.0", "abi": 1}
        "core.version" => Ok(json!({ "version": crate::VERSION, "abi": crate::ABI_VERSION })),

        // {status, body} → "rateLimit" | "usage" | "balance" | "denied" | null
        "quota.classify" => {
            Ok(quota::classify(int(params, "status")?, &string(params, "body")?).map_or(Value::Null, |limit| Value::from(limit.name())))
        }
        // {headers: {名字: 值}, body, now?} → 秒 | null（headers 的名字不区分大小写；只看 quota.headerNames 里的和 Retry-After）
        "quota.resetDelay" => {
            let headers = match params.get("headers") {
                None | Some(Value::Null) => Map::new(),
                Some(Value::Object(headers)) => headers.clone(),
                Some(_) => return Err(ApiError::invalid("headers 要是对象")),
            };
            let header = |name: &str| {
                headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).and_then(|(_, value)| value.as_str().map(str::to_string))
            };
            Ok(seconds(quota::reset_delay(header, &string(params, "body")?, now(params)?)))
        }
        // {} → 包装层要取的响应头名字（Retry-After 和 x-ratelimit-reset* 这些）
        "quota.headerNames" => {
            Ok(Value::from(std::iter::once("Retry-After").chain(quota::RESET_HEADERS.iter().copied()).collect::<Vec<_>>()))
        }
        // {raw, now?} → 秒 | null
        "quota.resetValue" => Ok(seconds(quota::reset_value(&string(params, "raw")?, now(params)?))),
        // {body, now?} → 秒 | null
        "quota.bodyDelay" => Ok(seconds(quota::body_delay(&string(params, "body")?, now(params)?))),
        // {text} → 秒 | null
        "quota.duration" => Ok(seconds(quota::duration(&string(params, "text")?))),
        // {kind: "usage" | "balance", attempt} → 秒
        "quota.probeDelay" => {
            let kind = match string(params, "kind")?.as_str() {
                "usage" => quota::WaitKind::Usage,
                "balance" => quota::WaitKind::Balance,
                other => return Err(ApiError::invalid(format!("kind 不认识：{other}"))),
            };
            Ok(Value::from(quota::probe_delay(kind, int(params, "attempt")?)))
        }

        // {name} → 内置工具名（name 是别家的叫法时），否则原样
        "tools.canonicalName" => Ok(Value::from(tool_aliases::canonical_name(&string(params, "name")?))),
        // {name, arguments} → 换过参数名的 arguments | null（没有要换的；name 是已经按注册表换好的工具名）
        "tools.canonicalArguments" => {
            let arguments = match params.get("arguments") {
                None | Some(Value::Null) => Map::new(),
                Some(Value::Object(arguments)) => arguments.clone(),
                Some(_) => return Err(ApiError::invalid("arguments 要是对象")),
            };
            let canonical = tool_aliases::canonical_arguments(&string(params, "name")?, &arguments);
            Ok(if canonical == arguments { Value::Null } else { Value::Object(canonical) })
        }

        _ => Err(ApiError { code: "unknownMethod", message: format!("没有这个方法：{method}") }),
    }
}

fn string(params: &Params, key: &str) -> Result<String, ApiError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(_) => Err(ApiError::invalid(format!("{key} 要是字符串"))),
    }
}

fn int(params: &Params, key: &str) -> Result<i64, ApiError> {
    match params.get(key) {
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_f64().filter(|value| value.fract() == 0.0).map(|value| value as i64))
            .ok_or_else(|| ApiError::invalid(format!("{key} 要是整数"))),
        _ => Err(ApiError::invalid(format!("缺少整数参数 {key}"))),
    }
}

fn now(params: &Params) -> Result<chrono::DateTime<chrono::Utc>, ApiError> {
    match params.get("now") {
        None | Some(Value::Null) => Ok(chrono::Utc::now()),
        Some(value) => value
            .as_f64()
            .and_then(|millis| quota::from_epoch_millis(millis.round() as i64))
            .ok_or_else(|| ApiError::invalid("now 要是 Unix 毫秒")),
    }
}

fn seconds(value: Option<f64>) -> Value {
    value.map_or(Value::Null, Value::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(method: &str, params: &str) -> Value {
        serde_json::from_str(&call(method, params)).unwrap()
    }

    #[test]
    fn envelopes() {
        assert_eq!(parsed("core.version", "")["result"]["abi"], json!(crate::ABI_VERSION));
        assert_eq!(parsed("nope", "{}")["error"]["code"], json!("unknownMethod"));
        assert_eq!(parsed("quota.classify", "[1]")["error"]["code"], json!("invalidParams"));
        assert_eq!(parsed("quota.classify", "{")["error"]["code"], json!("invalidParams"));
        assert_eq!(parsed("quota.classify", r#"{"body":"x"}"#)["error"]["code"], json!("invalidParams"));
    }

    #[test]
    fn quota_methods() {
        assert_eq!(parsed("quota.classify", r#"{"status":402,"body":""}"#)["result"], json!("balance"));
        assert_eq!(parsed("quota.classify", r#"{"status":500,"body":"quota"}"#)["result"], Value::Null);
        let now = quota::utc(2026, 10, 4, 6, 0, 0).timestamp_millis();
        let delay = parsed("quota.resetDelay", &json!({ "headers": { "retry-after": "120" }, "body": "", "now": now }).to_string());
        assert_eq!(delay["result"], json!(120.0));
        let delay = parsed("quota.resetDelay", &json!({ "body": "限额将在 2026-10-04 19:07:26 重置", "now": now }).to_string());
        assert_eq!(delay["result"], json!(18446.0));
        assert_eq!(parsed("quota.resetValue", &json!({ "raw": "1791097200", "now": now }).to_string())["result"], json!(3600.0));
        assert_eq!(parsed("quota.duration", r#"{"text":"6m0s"}"#)["result"], json!(360.0));
        assert_eq!(parsed("quota.probeDelay", r#"{"kind":"usage","attempt":2}"#)["result"], json!(1200.0));
        assert_eq!(parsed("quota.probeDelay", r#"{"kind":"x","attempt":2}"#)["error"]["code"], json!("invalidParams"));
        assert_eq!(parsed("quota.headerNames", "")["result"][0], json!("Retry-After"));
    }

    #[test]
    fn tools_methods() {
        assert_eq!(parsed("tools.canonicalName", r#"{"name":"Grep"}"#)["result"], json!("search_files"));
        let changed = parsed("tools.canonicalArguments", r#"{"name":"read_file","arguments":{"file":"a"}}"#);
        assert_eq!(changed["result"], json!({"path": "a"}));
        assert_eq!(parsed("tools.canonicalArguments", r#"{"name":"read_file","arguments":{"path":"a"}}"#)["result"], Value::Null);
        assert_eq!(parsed("tools.canonicalArguments", r#"{"name":"x","arguments":[]}"#)["error"]["code"], json!("invalidParams"));
    }
}
