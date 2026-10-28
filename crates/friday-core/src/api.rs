//! 两端调用的统一入口：`call(方法名, JSON 参数) -> JSON`。
//!
//! 结果总是一个对象：成功 `{"result": …}`，失败 `{"error": {"code": "…", "message": "…"}}`。
//! code：`unknownMethod`（没有这个方法）、`invalidParams`（参数不对）、`internal`（核心里出错，C 接口层兜住的 panic 也报这个）。
//! 时间一律用 Unix 毫秒（`now` 不传 = 现在），时长一律用秒（浮点）。
//!
//! 方法按模块分组（`quota.*`、`tools.*`、`secrets.*`、`vault.*`、`fold.*`、`conversations.*`、`diff.*`、`archive.*`、`mcpClient.*`、`journal.*`、`engines.*`、`prompts.*`、`promptHistory.*`、`models.*`、`stream.*`、`repetition.*`、`chat.*`、`gate.*`、`context.*`、`edit.*`、`files.*`、`changes.*`、`shell.*`、`tools.*`）。有状态的（`vault.*`、`journal.*`、`stream.*`）先 open 拿句柄，后面的方法带上它，每个方法的参数和结果写在下面各自的函数上。

use serde_json::{Map, Value, json};

use std::collections::{HashMap, HashSet};

use crate::chat_models::Conversation;
use crate::{
    agents, approval, archive, capability_store, chat_request, chat_stream, chooser, context, conversations, delivery, deploy, engine_loop,
    engines, events, file_changes, file_edit, file_search, folding, gate, journal, line_diff, liveness, mcp_client, mcp_config,
    model_traits, prompt_history, prompt_store, prompts, provider_store, quota, repetition, script_tools, secret_guard, secret_store,
    shell_rules, swift_json, task_state, team, tool_aliases, tool_defs, tool_files,
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
        // {request, result? | error?} → 在等没有（两端做完核心请它做的事，回话；见 events.rs）
        "core.reply" => {
            let id = handle(params, "request")?;
            let outcome = match params.get("error").and_then(Value::as_str) {
                Some(error) => Err(error.to_string()),
                None => Ok(params.get("result").cloned().unwrap_or(Value::Null)),
            };
            Ok(Value::from(events::reply(id, outcome)))
        }
        // {data?, delayMs?} → null（两端自检用：过 delayMs 毫秒在核心的线程上推一个 core.pong，data 原样带回）
        "core.ping" => {
            let data = params.get("data").cloned().unwrap_or(Value::Null);
            let delay = params.get("delayMs").and_then(Value::as_u64).unwrap_or(0);
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(delay));
                events::emit("core.pong", data);
            });
            Ok(Value::Null)
        }
        // {kind, data?, timeoutMs?} → null（两端自检用：在核心的线程上请两端做 kind，拿到的结果推成 core.testRequestDone {result | error}）
        "core.testRequest" => {
            let kind = string(params, "kind")?;
            let data = params.get("data").cloned().unwrap_or(Value::Null);
            let timeout = std::time::Duration::from_millis(params.get("timeoutMs").and_then(Value::as_u64).unwrap_or(5_000));
            std::thread::spawn(move || {
                let done = match events::request(&kind, data, Some(timeout)) {
                    Ok(result) => json!({ "result": result }),
                    Err(error) => json!({ "error": error }),
                };
                events::emit("core.testRequestDone", done);
            });
            Ok(Value::Null)
        }
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

        // {args} → {task, name?, role?, engine?, work?, thinking?, agentType?} | {failure}（派子智能体的参数）
        "agents.taskSpec" => Ok(match agents::task_spec(&object(params, "args")?) {
            Ok(spec) => spec,
            Err(failure) => json!({ "failure": failure }),
        }),
        // {pool: [{id, tier, billing, isChat?, vision?, usable?}], tier, preferred?, preferVision?} → 挑中的 id 或 null
        "models.pick" => {
            let pool = candidates(params, "pool")?;
            let preferred = params.get("preferred").and_then(Value::as_str).map(str::to_string);
            let index = chooser::pick(&pool, &string(params, "tier")?, preferred.as_deref(), flag(params, "preferVision"));
            Ok(json!(index.map(|index| pool[index].id.clone())))
        }
        // {pool} → 小活用的快模型 id 或 null
        "models.pickLight" => {
            let pool = candidates(params, "pool")?;
            Ok(json!(chooser::pick_light(&pool).map(|index| pool[index].id.clone())))
        }
        // {wanted?, work?, cap, levels?} → 子智能体的深度思考档位
        "models.thinking" => {
            let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_string);
            let levels = if params.contains_key("levels") { string_list(params, "levels")? } else { Vec::new() };
            Ok(json!(chooser::thinking(text("wanted").as_deref(), text("work").as_deref(), &string(params, "cap")?, &levels)))
        }
        // {work?, mainTier?} → 子智能体挑哪一档
        "models.subAgentTier" => {
            let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_string);
            Ok(json!(chooser::sub_agent_tier(text("work").as_deref(), text("mainTier").as_deref())))
        }
        // {mode, engines: [{id, title, model, tier, billing, isChat, vision, usable, disabled, power}], allowed: [同上], current?, requested?, work?, hasEnabled}
        //   → {engine} | {failure, kind: invalid | failed}（子智能体用哪个大脑）
        "agents.chooseEngine" => {
            let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_string);
            Ok(chooser::choose_engine(
                &text("mode").unwrap_or_default(),
                &candidates(params, "engines")?,
                &candidates(params, "allowed")?,
                text("current").as_deref(),
                text("requested").as_deref(),
                text("work").as_deref(),
                flag(params, "hasEnabled"),
            ))
        }
        // {text} → look / change / hard 或 null（说的活的种类，中文也认）
        "agents.parseWork" => Ok(json!(agents::parse_work(&string(params, "text")?))),
        // {text} → off / low / medium / high 或 null（深度思考，中文也认）
        "agents.parseThinking" => Ok(json!(agents::parse_thinking(&string(params, "text")?))),
        // {given?, definition?, role?, task, used} → 名字（给的 > 现成子智能体的 > 角色里的 > 任务第一句 > 通才；重名加编号）
        "agents.name" => {
            let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_string);
            let used: HashSet<String> = string_list(params, "used")?.into_iter().collect();
            Ok(json!(agents::agent_name(
                text("given").as_deref(),
                text("definition").as_deref(),
                text("role").as_deref(),
                &string(params, "task")?,
                &used
            )))
        }
        // {definitionRole?, role?, override?} → 子智能体的角色
        "agents.role" => {
            let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_string);
            Ok(json!(agents::composed_role(text("definitionRole").as_deref(), text("role").as_deref(), override_id(params))))
        }
        // {text} → {asksForPlan, asksForPlanExplicitly, looksLikeTask, taskWantsAnalysis, looksLikePlan, looksLikeAnnouncement, handsBackFix}
        //   （交付检查的「起疑」：用户的话 / 任务 / 回复各看要的那几项）
        "delivery.classify" => {
            let text = string(params, "text")?;
            Ok(json!({
                "asksForPlan": delivery::asks_for_plan(&text),
                "asksForPlanExplicitly": delivery::asks_for_plan_explicitly(&text),
                "looksLikeTask": delivery::looks_like_task(&text),
                "taskWantsAnalysis": delivery::task_wants_analysis(&text),
                "looksLikePlan": delivery::looks_like_plan(&text),
                "looksLikeAnnouncement": delivery::looks_like_announcement(&text),
                "handsBackFix": delivery::hands_back_fix(&text),
            }))
        }
        // {blocks, permissions: {名字: readOnly|writeFiles|execute|network}} → 真正动手了没有
        "delivery.didWork" => Ok(json!(delivery::did_work(&blocks_of(params)?, &string_map(params, "permissions")?))),
        // {blocks} → 最后一个工具调用之后的文字
        "delivery.finalReply" => Ok(json!(delivery::final_reply(&blocks_of(params)?))),
        // {text} → 「未完成与风险」写的东西或 null
        "delivery.unfinished" => Ok(json!(delivery::unfinished_items(&string(params, "text")?))),
        // {stops, offNote, override?} → 主智能体交了方案又没动手时的提醒
        "delivery.mainPrompt" => {
            Ok(json!(delivery::main_prompt(&string(params, "stops")?, &string(params, "offNote")?, override_id(params))))
        }
        // {delegates, override?} → 发现了问题却交回给用户时的提醒
        "delivery.fixPrompt" => Ok(json!(delivery::fix_prompt(flag(params, "delegates"), override_id(params)))),
        // {said, override?} → 只说了一句下一步时的提醒
        "delivery.announcePrompt" => Ok(json!(delivery::announce_prompt(&string(params, "said")?, override_id(params)))),
        // {result, unverified, stillPlan, stops, offNote, override?} → 交给父级的结果后面附的验收提示或 null
        "delivery.reviewHint" => Ok(json!(delivery::review_hint(
            &string(params, "result")?,
            &string_list(params, "unverified")?,
            flag(params, "stillPlan"),
            &string(params, "stops")?,
            &string(params, "offNote")?,
            override_id(params)
        ))),
        // {blocks, permissions, windows?} → 写了、写完之后没运行或读过的文件
        "turnCheck.unverified" => {
            let windows = params.get("windows").and_then(Value::as_bool).unwrap_or(cfg!(windows));
            Ok(json!(delivery::unverified_writes(&blocks_of(params)?, &string_map(params, "permissions")?, windows)))
        }
        // {request} → 用户说了不用运行 / 测试
        "turnCheck.declined" => Ok(json!(delivery::user_declined_verification(&string(params, "request")?))),
        // {files, delegates, override?} → 写完没验证的提醒
        "turnCheck.prompt" => {
            Ok(json!(delivery::turn_prompt(&string_list(params, "files")?, flag(params, "delegates"), override_id(params))))
        }
        // {id, blocks} → 按 id 前缀找到的子智能体事件或 null
        "agentLog.find" => {
            let blocks = blocks_of(params)?;
            Ok(delivery::find_log(&string(params, "id")?, &blocks).map(to_value).transpose()?.unwrap_or(Value::Null))
        }
        // {event} → 运行记录整理成的文字（不截断）
        "agentLog.text" => {
            let event: crate::chat_models::ToolEvent = serde_json::from_value(Value::Object(object(params, "event")?))
                .map_err(|error| ApiError::invalid(format!("event 解不出来：{error}")))?;
            Ok(json!(delivery::log_text(&event)))
        }
        // {state?, args} → 应用一次 update_task_state 之后的状态（state：{goal, constraints, decisions, steps: [{text, status}], facts}）
        "taskState.apply" => {
            let mut state = task_state_of(params)?;
            state.apply(&object(params, "args")?);
            to_value(state)
        }
        // {calls: [参数的 JSON 文本]} → 依次重放后的状态
        "taskState.replay" => to_value(task_state::replay(&string_list(params, "calls")?)),
        // {state} → 展示给模型的样子
        "taskState.render" => Ok(json!(task_state_of(params)?.rendered())),
        // {old, new} → 两份之间改了什么（行）
        "taskState.delta" => Ok(json!(task_state::delta_lines(&string(params, "old")?, &string(params, "new")?))),
        // {note, previous?} → 附在用户消息后面交给模型的样子或 null
        "taskState.renderedNote" => {
            let previous = params.get("previous").and_then(Value::as_str).map(str::to_string);
            Ok(json!(task_state::rendered_note(&string(params, "note")?, previous.as_deref())))
        }
        // {events: [{note?, folded} | {call, shown}]} → {notes: [每条 note 交给模型的样子或 null], text?}（模型在没折叠的部分里看到的）
        "taskState.visible" => {
            let events: Vec<task_state::Event> = list(params, "events")?
                .iter()
                .map(|event| match event.get("call").and_then(Value::as_str) {
                    Some(call) => task_state::Event::Call {
                        arguments: call.to_string(),
                        shown: event.get("shown").and_then(Value::as_bool).unwrap_or(true),
                    },
                    None => task_state::Event::Note {
                        note: event.get("note").and_then(Value::as_str).map(str::to_string),
                        folded: event.get("folded").and_then(Value::as_bool).unwrap_or(false),
                    },
                })
                .collect();
            let (notes, text) = task_state::visible(&events);
            Ok(json!({ "notes": notes, "text": text }))
        }
        // {text?, state, args, shown} → {text, state, before}（沿路重放一次 update_task_state：模型看到的整份、重放到的状态、改之前的状态）
        "taskState.visibleApply" => {
            let mut text = params.get("text").and_then(Value::as_str).map(str::to_string);
            let mut state = task_state_of(params)?;
            let before = task_state::visible_apply(&mut text, &mut state, &object(params, "args")?, flag(params, "shown"));
            Ok(json!({ "text": text, "state": to_value(state)?, "before": to_value(before)? }))
        }
        // {state, messages: [{text, fromAgent?, interjected?, files?}]} → 附在用户消息后面的那段或 null
        "taskState.promptNote" => Ok(json!(task_state::prompt_note(&task_state_of(params)?, &user_messages(params)?))),
        // {state, messages} → 附在子智能体任务后面的主任务背景或 null
        "taskState.subAgentNote" => Ok(json!(task_state::sub_agent_note(&task_state_of(params)?, &user_messages(params)?))),
        // {messages} → 像是在提要求的用户消息（最近 3 条，每条最多 300 字）
        "taskState.requirements" => Ok(json!(task_state::requirement_messages(&user_messages(params)?))),
        // {messages} → 子智能体背景里用户说过的话或 null
        "taskState.userWords" => Ok(json!(task_state::user_words(&user_messages(params)?))),
        // {text} → {requirement, rule}（像在提要求 / 关键信息里的规矩）
        "taskState.classify" => {
            let text = string(params, "text")?;
            Ok(json!({ "requirement": task_state::looks_like_requirement(&text), "rule": task_state::looks_like_rule(&text) }))
        }
        // {path} → {entries, legacyDisabledKinds}（capabilities.json：三组提供方合在一起、旧版本按能力整体关掉的）
        "capabilities.loadConfig" => Ok(capability_store::load_config(std::path::Path::new(&string(params, "path")?))),
        // {path, entries} → null | {failure}（按类型分三组写）
        "capabilities.saveConfig" => {
            Ok(match capability_store::save_config(std::path::Path::new(&string(params, "path")?), &list(params, "entries")?) {
                Ok(()) => Value::Null,
                Err(failure) => json!({ "failure": failure }),
            })
        }
        // {path} → [任务]（jobs.json）
        "capabilities.loadJobs" => Ok(json!(capability_store::load_jobs(std::path::Path::new(&string(params, "path")?)))),
        // {path, jobs} → null | {failure}（只保留最近 50 个）
        "capabilities.saveJobs" => {
            Ok(match capability_store::save_jobs(std::path::Path::new(&string(params, "path")?), &list(params, "jobs")?) {
                Ok(()) => Value::Null,
                Err(failure) => json!({ "failure": failure }),
            })
        }
        // {output} → 远程「FRIDAY-DEPLOY …」那几行解出的报告（含 missingPackages、canInstallLibraries、libraryCommand）
        "deploy.parseReport" => Ok(deploy::parse_report(&string(params, "output")?)),
        // {system, machine} → 包的键（linux-x64…）或空
        "deploy.platformKey" => Ok(json!(deploy::platform_key(&string(params, "system")?, &string(params, "machine")?))),
        // {manifest, channel: stable|beta, key, server?} → 要装的包或 null（latest.json）
        "deploy.fromManifest" => Ok(deploy::from_manifest(
            params.get("manifest").unwrap_or(&Value::Null),
            &string(params, "channel")?,
            &string(params, "key")?,
            flag(params, "server"),
        )),
        // {directory, key, server?, protocol} → 目录里的包或 null
        "deploy.fromDirectory" => Ok(deploy::from_directory(
            std::path::Path::new(&string(params, "directory")?),
            &string(params, "key")?,
            flag(params, "server"),
            params.get("protocol").and_then(Value::as_i64).unwrap_or(0),
        )),
        // {a, b} → -1 / 0 / 1
        "deploy.compareVersions" => Ok(json!(deploy::compare_versions(&string(params, "a")?, &string(params, "b")?))),
        // {version} → 是不是正式的 X.Y.Z
        "deploy.isReleaseVersion" => Ok(json!(deploy::is_release_version(&string(params, "version")?))),
        // {server, package, client, clientIsDevelopment, localPackages} → 服务端该不该升级
        "deploy.shouldUpgrade" => Ok(json!(deploy::should_upgrade(
            &string(params, "server")?,
            &string(params, "package")?,
            &string(params, "client")?,
            flag(params, "clientIsDevelopment"),
            flag(params, "localPackages")
        ))),
        // {report} → {managesInstall, usesServerPackage}
        "deploy.decide" => {
            let report = params.get("report").cloned().unwrap_or(Value::Null);
            Ok(json!({ "managesInstall": deploy::manages_install(&report), "usesServerPackage": deploy::uses_server_package(&report) }))
        }
        // {text} → {version, packageName}（能不能拼进远程命令）
        "deploy.checkText" => {
            let text = string(params, "text")?;
            Ok(json!({ "version": deploy::is_version_text(&text), "packageName": deploy::is_package_name(&text) }))
        }
        // {failure} → 失败原因的说法
        "deploy.describeFailure" => Ok(json!(deploy::describe_failure(&string(params, "failure")?))),
        // {shell: posix|powershell} → 看远程装了什么的命令
        "deploy.inspectCommand" => Ok(json!(deploy::inspect_command(&string(params, "shell")?))),
        // {shell, version, upload, server?, keep?} → 装 / 换上的命令（版本号不对时为空）
        "deploy.deployCommand" => Ok(json!(deploy::deploy_command(
            &string(params, "shell")?,
            &string(params, "version")?,
            flag(params, "upload"),
            flag(params, "server"),
            params.get("keep").and_then(Value::as_str).unwrap_or("")
        ))),
        // {packages} → 用 apt-get 装系统库的命令（包名不对时为空）
        "deploy.installLibrariesCommand" => Ok(json!(deploy::install_libraries_command(&string_list(params, "packages")?))),
        // {id} → 提供方 id 合不合法
        "providers.isValidID" => Ok(json!(provider_store::is_valid_id(&string(params, "id")?))),
        // {manifest} → 补默认值的清单 | {failure}（provider.json）
        "providers.normalize" => Ok(match provider_store::normalize(&object(params, "manifest")?) {
            Ok(manifest) => Value::Object(manifest),
            Err(failure) => json!({ "failure": failure }),
        }),
        // {manifest, builtins, catalog} → 清单说不通的地方或 null
        "providers.problem" => Ok(json!(provider_store::problem(
            &object(params, "manifest")?,
            &string_list(params, "builtins")?,
            &string_list(params, "catalog")?
        ))),
        // {root} → {packages: [{manifest, code, directory}], problems}
        "providers.loadAll" => Ok(provider_store::load_all(std::path::Path::new(&string(params, "root")?))),
        // {directory} → {manifest, code, directory} | {failure}
        "providers.read" => Ok(provider_store::read(std::path::Path::new(&string(params, "directory")?))
            .unwrap_or_else(|failure| json!({ "failure": failure }))),
        // {root, manifest, code} → {manifest, code, directory} | {failure}（存新版本，旧的归档）
        "providers.save" => {
            Ok(provider_store::save(std::path::Path::new(&string(params, "root")?), &object(params, "manifest")?, &string(params, "code")?)
                .unwrap_or_else(|failure| json!({ "failure": failure })))
        }
        // {root, manifest} → null | {failure}（只更新清单）
        "providers.updateManifest" => {
            Ok(match provider_store::update_manifest(std::path::Path::new(&string(params, "root")?), &object(params, "manifest")?) {
                Ok(()) => Value::Null,
                Err(failure) => json!({ "failure": failure }),
            })
        }
        // {folder, sourceName} → {folder} | {failure}（解开的包里 provider.json 在哪一层）
        "providers.locate" => {
            Ok(match provider_store::locate(std::path::Path::new(&string(params, "folder")?), &string(params, "sourceName")?) {
                Ok(folder) => json!({ "folder": tool_files::slash(&folder) }),
                Err(failure) => json!({ "failure": failure }),
            })
        }
        // {root, folder, sourceName, builtins, catalog} → {package, replaced} | {failure}（导入解开的包）
        "providers.install" => Ok(provider_store::install(
            std::path::Path::new(&string(params, "root")?),
            std::path::Path::new(&string(params, "folder")?),
            &string(params, "sourceName")?,
            &string_list(params, "builtins")?,
            &string_list(params, "catalog")?,
        )
        .unwrap_or_else(|failure| json!({ "failure": failure }))),
        // {manifest} → 导出用的 provider.json 文本（不带自测结果）
        "providers.exportManifest" => Ok(json!(provider_store::export_manifest(&object(params, "manifest")?))),
        // {manifest} → 补全默认值的清单或 null（脚本工具的 tool.json）
        "scriptTools.normalize" => Ok(json!(script_tools::normalize_manifest(&object(params, "manifest")?))),
        // {name} → 工具名合不合法
        "scriptTools.isValidName" => Ok(json!(script_tools::is_valid_name(&string(params, "name")?))),
        // {root} → [{manifest, code, directory}]（按名字排）
        "scriptTools.loadAll" => Ok(json!(script_tools::load_all(std::path::Path::new(&string(params, "root")?)))),
        // {directory} → {manifest, code, directory} 或 null
        "scriptTools.load" => Ok(json!(script_tools::load(std::path::Path::new(&string(params, "directory")?)))),
        // {root, manifest, code} → {manifest, code, directory} | {failure}（存新版本，旧的归档）
        "scriptTools.save" => Ok(
            match script_tools::save(std::path::Path::new(&string(params, "root")?), &object(params, "manifest")?, &string(params, "code")?)
            {
                Ok(tool) => tool,
                Err(failure) => json!({ "failure": failure }),
            },
        ),
        // {root, manifest} → null | {failure}（只更新清单）
        "scriptTools.updateManifest" => {
            Ok(match script_tools::update_manifest(std::path::Path::new(&string(params, "root")?), &object(params, "manifest")?) {
                Ok(()) => Value::Null,
                Err(failure) => json!({ "failure": failure }),
            })
        }
        // {root, name} → [{manifest, code}]（新的在前）
        "scriptTools.versions" => {
            Ok(json!(script_tools::versions(std::path::Path::new(&string(params, "root")?), &string(params, "name")?)))
        }
        // {exitCode, stdout, stderr, timedOut, timeout} → 运行摘要
        "scriptTools.summary" => Ok(json!(script_tools::run_summary(
            params.get("exitCode").and_then(Value::as_i64).unwrap_or(0),
            &string(params, "stdout")?,
            &string(params, "stderr")?,
            flag(params, "timedOut"),
            params.get("timeout").and_then(Value::as_f64).unwrap_or(60.0)
        ))),
        // {stdout, workDirectory, home} → 脚本声明的产出文件（存在的）
        "scriptTools.mediaPaths" => Ok(json!(script_tools::media_paths(
            &string(params, "stdout")?,
            std::path::Path::new(&string(params, "workDirectory")?),
            std::path::Path::new(&string(params, "home")?)
        ))),
        // {name, command?, path?, alwaysConfirm, paid, mode: auto|plan|full, permissionNeedsApproval, stopsSystem, browserStop, powerShell?}
        //   → {needed, forced}（一个调用要不要先问用户；powerShell 不传按这台是不是 Windows）
        "approval.decide" => {
            let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_string);
            let (name, command, path, mode) =
                (string(params, "name")?, text("command"), text("path"), text("mode").unwrap_or_else(|| "auto".into()));
            let request = approval::Request {
                name: &name,
                command: command.as_deref(),
                path: path.as_deref(),
                always_confirm: flag(params, "alwaysConfirm"),
                paid: flag(params, "paid"),
                mode: &mode,
                permission_needs_approval: flag(params, "permissionNeedsApproval"),
                stops_system: flag(params, "stopsSystem"),
                browser_stop: flag(params, "browserStop"),
                power_shell: params.get("powerShell").and_then(Value::as_bool).unwrap_or(cfg!(windows)),
            };
            let (needed, forced) = approval::decide(&request);
            Ok(json!({ "needed": needed, "forced": forced }))
        }
        // {name, command?, path?, powerShell?} → {destructive, systemChange}；{command} 也给 {systemCommand}，{path} 给 {systemPath}
        "approval.classify" => {
            let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_string);
            let (name, command, path) = (text("name").unwrap_or_default(), text("command"), text("path"));
            let power_shell = params.get("powerShell").and_then(Value::as_bool).unwrap_or(cfg!(windows));
            Ok(json!({
                "destructive": approval::is_destructive(&name, command.as_deref(), power_shell),
                "destructiveCommand": command.as_deref().is_some_and(|command| approval::is_destructive_command(command, power_shell)),
                "systemChange": approval::is_system_change(&name, command.as_deref(), path.as_deref()),
                "systemCommand": command.as_deref().is_some_and(approval::is_system_command),
                "systemPath": path.as_deref().is_some_and(approval::is_system_path),
            }))
        }
        // {items: [{role, task, engine}], attempts: [{id, name, role, task, engine}], running: [{id, name, role, assignment}], single?, own?}
        //   （single、own：只派一个下级时它的任务和调用方自己的任务）
        //   → 不通过时给模型看的说明或 null（派发前的检查）
        "team.checkDispatch" => Ok(json!(team::check_from_json(params))),
        // {from?} → 交给子智能体的补充消息前面的说明
        "team.messageNote" => Ok(json!(team::message_note(params.get("from").and_then(Value::as_str)))),
        // {kind, name, id, text} → 汇报 / 完成 / 失败 / 被停 / 工作流结束交给父级的文字
        "team.noticeText" => Ok(json!(team::notice_text(
            &string(params, "kind")?,
            &string(params, "name")?,
            &string(params, "id")?,
            &string(params, "text")?
        ))),
        // {chain: [{giver?, name, assignment}], me} → 「你在整个任务里的位置」或 null
        "team.ancestryNote" => Ok(json!(team::ancestry_note(&list(params, "chain")?, &string(params, "me")?))),
        // {running: [{id, name}], waiting: [{id, name}]} → 现在有哪些子智能体
        "team.childrenSummary" => Ok(json!(team::children_summary(&list(params, "running")?, &list(params, "waiting")?))),
        // {stoppedBy?, message, partial?} → 被停掉 / 失败时卡片上、交给父级的说明
        "agents.endedText" => {
            let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_string);
            Ok(json!(liveness::ended_text(text("stoppedBy").as_deref(), &text("message").unwrap_or_default(), text("partial").as_deref())))
        }
        // {cardText} → 给父级的建议
        "agents.advice" => Ok(json!(liveness::advice(&string(params, "cardText")?))),
        // {seconds} → N 秒 / N 分 M 秒 / N 小时 M 分
        "agents.duration" => Ok(json!(liveness::duration(params.get("seconds").and_then(Value::as_f64).unwrap_or(0.0)))),
        // {now?, stallLimit, grace?, runs: [{id, name, deadline?, warned, generating, lastOutput?}]} → [{id, action: stop|warn, by?, text?, journal?}]
        "agents.watchdog" => {
            let number = |value: &Value, key: &str| value.get(key).and_then(Value::as_f64);
            let runs: Vec<liveness::Watched> = list(params, "runs")?
                .iter()
                .map(|run| liveness::Watched {
                    id: run.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                    name: run.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                    deadline: number(run, "deadline"),
                    warned: run.get("warned").and_then(Value::as_bool).unwrap_or(false),
                    generating: run.get("generating").and_then(Value::as_bool).unwrap_or(false),
                    last_output: number(run, "lastOutput"),
                })
                .collect();
            let stall = params.get("stallLimit").and_then(Value::as_f64).unwrap_or(300.0);
            let grace = params.get("grace").and_then(Value::as_f64).unwrap_or(liveness::DEADLINE_GRACE);
            Ok(json!(liveness::watchdog(now_ms(params), stall, grace, &runs)))
        }
        // {seconds?, inherited?, now?} → 下级的期限（Unix 毫秒）或 null
        "agents.childDeadline" => Ok(json!(liveness::child_deadline(
            params.get("seconds").and_then(Value::as_f64),
            params.get("inherited").and_then(Value::as_f64),
            now_ms(params)
        ))),
        // {name, arguments, depth, last?: {signature, count}} → {count, signature}（同样的调用连续第几次；轮询类工具是 0）
        "agents.repeat" => {
            let last = params.get("last").and_then(|last| Some((last.get("signature")?.as_str()?, last.get("count")?.as_i64()?)));
            let depth = params.get("depth").and_then(Value::as_i64).unwrap_or(0);
            let (count, signature) = liveness::repeat_count(&string(params, "name")?, &string(params, "arguments")?, depth, last);
            Ok(json!({ "count": count, "signature": signature }))
        }
        // {text, reports} → 并进中途汇报的结果
        "agents.withReports" => Ok(json!(liveness::with_reports(&string(params, "text")?, &string_list(params, "reports")?))),
        // {id, blocks, finalText, directory} → 做到一半的结果（长的全文存进 directory）或 null
        "agents.partial" => {
            let blocks: Vec<crate::chat_models::MessageBlock> = serde_json::from_value(Value::Array(list(params, "blocks")?))
                .map_err(|error| ApiError::invalid(format!("blocks 解不出来：{error}")))?;
            let directory = std::path::PathBuf::from(string(params, "directory")?);
            Ok(json!(liveness::partial_result(&string(params, "id")?, &blocks, &string(params, "finalText")?, &directory)))
        }
        // {text, id, directory} → 交给上级的正文（太长的存全文、只交开头结尾和路径）
        "agents.clipResult" => {
            let directory = std::path::PathBuf::from(string(params, "directory")?);
            Ok(json!(liveness::clip_result(&string(params, "text")?, &string(params, "id")?, &directory)))
        }
        // {args} → {goal, steps: [{key, spec, after, check, arguments}]} | {failure}（run_workflow 的计划）
        "workflow.plan" => Ok(match agents::workflow_plan(&object(params, "args")?) {
            Ok(plan) => plan,
            Err(failure) => json!({ "failure": failure }),
        }),
        // {steps: [{key, name, after}], outcomes: [done|failed|skipped|null], started: [index], stopped} → {skip: [{index, reason}], launch, finished}
        "workflow.advance" => {
            let steps = list(params, "steps")?;
            let field = |step: &Value, key: &str| step.get(key).and_then(Value::as_str).unwrap_or("").to_string();
            let keys: Vec<String> = steps.iter().map(|step| field(step, "key")).collect();
            let names: Vec<String> = steps.iter().map(|step| field(step, "name")).collect();
            let after: Vec<Vec<String>> = steps
                .iter()
                .map(|step| {
                    step.get("after")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .collect();
            let given = list(params, "outcomes")?;
            let mut outcomes: Vec<Option<agents::Outcome>> =
                (0..steps.len()).map(|index| given.get(index).and_then(Value::as_str).and_then(agents::Outcome::parse)).collect();
            let started: HashSet<usize> = list(params, "started")?.iter().filter_map(Value::as_u64).map(|index| index as usize).collect();
            Ok(agents::workflow_advance(&keys, &names, &after, &mut outcomes, &started, flag(params, "stopped")))
        }
        // {id, steps: [{key, name, card, after}], override?} → 交给主智能体的「已启动」
        "workflow.startedText" => Ok(json!(agents::workflow_started(&string(params, "id")?, &list(params, "steps")?, override_id(params)))),
        // {goal, task, check, upstream: [{key, name, result}], override?} → 一步的任务
        "workflow.stepTask" => Ok(json!(agents::workflow_step_task(
            &string(params, "goal")?,
            &string(params, "task")?,
            &string(params, "check")?,
            &list(params, "upstream")?,
            override_id(params)
        ))),
        // {goal, steps: [{key, name, card, outcome, check, result}], override?} → 交回主智能体的验收材料
        "workflow.report" => Ok(json!(agents::workflow_report(&string(params, "goal")?, &list(params, "steps")?, override_id(params)))),
        // {done, total} → 工作流卡片定稿时的一句
        "workflow.summary" => {
            let count = |key: &str| params.get(key).and_then(Value::as_u64).unwrap_or(0) as usize;
            Ok(json!(agents::workflow_summary(count("done"), count("total"))))
        }

        // {name?, version?} → {client}（MCP 客户端；clientInfo 默认 Friday / 1.0）。事件：mcpClient.stderr {client, text}、
        //   mcpClient.exited {client, reason}（进程自己退出）、mcpClient.httpPost {client, post, url, headers: [[名, 值]], body}、
        //   mcpClient.httpAbort {client, post}。下面阻塞的方法结果：成功的内容，或 {failure, cancelled?}
        "mcpClient.open" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("Friday");
            let version = params.get("version").and_then(Value::as_str).unwrap_or("1.0");
            Ok(json!({ "client": mcp_client::open(name, version) }))
        }
        // {client} → null（断开并丢掉句柄）
        "mcpClient.close" => {
            mcp_client::close(handle(params, "client")?);
            Ok(Value::Null)
        }
        // {client, program, arguments, environment, workingDirectory?, timeoutMs, call?} → {ok} | {failure}（起 stdio server 并握手）
        "mcpClient.connectStdio" => {
            let client = mcp_client_of(params)?;
            let environment = string_map(params, "environment")?;
            let directory = params.get("workingDirectory").and_then(Value::as_str).unwrap_or("").to_string();
            let outcome = client.connect_stdio(
                &string(params, "program")?,
                &string_list(params, "arguments")?,
                &environment,
                &directory,
                timeout_ms(params),
                call_id(params),
            );
            Ok(mcp_outcome(outcome.map(|_| json!({ "ok": true }))))
        }
        // {client, url, headers: {名: 值}, timeoutMs, call?} → {ok} | {failure}（连 Streamable HTTP server 并握手）
        "mcpClient.connectHttp" => {
            let client = mcp_client_of(params)?;
            let mut headers: Vec<(String, String)> = string_map(params, "headers")?.into_iter().collect();
            headers.sort();
            let outcome = client.connect_http(&string(params, "url")?, headers, timeout_ms(params), call_id(params));
            Ok(mcp_outcome(outcome.map(|_| json!({ "ok": true }))))
        }
        // {client, cursor?, timeoutMs, call?} → {tools: [{name, title?, description?, inputSchema, annotationsTitle?, readOnlyHint?}], nextCursor?}
        "mcpClient.listTools" => {
            let client = mcp_client_of(params)?;
            let cursor = params.get("cursor").and_then(Value::as_str).map(str::to_string);
            Ok(mcp_outcome(client.list_tools(cursor.as_deref(), timeout_ms(params), call_id(params))))
        }
        // {client, name, arguments, timeoutMs, call?} → {content: [{kind, text? | data + mimeType? | uri, name?, resourceText?}], isError?}
        "mcpClient.callTool" => {
            let client = mcp_client_of(params)?;
            let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            Ok(mcp_outcome(client.call_tool(&string(params, "name")?, arguments, timeout_ms(params), call_id(params))))
        }
        // {client, call} → null（这次调用马上以「已取消」结束）
        "mcpClient.cancel" => {
            if let Some(client) = mcp_client::get(handle(params, "client")?) {
                client.cancel(handle(params, "call")?);
            }
            Ok(Value::Null)
        }
        // {client} → null（断开、结束进程，句柄留着）
        "mcpClient.disconnect" => {
            if let Some(client) = mcp_client::get(handle(params, "client")?) {
                client.disconnect();
            }
            Ok(Value::Null)
        }
        // {client} → {connected, processRunning, pid?}
        "mcpClient.status" => {
            let client = mcp_client_of(params)?;
            let mut status = json!({ "connected": client.is_connected(), "processRunning": client.process_running() });
            if let Some(pid) = client.pid() {
                status["pid"] = json!(pid);
            }
            Ok(status)
        }
        // 两端发 HTTP：{client, post, status, contentType, sessionId} → null（响应头）
        "mcpClient.httpHead" => {
            if let Some(client) = mcp_client::get(handle(params, "client")?) {
                let status = params.get("status").and_then(Value::as_i64).unwrap_or(0);
                let content_type = params.get("contentType").and_then(Value::as_str).unwrap_or("");
                let session = params.get("sessionId").and_then(Value::as_str).unwrap_or("");
                client.http_head(handle(params, "post")?, status, content_type, session);
            }
            Ok(Value::Null)
        }
        // {client, post, data（base64）} → null（一块响应体）
        "mcpClient.httpData" => {
            if let Some(client) = mcp_client::get(handle(params, "client")?) {
                let data = mcp_client::decode_base64(&string(params, "data")?).ok_or_else(|| ApiError::invalid("data 不是 base64"))?;
                client.http_data(handle(params, "post")?, &data);
            }
            Ok(Value::Null)
        }
        // {client, post, error?, aborted?} → null（结束：error 是网络层错误；aborted 是 httpAbort 之后断开的）
        "mcpClient.httpEnd" => {
            if let Some(client) = mcp_client::get(handle(params, "client")?) {
                let error = params.get("error").and_then(Value::as_str).filter(|error| !error.is_empty());
                client.http_end(handle(params, "post")?, error, flag(params, "aborted"));
            }
            Ok(Value::Null)
        }
        // {tool} → 宽松解出的工具（同 mcpClient.listTools 里的）
        "mcp.normalizeTool" => Ok(mcp_client::normalize_tool(&Value::Object(object(params, "tool")?))),
        // {content} → 解出的一段内容，解不出来是 {failure}（同 mcpClient.callTool 里的严格解码）
        "mcp.decodeContent" => Ok(mcp_client::decode_one(params.get("content").unwrap_or(&Value::Null))
            .unwrap_or_else(|| json!({ "failure": mcp_client::TYPE_MISMATCH }))),
        // {current: [{name, config}], next: {名字: config}, force?} → {detach, keep, connect, disabled}（配置变了哪些要重连）
        "mcp.syncPlan" => {
            let current: Vec<(String, Value)> = list(params, "current")?
                .into_iter()
                .map(|item| {
                    (item.get("name").and_then(Value::as_str).unwrap_or("").to_string(), item.get("config").cloned().unwrap_or(Value::Null))
                })
                .collect();
            let force = if params.contains_key("force") { string_list(params, "force")? } else { Vec::new() };
            Ok(mcp_client::sync_plan(&current, &object(params, "next")?, &force))
        }

        // {path, teamTools} → {archive, conversations}（同 conversations.load，核心留一份存档，之后落盘只交改过的会话；
        //   path 为空：两端自己读的 / 测试用的，conversations 给初始内容，不读文件）
        "archive.open" => {
            let path = std::path::PathBuf::from(string(params, "path")?);
            if params.contains_key("conversations") {
                let items = conversations_of(params, "conversations")?;
                return Ok(json!({ "archive": archive::open_with(path, items) }));
            }
            let (id, loaded) = archive::open(path, &string_list(params, "teamTools")?);
            Ok(json!({ "archive": id, "conversations": to_value(loaded)? }))
        }
        // {archive} → null
        "archive.close" => {
            archive::close(handle(params, "archive")?);
            Ok(Value::Null)
        }
        // {archive, seq, order, changed, encodeSeconds?} → {interval, missing}（后台写盘；seq 递增，旧的不覆盖新的；
        //   interval：生成过程中下一次强制落盘的间隔（秒）；missing：顺序里有、核心还没有内容的会话 id）
        "archive.save" => {
            let id = handle(params, "archive")?;
            let seq = params.get("seq").and_then(Value::as_u64).unwrap_or(0);
            let encode = params.get("encodeSeconds").and_then(Value::as_f64).unwrap_or(0.0);
            let changed = conversations_of(params, "changed")?;
            let (interval, missing) = archive::save(id, seq, string_list(params, "order")?, changed, encode)
                .ok_or_else(|| ApiError::invalid(format!("没有这个存档：{id}")))?;
            Ok(json!({ "interval": interval, "missing": missing }))
        }
        // {conversation, through, id?} → Conversation（从 through 这条消息处 fork 出的新会话：当前路径到它为止、消息换新 id、
        //   标题加「（fork）」，大脑 / 目录 / 权限 / 深度思考 / 任务模式照旧；id：指定新会话的 id）
        "conversations.fork" => {
            let source: Conversation = serde_json::from_value(Value::Object(object(params, "conversation")?))
                .map_err(|error| ApiError::invalid(format!("conversation 解不出来：{error}")))?;
            let new_id = params.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()).map(str::to_string);
            to_value(archive::fork(&source, &string(params, "through")?, new_id))
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
        // {model, instructions?, history, temperature, maxTokens?, tools?, baseURL, thinking, enableThinking, reasoningEffort?, extraBody?,
        // engineID?, xFriday?, mask?} → 请求体（tools：要带的工具定义，收尾请求、不支持工具的传空；thinking：ThinkingSupport 的原始值；
        // mask：按密钥库遮一遍）
        "chat.requestBody" => {
            let empty = Vec::new();
            let array = |key: &str| match params.get(key) {
                None | Some(Value::Null) => Ok(&empty),
                Some(Value::Array(items)) => Ok(items),
                Some(_) => Err(ApiError::invalid(format!("{key} 要是数组"))),
            };
            let flag = |key: &str| params.get(key).and_then(Value::as_bool).unwrap_or(false);
            let options = chat_request::BodyOptions {
                model: string(params, "model")?,
                instructions: string(params, "instructions")?,
                history: array("history")?,
                temperature: params.get("temperature").and_then(Value::as_f64).unwrap_or(0.6),
                max_tokens: params.get("maxTokens").and_then(Value::as_i64),
                tools: array("tools")?,
                base_url: string(params, "baseURL")?,
                thinking: string(params, "thinking")?,
                enable_thinking: flag("enableThinking"),
                reasoning_effort: params.get("reasoningEffort").and_then(Value::as_str).map(str::to_string),
                extra_body: string(params, "extraBody")?,
                engine_id: string(params, "engineID")?,
                x_friday: params.get("xFriday").filter(|value| !value.is_null()).cloned(),
            };
            let body = chat_request::request_body(&options);
            Ok(if flag("mask") { chat_request::masked(body) } else { body })
        }
        // {status, text} → [接口说不接受的可选参数]
        "chat.unsupportedParameters" => {
            Ok(Value::from(chat_request::unsupported_parameters(int(params, "status")?, &string(params, "text")?)))
        }
        // {engineID} → [这个引擎说过不接受的]
        "chat.unsupportedFor" => Ok(Value::from(chat_request::unsupported_for(&string(params, "engineID")?))),
        // {engineID, names} → null（记住，本次运行内之后的请求都不带）
        "chat.rememberUnsupported" => {
            chat_request::remember_unsupported(&string(params, "engineID")?, &string_list(params, "names")?);
            Ok(Value::Null)
        }
        // {attempt, retryAfter?} → 秒（服务繁忙、连不上时第 attempt 次重试前；retryAfter：秒数或 Retry-After 头的原文）
        "chat.retryDelay" => Ok(Value::from(chat_request::retry_delay(int(params, "attempt")?, retry_after(params)))),
        // {attempt, retryAfter?, jitterMs?} → 秒（可重试的 429）
        "chat.rateLimitDelay" => {
            let jitter = params.get("jitterMs").and_then(Value::as_i64).unwrap_or(0);
            Ok(Value::from(chat_request::rate_limit_delay(int(params, "attempt")?, retry_after(params), jitter)))
        }
        // {status, body} → {promptTokens, contextTokens} | null（是不是上下文超长；数字拿不到的是 0）
        "chat.contextOverflow" => Ok(chat_request::context_overflow(int(params, "status")?, &string(params, "body")?)
            .map_or(Value::Null, |(prompt, context)| json!({ "promptTokens": prompt, "contextTokens": context }))),
        // 请求并发闸门（按引擎 id 共用）：排着的请求由两端编号、自己等，这里说放行哪几个（admitted）。note：并发上限变了，记进日志
        // {engineID, initial?} → null（第一次建按 initial，之后不再起作用）
        "gate.create" => {
            let initial = params.get("initial").and_then(Value::as_i64);
            gate::with(&string(params, "engineID")?, initial, |_| ());
            Ok(Value::Null)
        }
        // {engineID} → 拿到名额没有（没人排队、还有名额才拿到）
        "gate.tryAcquire" => Ok(Value::from(gate::with(&string(params, "engineID")?, None, |gate| gate.try_acquire()))),
        // {engineID, id, priority} → [admitted]（排队；可能马上就放行它自己）
        "gate.enqueue" => {
            let (id, priority) = (handle(params, "id")?, int(params, "priority")?);
            Ok(Value::from(gate::with(&string(params, "engineID")?, None, |gate| gate.enqueue(id, priority))))
        }
        // {engineID, id, refund?} → {removed, admitted}（不排了；refund：已经放行却不要了，退回名额）
        "gate.cancel" => {
            let (id, refund) = (handle(params, "id")?, params.get("refund").and_then(Value::as_bool).unwrap_or(false));
            let (removed, admitted) = gate::with(&string(params, "engineID")?, None, |gate| gate.cancel(id, refund));
            Ok(json!({ "removed": removed, "admitted": admitted }))
        }
        // {engineID, outcome: ok | rateLimited | failed} → {admitted, note?}
        "gate.release" => {
            let outcome = match string(params, "outcome")?.as_str() {
                "ok" => gate::Outcome::Ok,
                "rateLimited" => gate::Outcome::RateLimited,
                "failed" => gate::Outcome::Failed,
                other => return Err(ApiError::invalid(format!("outcome 要是 ok / rateLimited / failed：{other}"))),
            };
            let change = gate::with(&string(params, "engineID")?, None, |gate| gate.release(outcome, std::time::Instant::now()));
            Ok(json!({ "admitted": change.admitted, "note": change.note }))
        }
        // {engineID, ceiling} → {admitted, note?}（本地大模型报它同时能算几个）
        "gate.follow" => {
            let ceiling = int(params, "ceiling")?;
            let change = gate::with(&string(params, "engineID")?, None, |gate| gate.follow(ceiling, std::time::Instant::now()));
            Ok(json!({ "admitted": change.admitted, "note": change.note }))
        }
        // {engineID} → {limit, inFlight, waiting}
        "gate.snapshot" => {
            let (limit, in_flight, waiting) = gate::with(&string(params, "engineID")?, None, |gate| gate.snapshot());
            Ok(json!({ "limit": limit, "inFlight": in_flight, "waiting": waiting }))
        }
        // {} → null（测试用：清掉全部闸门）
        "gate.reset" => {
            gate::reset();
            Ok(Value::Null)
        }

        // {limit} → {reserve, threshold, target}（按上下文上限压历史：超过 threshold 才压，一压压到 target）
        "context.budget" => {
            let limit = int(params, "limit")?;
            Ok(json!({ "reserve": context::reserve(limit), "threshold": context::threshold(limit), "target": context::target(limit) }))
        }
        // {history, folded, budget, summaries?: {echoesReasoning, taskStateTool, wholeTools}}
        //   → {shortened, dropped, before, after, folded, history?}（压了才带 history；summaries 不给用默认的摘要）
        "context.compact" => {
            let mut history = list(params, "history")?;
            let mut folded = usize::try_from(int(params, "folded")?).map_err(|_| ApiError::invalid("folded 不能是负数"))?;
            let summaries = match params.get("summaries") {
                Some(Value::Object(summaries)) => Some(context::Summaries {
                    echoes_reasoning: summaries.get("echoesReasoning").and_then(Value::as_bool).unwrap_or(false),
                    special: special(summaries)?,
                }),
                _ => None,
            };
            let result = context::compact(&mut history, &mut folded, int(params, "budget")?, summaries.as_ref());
            let mut out = json!({
                "shortened": result.shortened, "dropped": result.dropped, "before": result.before, "after": result.after, "folded": folded,
            });
            if result.shortened > 0 || result.dropped > 0 {
                out["history"] = Value::Array(history);
            }
            Ok(out)
        }
        // {count} → 整段去掉的地方留的说明
        "context.omittedNote" => Ok(Value::from(context::omitted_note(int(params, "count")?.max(0) as usize))),
        // {key} → 上限 | null；{key, limit} → null（key：引擎 id|模型；本地大模型问到的、请求超长时报的）
        "context.knownLimit" => Ok(context::known_limit(&string(params, "key")?).map_or(Value::Null, Value::from)),
        "context.rememberLimit" => {
            context::remember_limit(&string(params, "key")?, int(params, "limit")?);
            Ok(Value::Null)
        }
        // {key} → 估算的校准系数（没量过是 1）；{key, ratio} → null（记成 0.5–4）
        "context.ratio" => Ok(Value::from(context::ratio(&string(params, "key")?))),
        "context.rememberRatio" => {
            context::remember_ratio(&string(params, "key")?, params.get("ratio").and_then(Value::as_f64).unwrap_or(1.0));
            Ok(Value::Null)
        }
        // {baseURL, model} → 服务商报的上限 | null；{baseURL, entries: [{id, contextLength?}]} → null
        "context.providerLimit" => {
            Ok(context::provider_limit(&string(params, "baseURL")?, &string(params, "model")?).map_or(Value::Null, Value::from))
        }
        "context.rememberProvider" => {
            let entries: Vec<(String, i64)> = list(params, "entries")?
                .iter()
                .filter_map(|entry| Some((entry.get("id")?.as_str()?.to_string(), entry.get("contextLength")?.as_i64()?)))
                .collect();
            context::remember_provider(&string(params, "baseURL")?, &entries);
            Ok(Value::Null)
        }
        // {engineID, baseURL, model, fallback} → {tokens, guessed}（fallback：设置里的最大上下文长度）
        "context.limit" => {
            let (tokens, guessed) = context::limit(
                &string(params, "engineID")?,
                &string(params, "baseURL")?,
                &string(params, "model")?,
                int(params, "fallback")?,
            );
            Ok(json!({ "tokens": tokens, "guessed": guessed }))
        }
        // {instructions?, toolBytes, history} → {system, tools, user, replies, toolUse}（按字数估算）
        "context.parts" => {
            let tool_bytes = params.get("toolBytes").and_then(Value::as_i64).unwrap_or(0);
            Ok(context::Parts::estimate(&string(params, "instructions")?, tool_bytes, &list(params, "history")?).to_json())
        }
        // {parts, total} → parts（等比例缩放到合计 total）
        "context.scaleParts" => Ok(context::Parts::from_json(&object(params, "parts")?).scaled(int(params, "total")?).to_json()),
        // {tokens} → 「1.2K」「131K」「1M」
        "context.compactNumber" => Ok(Value::from(context::compact_number(int(params, "tokens")?))),
        // {fit?, provider?} → null（测试用：fit 忘掉记住的上限和校准，provider 忘掉服务商报的；都不给就都忘）
        "context.reset" => {
            let (fit, provider) = (params.get("fit").and_then(Value::as_bool), params.get("provider").and_then(Value::as_bool));
            let both = fit.is_none() && provider.is_none();
            context::reset(both || fit == Some(true), both || provider == Some(true));
            Ok(Value::Null)
        }

        // 内置文件工具（路径由两端解析好传进来）。结果 {output, lineDiff?: {added, removed}}；失败 {failure: {kind: invalid | failed, message}}
        // {path, workingDirectory?}
        "files.list" => tool_output(tool_files::list_directory(&string(params, "path")?, &string(params, "workingDirectory")?)),
        // {path, offset?, limit?, workingDirectory?}（文字；图片、视频、音频两端先认出来交给界面）
        "files.read" => tool_output(tool_files::read_file(
            &string(params, "path")?,
            optional_int(params, "offset"),
            optional_int(params, "limit"),
            &string(params, "workingDirectory")?,
        )),
        // {path, content}
        "files.write" => tool_output(tool_files::write_file(&string(params, "path")?, &string(params, "content")?)),
        // {path, workingDirectory?, edits?: [{old_text, new_text, replace_all?}] | oldText, newText, replaceAll?}（edits 给了就按几处改）
        "files.edit" => {
            let change = edit_change(params)?;
            tool_output(tool_files::edit_file(&string(params, "path")?, &change, &string(params, "workingDirectory")?))
        }
        // {pattern, root, workingDirectory?}
        "files.find" => text_output(file_search::find_files(
            &string(params, "pattern")?,
            &string(params, "root")?,
            &string(params, "workingDirectory")?,
        )),
        // {pattern, root, glob?, ignoreCase?, workingDirectory?}
        "files.search" => {
            let glob = params.get("glob").and_then(Value::as_str);
            text_output(file_search::search_files(
                &string(params, "pattern")?,
                &string(params, "root")?,
                glob,
                flag(params, "ignoreCase"),
                &string(params, "workingDirectory")?,
            ))
        }
        // {keywords, root, glob?, matchCase?, matchAny?, workingDirectory?}
        "files.keywords" => {
            let glob = params.get("glob").and_then(Value::as_str);
            text_output(file_search::search_keywords(
                &string(params, "keywords")?,
                &string(params, "root")?,
                glob,
                flag(params, "matchCase"),
                flag(params, "matchAny"),
                &string(params, "workingDirectory")?,
            ))
        }
        // {name} → 遍历时跳不跳过这个目录（隐藏的、依赖和构建目录）
        "files.skipsDirectory" => Ok(Value::from(file_search::skips_directory(&string(params, "name")?))),
        // {text, matchCase?} → [关键词]（双引号括起来的算一个词，去掉重复的）
        "files.splitKeywords" => Ok(Value::from(file_search::split_keywords(&string(params, "text")?, flag(params, "matchCase")))),
        // {text, offset?, limit?, maxCharacters?} → 按行读的结果
        "files.readLines" => Ok(Value::from(file_search::read_lines(
            &string(params, "text")?,
            optional_int(params, "offset"),
            optional_int(params, "limit"),
            optional_int(params, "maxCharacters").map_or(tool_files::MAX_OUTPUT, |max| max.max(1) as usize),
        ))),
        // {path, workingDirectory?, wantDirectory} → 路径不对时的提示（以换行开头，没有为空）
        "files.pathHint" => Ok(Value::from(tool_files::path_hint(
            &string(params, "path")?,
            &string(params, "workingDirectory")?,
            flag(params, "wantDirectory"),
        ))),
        // {path} → 读失败的说明
        "files.readFailure" => Ok(Value::from(tool_files::read_failure(&string(params, "path")?))),
        // {path, workingDirectory?} → 给模型看的路径（在工作目录下写相对路径）
        "files.displayPath" => Ok(Value::from(file_search::display_path(&string(params, "path")?, &string(params, "workingDirectory")?))),
        // {glob, relativePath} → 通配对不对得上（写错了为 null）
        "files.globMatches" => {
            let (glob, relative) = (string(params, "glob")?, string(params, "relativePath")?);
            Ok(file_search::glob_expression(&glob)
                .map_or(Value::Null, |expression| Value::from(file_search::matches(&expression, &glob, &relative))))
        }
        // {text} → 太长的留头尾；{text, offset?} → 分页；{html} → 去掉标签的文字；{bytes} → 「12 KB」
        "tools.truncate" => Ok(Value::from(tool_files::truncate(&string(params, "text")?))),
        "tools.page" => Ok(Value::from(tool_files::page(&string(params, "text")?, optional_int(params, "offset")))),
        "tools.stripHTML" => Ok(Value::from(tool_files::strip_html(&string(params, "html")?))),
        "tools.byteCount" => Ok(Value::from(tool_files::byte_count(int(params, "bytes")?))),

        // {text, oldText, newText, replaceAll?} → {text, count, firstLine, lastLine, inserted, replacedAt, removedLength, note, summary?}
        //   | {failure: {kind: invalid, message}}（path 给了就带 summary）
        "edit.apply" => {
            match file_edit::apply(
                &string(params, "text")?,
                &string(params, "oldText")?,
                &string(params, "newText")?,
                flag(params, "replaceAll"),
            ) {
                Ok(edited) => {
                    let path = string(params, "path")?;
                    let summary = (!path.is_empty()).then(|| file_edit::summary(&path, &edited));
                    Ok(json!({
                        "text": edited.text, "count": edited.count, "firstLine": edited.first_line, "lastLine": edited.last_line,
                        "inserted": edited.inserted, "replacedAt": edited.replaced_at, "removedLength": edited.removed_length,
                        "note": edited.note, "summary": summary,
                    }))
                }
                Err(message) => Ok(json!({ "failure": { "kind": "invalid", "message": message } })),
            }
        }
        // {text, edits: [{old_text, new_text, replace_all?}], path?} → {text, count, inserted, starts, notes, summary?} | {failure}
        "edit.applyAll" => {
            let edits = edits(params)?.unwrap_or_default();
            match file_edit::apply_all(&string(params, "text")?, &edits) {
                Ok(result) => {
                    let path = string(params, "path")?;
                    let summary = (!path.is_empty()).then(|| file_edit::summary_all(&path, &result));
                    Ok(json!({
                        "text": result.text, "count": result.count, "inserted": result.inserted, "starts": result.starts,
                        "notes": result.notes, "summary": summary,
                    }))
                }
                Err(message) => Ok(json!({ "failure": { "kind": "invalid", "message": message } })),
            }
        }

        // {path, text, count, firstLine, lastLine, note?} → 「已修改 <path>：替换了 N 处……」+ 改完的几行（edit.apply 的结果）
        "edit.summary" => {
            let edited = file_edit::Edited {
                text: string(params, "text")?,
                count: int(params, "count")?.max(0) as usize,
                first_line: int(params, "firstLine")?.max(1) as usize,
                last_line: int(params, "lastLine")?.max(1) as usize,
                inserted: String::new(),
                replaced_at: Vec::new(),
                removed_length: 0,
                note: string(params, "note")?,
            };
            Ok(Value::from(file_edit::summary(&string(params, "path")?, &edited)))
        }
        // {path, text, count, inserted, starts, notes?} → 「已修改 <path>：N 处修改都改好了……」（edit.applyAll 的结果）
        "edit.summaryAll" => {
            let starts = list(params, "starts")?.iter().map(|start| start.as_u64().unwrap_or(0) as usize).collect();
            let result = file_edit::MultiEdited {
                text: string(params, "text")?,
                count: int(params, "count")?.max(0) as usize,
                inserted: string_list(params, "inserted")?,
                starts,
                notes: string_list(params, "notes")?,
            };
            if result.starts.len() != result.inserted.len() {
                return Err(ApiError::invalid("starts 和 inserted 要一样多"));
            }
            Ok(Value::from(file_edit::summary_all(&string(params, "path")?, &result)))
        }

        // 命令前后看改了哪些文件。{command, workingDirectory?, home?, currentDirectory?} → {changes}（执行前看一遍）
        "changes.before" => {
            let snapshot = file_changes::before(
                &string(params, "command")?,
                &string(params, "workingDirectory")?,
                &string(params, "home")?,
                &string(params, "currentDirectory")?,
            );
            Ok(json!({ "changes": file_changes::open(snapshot) }))
        }
        // {changes} → {lineDiff: {added, removed} | null, changed: [{path, kind: created | modified | deleted, before: [指纹] | null}]}
        //   （执行后比一遍，句柄随之关掉；指纹是 16 位十六进制）
        "changes.after" => {
            let id = handle(params, "changes")?;
            let snapshot = file_changes::take(id).ok_or_else(|| ApiError::invalid(format!("没有这个句柄：{id}（比过了？）")))?;
            let (counts, changed) = file_changes::after(&snapshot);
            let changed: Vec<Value> = changed
                .into_iter()
                .map(|change| {
                    json!({
                        "path": change.path,
                        "kind": change.kind.name(),
                        "before": change.before.map(|lines| lines.into_iter().map(line_diff::hex).collect::<Vec<_>>()),
                    })
                })
                .collect();
            Ok(json!({
                "lineDiff": counts.map(|counts| json!({ "added": counts.added, "removed": counts.removed })),
                "changed": changed,
            }))
        }
        // {changes} → null（命令没跑成，不比了）
        "changes.discard" => {
            file_changes::take(handle(params, "changes")?);
            Ok(Value::Null)
        }
        // {command, base, home?} → [命令里写到的路径]
        "changes.mentionedPaths" => {
            Ok(Value::from(file_changes::mentioned_paths(&string(params, "command")?, &string(params, "base")?, &string(params, "home")?)))
        }
        // {path, home?} → null（write_file / edit_file 刚写完：同一时间跑着的命令不把它算进去）
        "changes.noteWritten" => {
            file_changes::note_written(&string(params, "path")?, &string(params, "home")?);
            Ok(Value::Null)
        }

        // run_shell 的规则。{timeoutSeconds?} → 秒（默认 60，限在 1–600）
        "shell.timeout" => Ok(Value::from(shell_rules::timeout_seconds(optional_int(params, "timeoutSeconds")))),
        // {command, processName, pid} → 拒绝的说明 | null（会结束 Friday 自己的命令）
        "shell.selfKill" => {
            Ok(shell_rules::self_kill_refusal(&string(params, "command")?, &string(params, "processName")?, int(params, "pid")?)
                .map_or(Value::Null, Value::from))
        }
        // {command, conversation?} → 结果后面的提示（以换行开头，没有为空）
        "shell.hint" => Ok(Value::from(shell_rules::hint(&string(params, "command")?, &string(params, "conversation")?))),
        // {exitCode?, output} → 「退出码 N」+ 输出（exitCode 不给：超时或被终止）
        "shell.result" => Ok(Value::from(shell_rules::result(optional_int(params, "exitCode"), &string(params, "output")?))),

        // 工具定义。{args?, override?} → [{name, title, symbol, fallbackSymbol, permission, spec}]（内置工具）
        "tools.builtins" => {
            let args = string_map(params, "args")?;
            Ok(Value::from(tool_defs::builtins(&args, override_id(params)).iter().map(tool_defs::Builtin::to_json).collect::<Vec<_>>()))
        }
        // {name, description?, parameters, args?, override?} → spec（说明有提示词的用提示词的）
        "tools.spec" => {
            let parameters = params.get("parameters").cloned().unwrap_or_else(|| json!({}));
            Ok(tool_defs::make_spec(
                &string(params, "name")?,
                &string(params, "description")?,
                &parameters,
                &string_map(params, "args")?,
                override_id(params),
            ))
        }
        // {params: [{name, type, description?, required?}]} → 参数的 JSON Schema
        "tools.schema" => {
            let list = list(params, "params")?;
            let fields: Vec<(String, String, String, bool)> = list
                .iter()
                .map(|item| {
                    let text = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or("").to_string();
                    (text("name"), text("type"), text("description"), item.get("required").and_then(Value::as_bool).unwrap_or(false))
                })
                .collect();
            let params: Vec<tool_defs::Param> = fields
                .iter()
                .map(|(name, kind, description, required)| tool_defs::Param { name, kind, description, required: *required })
                .collect();
            Ok(tool_defs::schema(&params))
        }
        // {spec, arguments} → [没给的必填参数]
        "tools.missingRequired" => {
            let spec = params.get("spec").cloned().unwrap_or(Value::Null);
            Ok(Value::from(tool_defs::missing_required(&spec, &object(params, "arguments")?)))
        }

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
            "directory": prompts::directory().map(|dir| tool_files::slash(&dir)),
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
        // {value, pretty?} → 文字（== Swift JSONEncoder [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]；pretty: false 是紧凑的）
        "json.format" => {
            let value = params.get("value").cloned().unwrap_or(Value::Null);
            let pretty = params.get("pretty").and_then(Value::as_bool).unwrap_or(true);
            Ok(Value::from(if pretty { swift_json::pretty(&value) } else { swift_json::compact(&value) }))
        }
        // MCP 的配置（见 mcp_config.rs）。{text, fallbackName?} → {servers: {名字: 配置}} | {failure: {kind: invalid | failed, message}}
        "mcp.parseServers" => {
            let fallback = params.get("fallbackName").and_then(Value::as_str);
            Ok(match mcp_config::parse_servers(&string(params, "text")?, fallback) {
                Ok(servers) => json!({ "servers": servers }),
                Err((invalid, message)) => json!({ "failure": { "kind": if invalid { "invalid" } else { "failed" }, "message": message } }),
            })
        }
        // {name} → server 名合不合法
        "mcp.isValidName" => Ok(Value::from(mcp_config::is_valid_name(&string(params, "name")?))),
        // {server, tool} → 工具的全名
        "mcp.qualifiedName" => Ok(Value::from(mcp_config::qualified_name(&string(params, "server")?, &string(params, "tool")?))),
        // {file} → 类型检查的失败原因 | null（file 是整个 mcp.json 的对象）
        "mcp.decodeProblem" => Ok(mcp_config::decode_problem(&object(params, "file")?).map_or(Value::Null, Value::from)),
        // {path} → {mcpServers: {…}}（读不出、格式不对都是空的）
        "mcp.load" => Ok(Value::Object(mcp_config::load(std::path::Path::new(&string(params, "path")?)))),
        // {path} → 编辑框里的原文
        "mcp.configText" => Ok(Value::from(mcp_config::config_text(std::path::Path::new(&string(params, "path")?)))),
        // {path, text} → 空 = 存好了，否则原因（先查格式，对了原样写）
        "mcp.saveText" => Ok(Value::from(
            mcp_config::save_text(std::path::Path::new(&string(params, "path")?), &string(params, "text")?).err().unwrap_or_default(),
        )),
        // {path, servers} → 空 = 存好了，否则原因（按 Swift JSONEncoder 的写法写回）
        "mcp.save" => Ok(Value::from(
            mcp_config::save(std::path::Path::new(&string(params, "path")?), &object(params, "servers")?).err().unwrap_or_default(),
        )),
        // 一轮对话的循环（见 engine_loop.rs：两端执行，核心决定下一步；历史在核心里）
        // {config: {engineID, model, baseURL, supportsVision, supportsTools, thinking, extraBody}, history?} → {loop}
        "loop.open" => {
            let config = object(params, "config")?;
            let text = |key: &str| config.get(key).and_then(Value::as_str).unwrap_or("").to_string();
            let flag = |key: &str| config.get(key).and_then(Value::as_bool).unwrap_or(false);
            let config = engine_loop::Config {
                engine_id: text("engineID"),
                model: text("model"),
                base_url: text("baseURL"),
                supports_vision: flag("supportsVision"),
                supports_tools: flag("supportsTools"),
                thinking: text("thinking"),
                extra_body: text("extraBody"),
            };
            Ok(json!({ "loop": engine_loop::open(config, list(params, "history")?) }))
        }
        // {loop} → null
        "loop.close" => {
            engine_loop::close(handle(params, "loop")?);
            Ok(Value::Null)
        }
        // {loop} → 历史（OpenAI 格式，不含 system）
        "loop.history" => loop_of(params, |engine| Value::Array(engine.history().to_vec())),
        // v2.86 {loop} → 这个循环接上的消息（原样）
        "loop.appended" => loop_of(params, |engine| Value::Array(engine.appended().to_vec())),
        // {loop, history} → null（整段换掉）；{loop, messages} → null（接在后面）；{loop} → null（之前的不再折叠）
        "loop.setHistory" => {
            let history = list(params, "history")?;
            loop_of(params, |engine| {
                engine.set_history(history);
                Value::Null
            })
        }
        "loop.append" => {
            let messages = list(params, "messages")?;
            loop_of(params, |engine| {
                engine.append(messages);
                Value::Null
            })
        }
        "loop.foldFromHere" => loop_of(params, |engine| {
            engine.fold_from_here();
            Value::Null
        }),
        // {loop, settings: {instructions, tools, temperature, enableThinking, reasoningEffort?, maxOutputTokens?, maxToolRounds,
        //   continuesWhenTruncated, hasDispatch, folding?: {budget, echoesReasoning, taskStateTool, wholeTools}}, begin?} → null
        //   （换上设置；begin：一轮开始，计数清零）
        "loop.settings" => {
            let settings = object(params, "settings")?;
            let folding = match settings.get("folding") {
                Some(Value::Object(folding)) => Some((
                    folding.get("budget").and_then(Value::as_i64).unwrap_or(folding::BUDGET),
                    folding.get("echoesReasoning").and_then(Value::as_bool).unwrap_or(false),
                    special(folding)?,
                )),
                _ => None,
            };
            let settings = engine_loop::Settings {
                instructions: settings.get("instructions").and_then(Value::as_str).unwrap_or("").to_string(),
                tools: settings.get("tools").and_then(Value::as_array).cloned().unwrap_or_default(),
                temperature: settings.get("temperature").and_then(Value::as_f64).unwrap_or(0.6),
                enable_thinking: settings.get("enableThinking").and_then(Value::as_bool).unwrap_or(false),
                reasoning_effort: settings.get("reasoningEffort").and_then(Value::as_str).map(str::to_string),
                max_output_tokens: settings.get("maxOutputTokens").and_then(Value::as_i64),
                max_tool_rounds: settings.get("maxToolRounds").and_then(Value::as_i64).unwrap_or(0),
                continues_when_truncated: settings.get("continuesWhenTruncated").and_then(Value::as_bool).unwrap_or(true),
                has_dispatch: settings.get("hasDispatch").and_then(Value::as_bool).unwrap_or(false),
                folding,
                // v2.86 已折叠步骤的上限（0 不限）、主智能体这一轮是第几轮（子智能体不给）
                folded_budget: settings.get("foldedBudget").and_then(Value::as_i64).unwrap_or(0),
                history_turn: settings.get("historyTurn").and_then(Value::as_i64),
            };
            let begin = flag(params, "begin");
            loop_of(params, |engine| {
                if begin {
                    engine.begin(settings);
                } else {
                    engine.configure(settings);
                }
                Value::Null
            })
        }
        // {loop, probed?: 上限 | null, override?} → {probe: true} | {includeTools, showReasoning, wrapUp, local, fitted, folded, trimmed}
        //   （probe：本地大模型要先问上限，问完带上 probed 再调一次；probed 为 null 是问过了没问到）
        "loop.prepare" => {
            let probed = params.get("probed").map(Value::as_i64);
            let override_id = override_id(params);
            loop_of(params, |engine| engine.prepare(probed, override_id))
        }
        // {loop, includeTools, xFriday?} → 请求体（遮过密钥）
        "loop.body" => {
            let include = flag(params, "includeTools");
            let x_friday = params.get("xFriday").filter(|value| !value.is_null()).cloned();
            loop_of(params, |engine| engine.body(include, x_friday))
        }
        // {loop, promptTokens, contextTokens} → {retry, fitted?}（请求被说超长）
        "loop.overflow" => {
            let (prompt, context) = (int(params, "promptTokens")?, int(params, "contextTokens")?);
            loop_of(params, |engine| engine.overflow(prompt, context))
        }
        // {loop} → 收尾请求失败时交给界面的那句话
        "loop.wrapUpFailed" => loop_of(params, |engine| Value::from(engine.wrap_up_failed())),
        // {loop, pass, started, finished, firstToken?, fallback} → 统计（时间是 Unix 毫秒；pass 是 stream.finish 的结果）
        "loop.passDone" => {
            let pass = engine_loop::pass_from(&object(params, "pass")?);
            let (started, finished, fallback) = (int(params, "started")?, int(params, "finished")?, int(params, "fallback")?);
            let first = optional_int(params, "firstToken");
            loop_of(params, |engine| engine.pass_done(pass, started, finished, first, fallback))
        }
        // {loop, keepGoing, override?} → {next: finish | prepare | tools, calls?: [{id, name, arguments | null, rejected?}], repetition?, chunks?, extraDone?}
        "loop.decide" => {
            let keep_going = params.get("keepGoing").and_then(Value::as_bool).unwrap_or(true);
            let override_id = override_id(params);
            loop_of(params, |engine| engine.decide(keep_going, override_id))
        }
        // {loop, results: [{id, name, content}], images?, imageMessage?, interjection?, override?} → {next: prepare, repetition?}
        "loop.toolResults" => {
            let results: Vec<(String, String, String)> = list(params, "results")?
                .iter()
                .map(|item| {
                    let field = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or("").to_string();
                    (field("id"), field("name"), field("content"))
                })
                .collect();
            let images = params.get("images").and_then(Value::as_u64).unwrap_or(0) as usize;
            let image_message = params.get("imageMessage").filter(|value| value.is_object()).cloned();
            let interjection = list(params, "interjection")?;
            let override_id = override_id(params);
            loop_of(params, |engine| engine.tool_results(results, images, image_message, interjection, override_id))
        }
        // 提示词文件夹（见 prompt_store.rs；状态推 promptStore.state，段变了推 promptStore.promptsChanged）。
        // {git?: 路径 | null（给了就用，不给 = 在 git 的队列里请两端找：promptStore.findGit）, environment?: {名字: 值}, version} → 状态
        "promptStore.start" => {
            let environment: Vec<(String, String)> = string_map(params, "environment")?.into_iter().collect();
            Ok(prompt_store::start(given_git(params), environment, &string(params, "version")?))
        }
        // {git?} → null（重新找 git、读历史）
        "promptStore.refresh" => {
            prompt_store::refresh(given_git(params));
            Ok(Value::Null)
        }
        // {id, message?} → 接了没有（正在忙、没装 git 不接）
        "promptStore.rollback" => {
            let message = params.get("message").and_then(Value::as_str).map(str::to_string);
            Ok(Value::from(prompt_store::rollback(&string(params, "id")?, message)))
        }
        // {} → 接了没有
        "promptStore.restoreBundled" => Ok(Value::from(prompt_store::restore_bundled())),
        // {sections: {键: 文字}, message, token?} → 空 = 好了，否则原因（token 给了的提交完推 promptStore.committed {token, version}）
        "promptStore.applySections" => {
            let sections = string_map(params, "sections")?.into_iter().collect();
            let token = params.get("token").and_then(Value::as_str).map(str::to_string);
            Ok(Value::from(prompt_store::apply_sections(&sections, &string(params, "message")?, token)))
        }
        // {timeoutMs?} → {idle, state}（马上看一遍文件、把要提交的提交掉，等 git 跑完；在调用的线程上等）
        "promptStore.settle" => {
            let timeout = std::time::Duration::from_millis(params.get("timeoutMs").and_then(Value::as_u64).unwrap_or(10_000));
            let idle = prompt_store::settle(timeout);
            Ok(json!({ "idle": idle, "state": prompt_store::state() }))
        }
        // {id} → {键: 文字}（那个版本的全部段；在调用的线程上跑 git）
        "promptStore.sectionsAt" => Ok(to_value(prompt_store::sections_at(&string(params, "id")?))?),
        // {} → 状态
        "promptStore.state" => Ok(prompt_store::state()),
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

fn mcp_client_of(params: &Params) -> Result<std::sync::Arc<mcp_client::Client>, ApiError> {
    let id = handle(params, "client")?;
    mcp_client::get(id).ok_or_else(|| ApiError::invalid(format!("没有这个 MCP 客户端：{id}")))
}

fn timeout_ms(params: &Params) -> u64 {
    params.get("timeoutMs").and_then(Value::as_u64).unwrap_or(30_000)
}

fn call_id(params: &Params) -> Option<u64> {
    params.get("call").and_then(Value::as_u64)
}

fn mcp_outcome(outcome: Result<Value, mcp_client::Failure>) -> Value {
    match outcome {
        Ok(value) => value,
        Err(failure) if failure.cancelled => json!({ "failure": failure.message, "cancelled": true }),
        Err(failure) => json!({ "failure": failure.message }),
    }
}

fn blocks_of(params: &Params) -> Result<Vec<crate::chat_models::MessageBlock>, ApiError> {
    serde_json::from_value(Value::Array(list(params, "blocks")?)).map_err(|error| ApiError::invalid(format!("blocks 解不出来：{error}")))
}

fn task_state_of(params: &Params) -> Result<task_state::TaskState, ApiError> {
    match params.get("state") {
        None | Some(Value::Null) => Ok(task_state::TaskState::default()),
        Some(value) => serde_json::from_value(value.clone()).map_err(|error| ApiError::invalid(format!("state 解不出来：{error}"))),
    }
}

fn user_messages(params: &Params) -> Result<Vec<task_state::UserMessage>, ApiError> {
    Ok(list(params, "messages")?
        .iter()
        .map(|message| task_state::UserMessage {
            text: message.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
            from_agent: message.get("fromAgent").and_then(Value::as_bool).unwrap_or(false),
            interjected: message.get("interjected").and_then(Value::as_bool).unwrap_or(false),
            files: message
                .get("files")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
        })
        .collect())
}

fn candidates(params: &Params, key: &str) -> Result<Vec<chooser::Candidate>, ApiError> {
    Ok(list(params, key)?.iter().map(chooser::Candidate::from_json).collect())
}

fn conversations_of(params: &Params, key: &str) -> Result<Vec<Conversation>, ApiError> {
    serde_json::from_value(Value::Array(list(params, key)?)).map_err(|error| ApiError::invalid(format!("{key} 解不出来：{error}")))
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

fn retry_after(params: &Params) -> Option<f64> {
    match params.get("retryAfter") {
        Some(Value::Number(seconds)) => seconds.as_f64(),
        Some(Value::String(text)) => chat_request::header_seconds(text),
        _ => None,
    }
}

fn flag(params: &Params, key: &str) -> bool {
    params.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// 可选的整数（整数值的小数也算）
fn optional_int(params: &Params, key: &str) -> Option<i64> {
    match params.get(key) {
        Some(Value::Number(_)) => int(params, key).ok(),
        _ => None,
    }
}

fn tool_output(outcome: Result<tool_files::Output, tool_files::ToolError>) -> Result<Value, ApiError> {
    Ok(match outcome {
        Ok(output) => json!({
            "output": output.text,
            "lineDiff": output.line_diff.map(|counts| json!({ "added": counts.added, "removed": counts.removed })),
        }),
        Err(error) => failure(&error),
    })
}

fn text_output(outcome: Result<String, tool_files::ToolError>) -> Result<Value, ApiError> {
    Ok(match outcome {
        Ok(text) => json!({ "output": text }),
        Err(error) => failure(&error),
    })
}

fn failure(error: &tool_files::ToolError) -> Value {
    let kind = match error {
        tool_files::ToolError::Invalid(_) => "invalid",
        tool_files::ToolError::Failed(_) => "failed",
    };
    json!({ "failure": { "kind": kind, "message": error.message() } })
}

/// edits 数组：每项 {old_text, new_text, replace_all?}；不给为 None
fn edits(params: &Params) -> Result<Option<Vec<file_edit::Edit>>, ApiError> {
    let Some(items) = params.get("edits").and_then(Value::as_array) else { return Ok(None) };
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let text = |key: &str| item.get(key).and_then(Value::as_str).map(str::to_string);
            match (text("old_text"), text("new_text")) {
                (Some(old_text), Some(new_text)) => Ok(file_edit::Edit {
                    old_text,
                    new_text,
                    replace_all: item.get("replace_all").and_then(Value::as_bool).unwrap_or(false),
                }),
                _ => Err(ApiError::invalid(format!("edits 的第 {} 项要是 {{old_text, new_text, replace_all?}}", index + 1))),
            }
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn edit_change(params: &Params) -> Result<tool_files::Change, ApiError> {
    if let Some(edits) = edits(params)?.filter(|edits| !edits.is_empty()) {
        return Ok(tool_files::Change::Several(edits));
    }
    Ok(tool_files::Change::One {
        old_text: string(params, "oldText")?,
        new_text: string(params, "newText")?,
        replace_all: flag(params, "replaceAll"),
    })
}

/// git 参数：不给 = 请两端找（None）；null = 没有；文字 = 路径
fn given_git(params: &Params) -> Option<Option<String>> {
    match params.get("git") {
        None => None,
        Some(Value::String(git)) if !git.is_empty() => Some(Some(git.clone())),
        Some(_) => Some(None),
    }
}

fn loop_of(params: &Params, body: impl FnOnce(&mut engine_loop::Loop) -> Value) -> Result<Value, ApiError> {
    let id = handle(params, "loop")?;
    engine_loop::with(id, body).ok_or_else(|| ApiError::invalid(format!("没有这个对话循环：{id}（关掉了？）")))
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

/// now（Unix 毫秒），不传是现在
fn now_ms(params: &Params) -> f64 {
    params.get("now").and_then(Value::as_f64).unwrap_or_else(|| chrono::Utc::now().timestamp_millis() as f64)
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

    #[test]
    fn chat_request() {
        let body = parsed(
            "chat.requestBody",
            r#"{"model":"m","history":[{"role":"user","content":"hi"}],"temperature":0.3,"baseURL":"https://open.bigmodel.cn/api/paas/v4",
                "thinking":"effort","enableThinking":true,"reasoningEffort":"high","tools":[{"type":"function"}],"mask":true}"#,
        )["result"]
            .clone();
        assert_eq!(body["messages"], json!([{"role": "user", "content": "hi"}]));
        assert_eq!((body["temperature"].clone(), body["reasoning_effort"].clone()), (json!(0.3), json!("high")));
        assert_eq!(body["thinking"], json!({"type": "enabled"}));
        assert_eq!(body["tools"], json!([{"type": "function"}]));
        assert_eq!(parsed("chat.retryDelay", r#"{"attempt":2,"retryAfter":" 9"}"#)["result"], json!(9.0));
        assert_eq!(parsed("chat.retryDelay", r#"{"attempt":2,"retryAfter":"soon"}"#)["result"], json!(4.0));
        assert_eq!(parsed("chat.rateLimitDelay", r#"{"attempt":1,"jitterMs":250}"#)["result"], json!(2.25));
        assert_eq!(
            parsed("chat.contextOverflow", r#"{"status":400,"body":"prompt is too long"}"#)["result"],
            json!({"promptTokens": 0, "contextTokens": 0})
        );
        parsed("chat.rememberUnsupported", r#"{"engineID":"api-test","names":["seed"]}"#);
        assert_eq!(parsed("chat.unsupportedFor", r#"{"engineID":"api-test"}"#)["result"], json!(["seed"]));
    }
}
