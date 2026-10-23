//! 两端调用的统一入口：`call(方法名, JSON 参数) -> JSON`。
//!
//! 结果总是一个对象：成功 `{"result": …}`，失败 `{"error": {"code": "…", "message": "…"}}`。
//! code：`unknownMethod`（没有这个方法）、`invalidParams`（参数不对）、`internal`（核心里出错，C 接口层兜住的 panic 也报这个）。
//! 时间一律用 Unix 毫秒（`now` 不传 = 现在），时长一律用秒（浮点）。
//!
//! 方法按模块分组（`quota.*`、`tools.*`、`secrets.*`、`vault.*`、`fold.*`、`conversations.*`）。有状态的（`vault.*`）先 open 拿句柄，后面的方法带上它，每个方法的参数和结果写在下面各自的函数上。

use serde_json::{Map, Value, json};

use std::collections::{HashMap, HashSet};

use crate::{conversations, folding, quota, secret_guard, secret_store, tool_aliases};

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
        // {kind, resumeAt?, attempts, now?} → {kind, resumeAt, known, attempts}（resumeAt：服务端说的恢复时间，没说不传）
        "quota.wait" => {
            let resume_at = match params.get("resumeAt") {
                None | Some(Value::Null) => None,
                Some(_) => Some(time(params, "resumeAt")?),
            };
            let wait = quota::wait_for(wait_kind(params)?, resume_at, int(params, "attempts")?, now(params)?);
            Ok(json!({
                "kind": if wait.kind == quota::WaitKind::Balance { "balance" } else { "usage" },
                "resumeAt": wait.resume_at.timestamp_millis(),
                "known": wait.known,
                "attempts": wait.attempts,
            }))
        }
        // {kind, resumeAt, known, now?} → 消息上「在等额度」那一行
        "quota.waitText" => {
            let known = params.get("known").and_then(Value::as_bool).unwrap_or(false);
            Ok(Value::from(quota::wait_text(wait_kind(params)?, time(params, "resumeAt")?, known, now(params)?)))
        }
        // {date, now?} → 「14:07」或「10月5日 14:07」（本机时区）
        "quota.clockText" => Ok(Value::from(quota::clock_text(time(params, "date")?, now(params)?))),
        // {seconds} → 「25 分钟」「3 小时 5 分」「2 天 4 小时」
        "quota.remainingText" => Ok(Value::from(quota::remaining_text(
            params.get("seconds").and_then(Value::as_f64).ok_or_else(|| ApiError::invalid("缺少 seconds"))?,
        ))),
        // {kind: "usage" | "balance", attempt} → 秒
        "quota.probeDelay" => Ok(Value::from(quota::probe_delay(wait_kind(params)?, int(params, "attempt")?))),

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

        // {values: {名字: 值}, ignored: [哈希], released: {名字: 值}} → {"known": 参与全文匹配的个数}（密钥库每次变化时调）
        "secrets.update" => {
            let known = secret_guard::shared().update(
                &string_map(params, "values")?,
                string_list(params, "ignored")?.into_iter().collect::<HashSet<_>>(),
                &string_map(params, "released")?,
            );
            Ok(json!({ "known": known }))
        }
        // {text, strongOnly?} → [{value, name, start, length, fromContext}]（start、length 按 UTF-16）
        "secrets.detect" => {
            let text = string(params, "text")?;
            let strong = params.get("strongOnly").and_then(Value::as_bool).unwrap_or(false);
            let candidates = secret_guard::shared().detect(&text, strong);
            Ok(Value::from(
                candidates
                    .into_iter()
                    .map(|candidate| {
                        let start = secret_guard::utf16_offset(&text, candidate.start);
                        json!({
                            "value": candidate.value,
                            "name": candidate.name,
                            "start": start,
                            "length": secret_guard::utf16_offset(&text, candidate.end) - start,
                            "fromContext": candidate.from_context,
                        })
                    })
                    .collect::<Vec<_>>(),
            ))
        }
        // {text} → 换过的文本
        "secrets.mask" => Ok(Value::from(secret_guard::shared().mask(&string(params, "text")?))),
        // {value} → 每个字符串都遮过的 value（发给在线模型之前）
        "secrets.maskJSON" => Ok(secret_guard::shared().mask_json(params.get("value").cloned().unwrap_or(Value::Null))),
        // {text} → 换过的 JSON 文本
        "secrets.maskStored" => Ok(Value::from(secret_guard::shared().mask_stored(&string(params, "text")?))),
        // {directory} → null（数据目录里的 conversations.json、learning.json、journal.json 原地换）
        "secrets.scrubStoredFiles" => {
            secret_guard::shared().scrub_stored_files(std::path::Path::new(&string(params, "directory")?));
            Ok(Value::Null)
        }
        // {value} → 是不是用户说过「不是密钥」的
        "secrets.isIgnored" => Ok(Value::from(secret_guard::shared().is_ignored(&string(params, "value")?))),
        // {value} → SHA-256（小写十六进制）
        "secrets.hash" => Ok(Value::from(secret_guard::hash(&string(params, "value")?))),
        // {name} → 密钥名合不合法
        "secrets.isValidName" => Ok(Value::from(secret_guard::is_valid_name(&string(params, "name")?))),

        // {directory?} → {vault, names, auto, known}（打开数据目录里的密钥库；不给 directory：只在内存里的库，不读写文件、不动 SecretGuard；
        //   auto：{名字: {source, addedAt(毫秒)}}）
        "vault.open" => {
            let directory = string(params, "directory")?;
            let id = secret_store::open((!directory.is_empty()).then(|| std::path::Path::new(&directory)));
            let mut snapshot = vault(params, id, |vault| vault.snapshot())?;
            snapshot["vault"] = Value::from(id);
            Ok(snapshot)
        }
        // {vault} → {known}（还有别的库开着就把最后打开的那个的值交给 SecretGuard，都关了才撤掉；known 是 SecretGuard 现在参与全文匹配的个数）
        "vault.close" => Ok(json!({ "known": secret_store::close(vault_id(params)?) })),
        // {vault} → {names, auto, known}
        "vault.snapshot" => vault(params, vault_id(params)?, |vault| vault.snapshot()),
        // {vault, name} → 值 | null
        "vault.get" => {
            let name = string(params, "name")?;
            vault(params, vault_id(params)?, |vault| vault.get(&name).map_or(Value::Null, Value::from))
        }
        // {vault, name, value} → {saved, names, auto, known}（值去掉首尾空白，空的不存）
        "vault.set" => {
            let (name, value) = (string(params, "name")?, string(params, "value")?);
            vault(params, vault_id(params)?, |vault| {
                let saved = vault.set(&name, &value);
                if saved {
                    vault.publish();
                }
                with_snapshot(vault, json!({ "saved": saved }))
            })
        }
        // {vault, name} → {names, auto, known}
        "vault.remove" => {
            let name = string(params, "name")?;
            vault(params, vault_id(params)?, |vault| {
                vault.remove(&name);
                vault.publish();
                vault.snapshot()
            })
        }
        // {vault, name} → 只露出前后几位的写法
        "vault.masked" => {
            let name = string(params, "name")?;
            vault(params, vault_id(params)?, |vault| Value::from(vault.masked(&name)))
        }
        // {vault, text, source} → {text, changed, names?, auto?, known?}（changed：收进了新的，带上快照）
        "vault.protect" => {
            let (text, source) = (string(params, "text")?, string(params, "source")?);
            vault(params, vault_id(params)?, |vault| {
                let (protected, changed) = vault.protect(&text, &source);
                let result = json!({ "text": protected, "changed": changed });
                if changed { with_snapshot(vault, result) } else { result }
            })
        }
        // {vault, directory, once} → {changed, names, auto, known}
        "vault.scanHistory" => {
            let directory = string(params, "directory")?;
            let once = params.get("once").and_then(Value::as_bool).unwrap_or(false);
            vault(params, vault_id(params)?, |vault| {
                let changed = vault.scan_stored_history(std::path::Path::new(&directory), once);
                with_snapshot(vault, json!({ "changed": changed }))
            })
        }
        // {vault, name} → {changed, names, auto, known}
        "vault.markNotSecret" => {
            let name = string(params, "name")?;
            vault(params, vault_id(params)?, |vault| {
                let changed = vault.mark_not_secret(&name);
                with_snapshot(vault, json!({ "changed": changed }))
            })
        }
        // {vault, text} → 换回明文的文本
        "vault.substitute" => {
            let text = string(params, "text")?;
            vault(params, vault_id(params)?, |vault| Value::from(vault.substitute(&text)))
        }
        // {vault, value} → 每个字符串都换回明文的 value
        "vault.substituteJSON" => {
            let value = params.get("value").cloned().unwrap_or(Value::Null);
            vault(params, vault_id(params)?, |vault| vault.substitute_json(value))
        }
        // {vault, text} → [引用了但库里没有的名字]
        "vault.missingReferences" => {
            let text = string(params, "text")?;
            vault(params, vault_id(params)?, |vault| Value::from(vault.missing_references(&text)))
        }
        // {vault, names} → {名字: 值}（只含库里有的）
        "vault.environment" => {
            let names = string_list(params, "names")?;
            vault(params, vault_id(params)?, |vault| json!(vault.environment(&names)))
        }

        // {path, teamTools} → [Conversation]（读 conversations.json 并规整：解不出的那段跳过、旧存档补父子链、
        //   上次中途退出的标成中断、按更新时间从新到旧；teamTools：会跑子 Agent 的工具名，中断时说明不会再交回结果）
        "conversations.load" => {
            let path = string(params, "path")?;
            let loaded = conversations::load(std::path::Path::new(&path), &string_list(params, "teamTools")?);
            serde_json::to_value(loaded).map_err(|error| ApiError { code: "internal", message: error.to_string() })
        }

        // {conversations, teamTools} → [Conversation]（同 conversations.load，规整内存里的会话）
        "conversations.normalize" => {
            let loaded = conversations::normalize(list(params, "conversations")?, &string_list(params, "teamTools")?);
            serde_json::to_value(loaded).map_err(|error| ApiError { code: "internal", message: error.to_string() })
        }
        // {blocks, teamTools} → [MessageBlock]（还在运行 / 等确认的调用标成中断，递归进子 Agent 记录）
        "blocks.markInterrupted" => {
            let mut blocks: Vec<crate::chat_models::MessageBlock> = serde_json::from_value(Value::Array(list(params, "blocks")?))
                .map_err(|error| ApiError::invalid(format!("blocks 解不出来：{error}")))?;
            crate::chat_models::mark_interrupted(&mut blocks, &string_list(params, "teamTools")?);
            serde_json::to_value(blocks).map_err(|error| ApiError { code: "internal", message: error.to_string() })
        }

        // {} → 主会话折叠的预算（估算 token；子 Agent 会话是它的两倍）
        "fold.budget" => Ok(Value::from(folding::BUDGET)),
        // {text} → 较早的回复正文的摘要
        "fold.text" => Ok(Value::from(folding::text(&string(params, "text")?))),
        // {arguments} → 较早的工具调用参数（对象）的摘要
        "fold.arguments" => Ok(Value::Object(folding::arguments(&object(params, "arguments")?))),
        // {json} → 较早的工具调用参数（JSON 文本）的摘要
        "fold.argumentsJSON" => Ok(Value::from(folding::arguments_json(&string(params, "json")?))),
        // {name, text, taskStateTool, wholeTools} → 较早的工具结果的摘要
        "fold.toolResult" => Ok(Value::from(folding::tool_result(&string(params, "name")?, &string(params, "text")?, &special(params)?))),
        // {entries} → 估算 token（entries 是在线接口写法的请求历史）
        "fold.estimate" => Ok(Value::from(folding::estimated_tokens(&list(params, "entries")?))),
        // {history, folded, budget, echoesReasoning, taskStateTool, wholeTools}
        //   → {steps, folded, before, after, history?}（folded 变了才带 history）
        "fold.session" => {
            let mut history = list(params, "history")?;
            let start = usize::try_from(int(params, "folded")?).map_err(|_| ApiError::invalid("folded 不能是负数"))?;
            let mut folded = start;
            let echoes = params.get("echoesReasoning").and_then(Value::as_bool).unwrap_or(false);
            let applied = folding::apply(&mut history, &mut folded, int(params, "budget")?, echoes, &special(params)?);
            let mut result = json!({ "steps": applied.steps, "folded": folded, "before": applied.before, "after": applied.after });
            if folded != start {
                result["history"] = Value::Array(history);
            }
            Ok(result)
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

fn object(params: &Params, key: &str) -> Result<Map<String, Value>, ApiError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(object)) => Ok(object.clone()),
        Some(_) => Err(ApiError::invalid(format!("{key} 要是对象"))),
    }
}

fn list(params: &Params, key: &str) -> Result<Vec<Value>, ApiError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items.clone()),
        Some(_) => Err(ApiError::invalid(format!("{key} 要是数组"))),
    }
}

fn special(params: &Params) -> Result<folding::Special, ApiError> {
    Ok(folding::Special { task_state: string(params, "taskStateTool")?, whole: string_list(params, "wholeTools")? })
}

fn vault_id(params: &Params) -> Result<u64, ApiError> {
    params.get("vault").and_then(Value::as_u64).ok_or_else(|| ApiError::invalid("缺少 vault（vault.open 返回的句柄）"))
}

fn vault(_params: &Params, id: u64, body: impl FnOnce(&mut secret_store::Vault) -> Value) -> Result<Value, ApiError> {
    secret_store::with(id, body).ok_or_else(|| ApiError::invalid(format!("没有这个密钥库句柄：{id}（关掉了？）")))
}

fn with_snapshot(vault: &secret_store::Vault, mut result: Value) -> Value {
    if let (Value::Object(result), Value::Object(snapshot)) = (&mut result, vault.snapshot()) {
        result.extend(snapshot);
    }
    result
}

fn string_map(params: &Params, key: &str) -> Result<HashMap<String, String>, ApiError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(HashMap::new()),
        Some(Value::Object(object)) => object
            .iter()
            .map(|(name, value)| value.as_str().map(|text| (name.clone(), text.to_string())))
            .collect::<Option<_>>()
            .ok_or_else(|| ApiError::invalid(format!("{key} 的值要是字符串"))),
        Some(_) => Err(ApiError::invalid(format!("{key} 要是对象"))),
    }
}

fn string_list(params: &Params, key: &str) -> Result<Vec<String>, ApiError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect::<Option<_>>()
            .ok_or_else(|| ApiError::invalid(format!("{key} 里要是字符串"))),
        Some(_) => Err(ApiError::invalid(format!("{key} 要是数组"))),
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

fn wait_kind(params: &Params) -> Result<quota::WaitKind, ApiError> {
    match string(params, "kind")?.as_str() {
        "usage" => Ok(quota::WaitKind::Usage),
        "balance" => Ok(quota::WaitKind::Balance),
        other => Err(ApiError::invalid(format!("kind 不认识：{other}"))),
    }
}

/// Unix 毫秒 → 时间
fn time(params: &Params, key: &str) -> Result<chrono::DateTime<chrono::Utc>, ApiError> {
    params
        .get(key)
        .and_then(Value::as_f64)
        .and_then(|millis| quota::from_epoch_millis(millis.round() as i64))
        .ok_or_else(|| ApiError::invalid(format!("{key} 要是 Unix 毫秒")))
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
        let wait = parsed("quota.wait", &json!({ "kind": "usage", "resumeAt": now + 3_600_000, "attempts": 1, "now": now }).to_string());
        assert_eq!(wait["result"], json!({ "kind": "usage", "resumeAt": now + 3_630_000, "known": true, "attempts": 1 }));
        let wait = parsed("quota.wait", &json!({ "kind": "balance", "attempts": 2, "now": now }).to_string());
        assert_eq!(wait["result"]["resumeAt"], json!(now + 1_800_000));
        assert_eq!(parsed("quota.remainingText", r#"{"seconds":3720}"#)["result"], json!("1 小时 2 分"));
        let text = parsed("quota.waitText", &json!({ "kind": "balance", "resumeAt": now, "known": false, "now": now }).to_string());
        assert_eq!(text["result"], json!("额度应该恢复了，马上接着做"));
    }

    #[test]
    fn secrets_methods() {
        let _guard = secret_store::GLOBAL_GUARD_TEST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // 全局那一份：只用这个测试自己的名字，不和别的测试抢
        let token = concat!("ghp_", "ApiApiApiApiApiApiApiApi0123456789ab");
        let detected = parsed("secrets.detect", &json!({ "text": format!("😀 https://x:{token}@github.com") }).to_string());
        assert_eq!(detected["result"][0]["name"], json!("GITHUB_TOKEN"));
        assert_eq!(detected["result"][0]["start"], json!(13));
        assert_eq!(detected["result"][0]["length"], json!(token.len()));
        assert_eq!(parsed("secrets.isValidName", r#"{"name":"API_KEY"}"#)["result"], json!(true));
        assert_eq!(parsed("secrets.hash", r#"{"value":"abc"}"#)["result"].as_str().map(str::len), Some(64));
        assert_eq!(parsed("secrets.update", r#"{"values":{"A":1}}"#)["error"]["code"], json!("invalidParams"));
    }

    #[test]
    fn fold_methods() {
        assert_eq!(parsed("fold.budget", "")["result"], json!(48_000));
        let special = json!({ "taskStateTool": "update_task_state", "wholeTools": ["delegate_task"] });
        let mut params = special.clone();
        params["name"] = json!("update_task_state");
        params["text"] = json!("x");
        assert_eq!(parsed("fold.toolResult", &params.to_string())["result"], json!("（已记下任务状态）"));
        assert_eq!(parsed("fold.estimate", r#"{"entries":[{"content":"aaaaaaa"}]}"#)["result"], json!(2));
        // 没超预算：不带 history
        let mut params = special.clone();
        params["history"] = json!([{ "role": "user", "content": "hi" }]);
        params["folded"] = json!(0);
        params["budget"] = json!(10);
        let result = parsed("fold.session", &params.to_string());
        assert_eq!(result["result"]["steps"], json!(0));
        assert!(result["result"].get("history").is_none());
        params["folded"] = json!(-1);
        assert_eq!(parsed("fold.session", &params.to_string())["error"]["code"], json!("invalidParams"));
    }

    #[test]
    fn vault_methods() {
        let _guard = secret_store::GLOBAL_GUARD_TEST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let directory = std::env::temp_dir().join(format!("friday-core-api-vault-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let opened = parsed("vault.open", &json!({ "directory": directory }).to_string());
        let id = opened["result"]["vault"].as_u64().unwrap();
        assert_eq!(opened["result"]["names"], json!([]));
        let set = parsed("vault.set", &json!({ "vault": id, "name": "API_KEY", "value": " AbCdEf123456 " }).to_string());
        assert_eq!(set["result"]["saved"], json!(true));
        assert_eq!(set["result"]["names"], json!(["API_KEY"]));
        assert_eq!(set["result"]["known"], json!(1));
        assert_eq!(parsed("vault.get", &json!({ "vault": id, "name": "API_KEY" }).to_string())["result"], json!("AbCdEf123456"));
        let substituted = parsed("vault.substitute", &json!({ "vault": id, "text": "k=${secret:API_KEY}" }).to_string());
        assert_eq!(substituted["result"], json!("k=AbCdEf123456"));
        let environment = parsed("vault.environment", &json!({ "vault": id, "names": ["API_KEY", "X"] }).to_string());
        assert_eq!(environment["result"], json!({ "API_KEY": "AbCdEf123456" }));
        assert_eq!(parsed("vault.close", &json!({ "vault": id }).to_string())["result"], json!({ "known": 0 }));
        assert_eq!(parsed("vault.get", &json!({ "vault": id, "name": "API_KEY" }).to_string())["error"]["code"], json!("invalidParams"));
        assert_eq!(parsed("vault.get", r#"{"name":"API_KEY"}"#)["error"]["code"], json!("invalidParams"));
        let _ = std::fs::remove_dir_all(&directory);
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
