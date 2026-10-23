//! 两端调用的统一入口：`call(方法名, JSON 参数) -> JSON`。
//!
//! 结果总是一个对象：成功 `{"result": …}`，失败 `{"error": {"code": "…", "message": "…"}}`。
//! code：`unknownMethod`（没有这个方法）、`invalidParams`（参数不对）、`internal`（核心里出错，C 接口层兜住的 panic 也报这个）。
//! 时间一律用 Unix 毫秒（`now` 不传 = 现在），时长一律用秒（浮点）。
//!
//! 方法按模块分组（`quota.*`、`tools.*`、`secrets.*`、`vault.*`、`fold.*`、`conversations.*`、`diff.*`、`journal.*`、`engines.*`、`prompts.*`、`promptHistory.*`、`models.*`、`stream.*`、`repetition.*`）。有状态的（`vault.*`、`journal.*`、`stream.*`）先 open 拿句柄，后面的方法带上它，每个方法的参数和结果写在下面各自的函数上。

use serde_json::{Map, Value, json};

use std::collections::{HashMap, HashSet};

use crate::{
    chat_stream, conversations, engines, folding, journal, line_diff, model_traits, prompt_history, prompts, quota, repetition,
    secret_guard, secret_store, tool_aliases,
};

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
        // {} → null（等后台排着的写盘都写完；两端退出前调）
        "core.flush" => {
            crate::writer::flush();
            Ok(Value::Null)
        }

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

        // {old, new} → {added, removed}（按全文算改了几行）
        "diff.count" => {
            let counts = line_diff::count_text(&string(params, "old")?, &string(params, "new")?);
            Ok(json!({ "added": counts.added, "removed": counts.removed }))
        }
        // {a, b} → {added, removed}（两边是每行的指纹，16 位十六进制）
        "diff.countHashes" => {
            let counts = line_diff::count(&hashes(params, "a")?, &hashes(params, "b")?);
            Ok(json!({ "added": counts.added, "removed": counts.removed }))
        }
        // {a, b} → ["same" | "removed" | "added"]（每一行的去向）
        "diff.script" => Ok(Value::from(
            line_diff::script(&hashes(params, "a")?, &hashes(params, "b")?).into_iter().map(line_diff::Op::name).collect::<Vec<_>>(),
        )),
        // {old, new} → [{op, oldLine, newLine, text}]（改动视图：两边逐行对出来）
        "diff.lines" => Ok(Value::from(
            line_diff::diff_lines(&string(params, "old")?, &string(params, "new")?)
                .into_iter()
                .map(|line| json!({ "op": line.op.name(), "oldLine": line.old_line, "newLine": line.new_line, "text": line.text }))
                .collect::<Vec<_>>(),
        )),
        // {text} → [指纹]（16 位十六进制；两端各有一份同样的算法，对照用）
        "diff.lineHashes" => {
            Ok(Value::from(line_diff::line_hashes(&string(params, "text")?).into_iter().map(line_diff::hex).collect::<Vec<_>>()))
        }

        // {directory?} → {journal}（打开数据目录里的 journal.json；不给 directory：只在内存里）
        "journal.open" => {
            let directory = string(params, "directory")?;
            Ok(json!({ "journal": journal::open((!directory.is_empty()).then(|| std::path::Path::new(&directory))) }))
        }
        // {journal} → null
        "journal.close" => {
            journal::close(handle(params, "journal")?);
            Ok(Value::Null)
        }
        // {journal, kind, title, detail} → 记下没有（kind：tool / framework / mcp / recovered；detail 截到 3000 字）
        "journal.record" => {
            let (kind, title, detail) = (string(params, "kind")?, string(params, "title")?, string(params, "detail")?);
            journal_of(params, |journal| Value::from(journal.record(&kind, &title, &detail)))
        }
        // {journal, limit?} → [{id, date, kind, title, detail}]（新的在前）
        "journal.entries" => {
            let limit = params.get("limit").and_then(Value::as_u64).map(|limit| limit as usize);
            journal_of(params, |journal| Value::from(journal.entries(limit).to_vec()))
        }
        // {journal} → null
        "journal.clear" => journal_of(params, |journal| {
            journal.clear();
            Value::Null
        }),
        // {showReasoning, quota} → {stream}（一段请求一个；quota：流里的报错要不要按额度用完认）
        "stream.open" => {
            let flag = |key: &str| params.get(key).and_then(Value::as_bool).unwrap_or(false);
            Ok(json!({ "stream": chat_stream::open(flag("showReasoning"), flag("quota")) }))
        }
        // {stream} → null
        "stream.close" => {
            chat_stream::close(handle(params, "stream")?);
            Ok(Value::Null)
        }
        // {chunk, stream, now?} → {chunks?, argumentsDelta?, firstToken?, loop?, error?: {kind: quota | rateLimit | failed, message,
        // limit?, resetSeconds?}}（chunk 是 SSE data: 的负载原样嵌进来；空的字段不写；loop / error：不再往下读）
        "stream.feed" => {
            let now = now(params)?;
            let chunk = params.get("chunk").unwrap_or(&Value::Null);
            stream_of(params, |parser| parser.feed(chunk, now).to_json())
        }
        // {stream} → {content, reasoning, toolCalls: [{id, name, arguments}], promptTokens, completionTokens, cachedTokens, finishReason,
        // loop: {start, unit, inReasoning} | null, inReasoning}（读完、停下、出错都能取；start 是 UTF-16 下标）
        "stream.finish" => stream_of(params, |parser| parser.result()),
        // {finishReason?, sawDone} → {transient, message} | null（流读完之后看是不是其实被掐断了）
        "stream.interrupted" => {
            let reason = string(params, "finishReason")?;
            let saw_done = params.get("sawDone").and_then(Value::as_bool).unwrap_or(false);
            Ok(chat_stream::interrupted(Some(&reason), saw_done)
                .map_or(Value::Null, |(transient, message)| json!({ "transient": transient, "message": message })))
        }
        // {error} → 流里的 error 对象是不是限流
        "stream.isRateLimit" => Ok(Value::from(chat_stream::is_rate_limit(&object(params, "error")?))),
        // {usage} → 命中缓存的输入 token | null
        "stream.cachedTokens" => Ok(chat_stream::cached_tokens(&object(params, "usage")?).map_or(Value::Null, Value::from)),
        // {text, minPeriod, copies} → {start, unit} | null（末尾连着复读；start 是 UTF-16 下标）
        "repetition.trailingLoop" => {
            let (min_period, copies) = (int(params, "minPeriod")?.max(0) as usize, int(params, "copies")?.max(0) as usize);
            Ok(repetition::trailing_loop_in(&string(params, "text")?, min_period, copies)
                .map_or(Value::Null, |found| json!({ "start": found.start, "unit": found.unit })))
        }
        // {text, earlier: [文字]} → 和之前几步说差不多的话的次数
        "repetition.similarCount" => Ok(Value::from(repetition::similar_count(&string(params, "text")?, &string_list(params, "earlier")?))),
        // {a, b} → 相似度（0–1）
        "repetition.similarity" => Ok(Value::from(repetition::similarity(&string(params, "a")?, &string(params, "b")?))),
        // {model, baseURL?, title?} → {tier, billing, isChat}（按模型名、接口地址、服务名推断；用户在高级设置里改过的两端自己用）
        "models.traits" => {
            let (model, base_url, title) = (string(params, "model")?, string(params, "baseURL")?, string(params, "title")?);
            Ok(json!({
                "tier": model_traits::infer_tier(&model),
                "billing": model_traits::infer_billing(&base_url, &title, &model),
                "isChat": model_traits::is_chat_model(&model),
            }))
        }
        // {baseURL, model, sendsEnableThinking, thinking?} → {thinkingSupport, inferredThinking, nativeSwitch, isLocal, echoesReasoning,
        //   asksParallelToolCalls, thinkingLevels}（thinking：用户设了的思考方式，没设按推断）
        "engines.rules" => {
            let (base_url, model) = (string(params, "baseURL")?, string(params, "model")?);
            let sends = params.get("sendsEnableThinking").and_then(Value::as_bool).unwrap_or(false);
            let inferred = engines::inferred_thinking(&base_url, sends);
            let set = string(params, "thinking")?;
            let support = if set.is_empty() { inferred.to_string() } else { set };
            Ok(json!({
                "thinkingSupport": support,
                "inferredThinking": inferred,
                "nativeSwitch": engines::native_switch(&base_url),
                "isLocal": engines::is_local(&base_url),
                "echoesReasoning": engines::echoes_reasoning(&base_url, &model),
                "asksParallelToolCalls": engines::asks_parallel_tool_calls(&base_url),
                "thinkingLevels": engines::thinking_levels(&support),
            }))
        }
        // {path} → [EngineConfig]（读 engines.json：缺的键补默认值，认不出的取值去掉）
        "engines.load" => to_value(engines::load(std::path::Path::new(&string(params, "path")?))),
        // {engines} → [EngineConfig]（同上，规整内存里的）
        "engines.normalize" => to_value(engines::normalize(list(params, "engines")?)),
        // {path, engines} → [EngineConfig]（规整后写进 engines.json，返回写下的）
        "engines.save" => {
            let normalized = engines::normalize(list(params, "engines")?);
            engines::save(std::path::Path::new(&string(params, "path")?), &normalized)
                .map_err(|error| ApiError { code: "internal", message: format!("写 engines.json 失败：{error}") })?;
            to_value(normalized)
        }

        // {bundled: {文件名: 内容}, directory?, platform: [{name, value} | {name, section}]} → {installed, added, updated, keptLocal}
        //   （两端第一次用到提示词时调：自带的文件、数据目录里的提示词文件夹（不给 = 用不了，全用自带的）、平台参数的取值）
        "prompts.setup" => {
            let bundled = string_map(params, "bundled")?.into_iter().collect();
            let directory = string(params, "directory")?;
            let mut platform = Vec::new();
            for item in list(params, "platform")? {
                let name = item.get("name").and_then(Value::as_str).ok_or_else(|| ApiError::invalid("platform 里每项要有 name"))?;
                let value = match (item.get("value").and_then(Value::as_str), item.get("section").and_then(Value::as_str)) {
                    (Some(value), _) => prompts::Platform::Value(value.to_string()),
                    (None, Some(section)) => prompts::Platform::Section(section.to_string()),
                    _ => return Err(ApiError::invalid(format!("platform 的 {name} 要有 value 或 section"))),
                };
                platform.push((name.to_string(), value));
            }
            prompts::setup(bundled, (!directory.is_empty()).then(|| directory.into()), platform);
            Ok(merge_report(prompts::last_merge()))
        }
        // {key, args?, override?} → 展开好的文字（override：prompts.override 返回的句柄）
        "prompts.text" => {
            let args: prompts::Args = string_map(params, "args")?.into_iter().collect();
            Ok(Value::from(prompts::text(&string(params, "key")?, &args, override_id(params))))
        }
        // {key, override?} → 有没有这一段
        "prompts.contains" => Ok(Value::from(prompts::contains(&string(params, "key")?, override_id(params)))),
        // {override?} → {段名: 原文}（现在实际用的）
        "prompts.sections" => Ok(json!(prompts::sections(override_id(params)))),
        // {key, args?} → 自带的那一段展开好的
        "prompts.bundledText" => {
            let args: prompts::Args = string_map(params, "args")?.into_iter().collect();
            Ok(Value::from(prompts::bundled_text(&string(params, "key")?, &args)))
        }
        // {key} → 自带的那一段的原文
        "prompts.bundledSource" => Ok(Value::from(prompts::bundled_source(&string(params, "key")?))),
        // {} → {directory, fileNames, problems, lastMerge}
        "prompts.state" => Ok(json!({
            "directory": prompts::directory().map(|dir| dir.to_string_lossy().into_owned()),
            "fileNames": prompts::file_names(),
            "problems": prompts::problems(),
            "lastMerge": merge_report(prompts::last_merge()),
        })),
        // {fileName} → [段名]（按文件里的顺序）
        "prompts.bundledKeys" => Ok(Value::from(prompts::bundled_keys(&string(params, "fileName")?))),
        // {key, text} → 行返回空串，不行返回原因
        "prompts.checkSection" => Ok(Value::from(prompts::check_section(&string(params, "key")?, &string(params, "text")?))),
        // {} → [内容变了的段名]（重新读数据目录里的文件）
        "prompts.reload" => Ok(Value::from(prompts::reload())),
        // {key, text} → 写成了没有
        "prompts.setSection" => Ok(Value::from(prompts::set_section(&string(params, "key")?, &string(params, "text")?))),
        // {} → 写成了没有
        "prompts.restoreBundled" => Ok(Value::from(prompts::restore_bundled())),
        // {files: [内容]} → {段名: 原文}
        "prompts.compose" => Ok(json!(prompts::compose(&string_list(params, "files")?))),
        // {content} → {段名: 正文}
        "prompts.parse" => Ok(json!(prompts::parse(&string(params, "content")?))),
        // {sections, key, args?} → 按给定的段展开
        "prompts.render" => {
            let sections: prompts::Sections = string_map(params, "sections")?.into_iter().collect();
            let args: prompts::Args = string_map(params, "args")?.into_iter().collect();
            Ok(Value::from(prompts::render(&sections, &string(params, "key")?, &args)))
        }
        // {sections} → {override}（临时换成这一套段，用完 prompts.release）
        "prompts.override" => Ok(json!({ "override": prompts::override_sections(string_map(params, "sections")?.into_iter().collect()) })),
        // {override} → null
        "prompts.release" => {
            prompts::release(handle(params, "override")?);
            Ok(Value::Null)
        }

        // {} → {gitBase: [每次都带的 git 设置], logArguments: [读历史用的参数]}
        "promptHistory.git" => Ok(json!({ "gitBase": prompt_history::GIT_BASE, "logArguments": prompt_history::LOG_ARGUMENTS })),
        // {version} → 启动时那一次的提交说明（按上次准备提示词时的合并结果）
        "promptHistory.startupMessage" => {
            Ok(Value::from(prompt_history::startup_message(&string(params, "version")?, &prompts::last_merge())))
        }
        // {} → null（记下现在实际用的段：每次提交完、没装 git 时也记）
        "promptHistory.markCommitted" => {
            prompt_history::mark_committed();
            Ok(Value::Null)
        }
        // {} → 上次提交以来改了哪几段（提交说明）
        "promptHistory.describeChanges" => Ok(Value::from(prompt_history::describe_changes())),
        // {output} → [{id, shortID, time, message}]（git log 的输出解成版本列表，新的在前）
        "promptHistory.parseLog" => Ok(Value::from(
            prompt_history::parse_log(&string(params, "output")?).into_iter().map(|version| version_json(&version)).collect::<Vec<_>>(),
        )),
        // {history, id} → 「时间 的版本（说明）」，找不到是短编号
        "promptHistory.label" => Ok(Value::from(prompt_history::version_label(&versions(params)?, &string(params, "id")?))),
        // {history, byVersion: [{version, tasks, firstPass, deliveries, followups}], rejected} → {bad, good, goodID, reason, note, message} | null
        "promptHistory.regression" => {
            let mut by_version = Vec::new();
            for item in list(params, "byVersion")? {
                let field = |key: &str| item.get(key).and_then(Value::as_i64).unwrap_or(0);
                let version = item.get("version").and_then(Value::as_str).unwrap_or("").to_string();
                by_version.push((
                    version,
                    prompt_history::Metrics {
                        tasks: field("tasks"),
                        first_pass: field("firstPass"),
                        deliveries: field("deliveries"),
                        followups: field("followups"),
                    },
                ));
            }
            let found = prompt_history::regression(&versions(params)?, &by_version, &string_list(params, "rejected")?);
            Ok(found.map_or(Value::Null, |found| {
                json!({ "bad": found.bad, "good": found.good, "goodID": found.good_id, "reason": found.reason, "note": found.note, "message": found.message })
            }))
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

fn hashes(params: &Params, key: &str) -> Result<Vec<u64>, ApiError> {
    string_list(params, key)?
        .iter()
        .map(|text| line_diff::parse_hex(text).ok_or_else(|| ApiError::invalid(format!("{key} 里要是 16 位十六进制的指纹：{text}"))))
        .collect()
}

fn handle(params: &Params, key: &str) -> Result<u64, ApiError> {
    params.get(key).and_then(Value::as_u64).ok_or_else(|| ApiError::invalid(format!("缺少 {key}（open 返回的句柄）")))
}

fn journal_of(params: &Params, body: impl FnOnce(&mut journal::Journal) -> Value) -> Result<Value, ApiError> {
    let id = handle(params, "journal")?;
    journal::with(id, body).ok_or_else(|| ApiError::invalid(format!("没有这个日志句柄：{id}（关掉了？）")))
}

fn stream_of(params: &Params, body: impl FnOnce(&mut chat_stream::Parser) -> Value) -> Result<Value, ApiError> {
    let id = handle(params, "stream")?;
    chat_stream::with(id, body).ok_or_else(|| ApiError::invalid(format!("没有这个流句柄：{id}（关掉了？）")))
}

fn to_value(value: impl serde::Serialize) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|error| ApiError { code: "internal", message: error.to_string() })
}

fn version_json(version: &prompt_history::Version) -> Value {
    json!({ "id": version.id, "shortID": version.short_id, "time": version.time, "message": version.message })
}

fn versions(params: &Params) -> Result<Vec<prompt_history::Version>, ApiError> {
    list(params, "history")?
        .into_iter()
        .map(|item| {
            let field = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or("").to_string();
            Ok(prompt_history::Version { id: field("id"), short_id: field("shortID"), time: field("time"), message: field("message") })
        })
        .collect()
}

fn override_id(params: &Params) -> Option<u64> {
    params.get("override").and_then(Value::as_u64)
}

fn merge_report(report: prompts::MergeReport) -> Value {
    json!({ "installed": report.installed, "added": report.added, "updated": report.updated, "keptLocal": report.kept_local })
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
    fn diff_methods() {
        assert_eq!(parsed("diff.count", r#"{"old":"a\nb\n","new":"a\nc\n"}"#)["result"], json!({ "added": 1, "removed": 1 }));
        let hashes = parsed("diff.lineHashes", r#"{"text":"a\nb"}"#)["result"].clone();
        let params = json!({ "a": hashes, "b": [hashes[0].clone()] });
        assert_eq!(parsed("diff.countHashes", &params.to_string())["result"], json!({ "added": 0, "removed": 1 }));
        assert_eq!(parsed("diff.script", &params.to_string())["result"], json!(["same", "removed"]));
        assert_eq!(parsed("diff.countHashes", r#"{"a":["xyz"],"b":[]}"#)["error"]["code"], json!("invalidParams"));
        let lines = parsed("diff.lines", r#"{"old":"","new":"x\n"}"#);
        assert_eq!(lines["result"], json!([{ "op": "added", "oldLine": 0, "newLine": 1, "text": "x" }]));
    }

    #[test]
    fn journal_and_engines_methods() {
        let id = parsed("journal.open", "{}")["result"]["journal"].as_u64().unwrap();
        let params = json!({ "journal": id, "kind": "framework", "title": "t", "detail": "d" });
        assert_eq!(parsed("journal.record", &params.to_string())["result"], json!(true));
        let entries = parsed("journal.entries", &json!({ "journal": id, "limit": 5 }).to_string());
        assert_eq!(entries["result"][0]["title"], json!("t"));
        parsed("journal.close", &json!({ "journal": id }).to_string());
        assert_eq!(parsed("journal.entries", &json!({ "journal": id }).to_string())["error"]["code"], json!("invalidParams"));
        let engines = parsed("engines.normalize", r#"{"engines":[{"id":"e","title":"t"}]}"#);
        assert_eq!(engines["result"][0]["model"], json!("gpt-4o-mini"));
    }

    #[test]
    fn tools_methods() {
        assert_eq!(parsed("tools.canonicalName", r#"{"name":"Grep"}"#)["result"], json!("search_files"));
        let changed = parsed("tools.canonicalArguments", r#"{"name":"read_file","arguments":{"file":"a"}}"#);
        assert_eq!(changed["result"], json!({"path": "a"}));
        assert_eq!(parsed("tools.canonicalArguments", r#"{"name":"read_file","arguments":{"path":"a"}}"#)["result"], Value::Null);
        assert_eq!(parsed("tools.canonicalArguments", r#"{"name":"x","arguments":[]}"#)["error"]["code"], json!("invalidParams"));
    }

    /// 两端把 SSE 的负载原样拼进参数：{"chunk":<负载>,"stream":N}——句柄写在最后，负载里夹带的 "stream" 盖不掉它，坏的负载整个不认
    #[test]
    fn stream_handle() {
        let id = parsed("stream.open", r#"{"showReasoning":true,"quota":true}"#)["result"]["stream"].as_u64().unwrap();
        let feed = |payload: &str| parsed("stream.feed", &format!(r#"{{"chunk":{payload},"stream":{id}}}"#));
        assert_eq!(feed(r#"{"choices":[{"delta":{"content":"你好"}}]}"#)["result"], json!({"chunks": ["你好"], "firstToken": true}));
        assert_eq!(feed(r#"{"choices":[{"delta":{"content":"！"}}]}"#)["result"], json!({"chunks": ["！"]}));
        assert_eq!(feed(r#"{"choices":[{"delta":{"content":"x"}}]},"stream":999999"#)["result"], json!({"chunks": ["x"]}));
        assert_eq!(feed(r#"{"choices":[{"#)["error"]["code"], json!("invalidParams"));
        let error = feed(r#"{"error":{"message":"KV 池满了","code":"429"}}"#)["result"]["error"].clone();
        assert_eq!(error, json!({"kind": "rateLimit", "message": "KV 池满了"}));
        let result = parsed("stream.finish", &json!({ "stream": id }).to_string())["result"].clone();
        assert_eq!(result["content"], "你好！x");
        assert_eq!(result["finishReason"], Value::Null);
        assert_eq!(parsed("stream.interrupted", r#"{"sawDone":false}"#)["result"]["transient"], json!(true));
        assert_eq!(parsed("stream.interrupted", r#"{"finishReason":"stop","sawDone":false}"#)["result"], Value::Null);
        parsed("stream.close", &json!({ "stream": id }).to_string());
        assert_eq!(parsed("stream.finish", &json!({ "stream": id }).to_string())["error"]["code"], json!("invalidParams"));
        assert_eq!(parsed("repetition.similarity", r#"{"a":"abcd","b":"abcd"}"#)["result"], json!(1.0));
    }
}
