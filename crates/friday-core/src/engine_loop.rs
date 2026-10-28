//! 在线大脑一轮对话的循环（== Swift `OpenAIChatEngine.run`、Qt `OpenAIChatEngine::run`）：请求 → 流式输出 → 有工具调用就执行并继续，
//! 直到模型给出最终回答。v2.79 第 4 阶段起决定都在这里：工具轮次到上限收尾、按会话折叠和上下文上限压历史、截断了自动接着写、
//! 复读截断和提醒、每步说差不多的话提醒、统计和上下文进度环、给模型看的截图只留最近两张。历史（OpenAI 格式，不含 system）也在这里。
//!
//! 两端照旧负责执行（核心做不了的）：发请求读流（走系统代理，流式解析用 chat_stream）、把文字交给界面、跑工具、读媒体文件、
//! 问本地大模型的上下文上限、用户中途补充的话。每一步做完把结果交过来，核心说下一步做什么：
//!
//! 1. `begin`（这一轮的设置）→ 两端 `append` 用户消息；
//! 2. `prepare` → `{probe}`（本地大模型要先问上限，问完再 `prepare` 一次带上 `probed`）或这段请求要不要带工具、显不显示思考；
//! 3. `body` 取请求体，两端发请求；超长报错交 `overflow`（压完重发一次）、收尾请求失败交 `wrap_up_failed`；
//! 4. `pass_done` 交这一段的结果，拿回统计（两端交给界面）；
//! 5. `decide`（界面还要不要接着）→ `finish` / `prepare`（再请求一段）/ `tools`（要跑的工具调用）；
//! 6. 跑完工具 `tool_results`（连同截图那条用户消息、中途补充的话）→ 回到 2。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use serde_json::{Map, Value, json};
use unicode_segmentation::UnicodeSegmentation;

use crate::chat_request::{self, BodyOptions};
use crate::context::{self, Parts, Summaries};
use crate::folding::{self, Special};
use crate::prompts::{self, Args};
use crate::repetition;

/// 输出被长度上限截断后最多自动接着写几次
pub const MAX_AUTO_CONTINUES: i64 = 5;
/// 给模型看的截图那条用户消息的开头（== Swift ModelImages.marker、Qt ModelImages::marker）
pub const IMAGE_MARKER: &str = "［工具截图］";

/// 不变的：哪个引擎（== EngineConfig）
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub engine_id: String,
    pub model: String,
    pub base_url: String,
    pub supports_vision: bool,
    pub supports_tools: bool,
    /// ThinkingSupport 的原始值（用户设了的按用户的）
    pub thinking: String,
    pub extra_body: String,
}

/// 每轮开头给的设置（两端引擎上的属性，轮与轮之间会变）
#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub instructions: String,
    pub tools: Vec<Value>,
    pub temperature: f64,
    pub enable_thinking: bool,
    pub reasoning_effort: Option<String>,
    pub max_output_tokens: Option<i64>,
    /// 一轮里最多请求几次（0 = 不限）；到上限再请求一次、不给工具，让模型交付目前的结果
    pub max_tool_rounds: i64,
    pub continues_when_truncated: bool,
    /// 有没有跑工具的（没有的话模型要调工具也当说完了）
    pub has_dispatch: bool,
    /// 按会话折叠较早的步骤（子智能体、主智能体这一轮自己的步骤）；None 不折
    pub folding: Option<(i64, bool, Special)>,
    /// v2.86 已折叠的步骤最多留多少（估算 token；0 不限）：超过就把最早的整步去掉（folding::trim）
    pub folded_budget: i64,
    /// v2.86 主智能体这一轮是第几轮（去掉的步骤留的说明写「用 read_history 看第几轮」）；None 是子智能体（只说重新调用工具）
    pub history_turn: Option<i64>,
}

/// 这一段请求的结果（两端读完流交过来的；== PassResult）
#[derive(Debug, Clone, Default)]
pub struct Pass {
    pub content: String,
    pub reasoning: String,
    /// （id, 名字, 参数原文）
    pub tool_calls: Vec<(String, String, String)>,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: Option<i64>,
    pub finish_reason: Option<String>,
    /// 复读：（开始位置 UTF-16，那一段，复读的是思考）
    pub repetition: Option<(usize, String, bool)>,
}

impl Pass {
    fn truncated(&self) -> bool {
        self.finish_reason.as_deref() == Some("length")
    }
}

#[derive(Debug, Default)]
pub struct Loop {
    config: Config,
    settings: Settings,
    history: Vec<Value>,
    /// history 前多少条已经折叠过
    folded: usize,
    /// v2.86 从哪条起归这次运行折（fold_from_here 记下）、这次运行一共去掉了几步已折叠的
    fold_start: usize,
    dropped: usize,
    /// v2.86 这个循环接上的消息（原样，折叠、压缩都不动它）：存档存原样的两端（EDITH）拿它
    appended: Vec<Value>,
    /// 上一段请求发出时的估算（本地大模型收到 prompt_tokens 后校准用）
    last_estimate: i64,
    // 这一轮的
    passes: i64,
    auto_continues: i64,
    /// 这一段是截断后的续写：续写前那一两句思考不显示
    continuing: bool,
    loops: i64,
    sayings: Vec<String>,
    // 这一段的
    wrap_up: bool,
    local: bool,
    refitted: bool,
    /// prepare 问上限去了，回来接着压
    awaiting_probe: bool,
    /// 问上限之前那次折叠的结果（问完再交给两端）
    folded_note: Value,
    trimmed_note: Value,
    parts: Parts,
    pass: Option<Pass>,
}

fn text(key: &str, args: &[(&str, String)], override_id: Option<u64>) -> String {
    let args: Args = args.iter().map(|(name, value)| (name.to_string(), value.clone())).collect();
    prompts::text(key, &args, override_id)
}

/// 没有 usage 时的估算：约 3.5 字节 1 token
pub fn estimate_tokens(text: &str) -> i64 {
    if text.is_empty() { 0 } else { ((text.len() as f64 / 3.5).round() as i64).max(1) }
}

/// 连续空白换成一个空格、去掉首尾空白（== QString::simplified）
fn simplified(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn prefix(text: &str, count: usize) -> String {
    text.graphemes(true).take(count).collect()
}

fn utf16_prefix(text: &str, count: usize) -> String {
    String::from_utf16_lossy(&text.encode_utf16().take(count).collect::<Vec<_>>())
}

fn user(content: String) -> Value {
    json!({ "role": "user", "content": content })
}

impl Loop {
    pub fn new(config: Config, history: Vec<Value>) -> Self {
        Loop { config, history, ..Loop::default() }
    }

    fn is_local(&self) -> bool {
        self.config.base_url.starts_with("power://")
    }

    fn context_key(&self) -> String {
        format!("{}|{}", self.config.engine_id, self.config.model)
    }

    pub fn history(&self) -> &[Value] {
        &self.history
    }

    pub fn set_history(&mut self, history: Vec<Value>) {
        self.history = history;
        self.folded = self.folded.min(self.history.len());
        self.fold_start = self.fold_start.min(self.folded);
    }

    pub fn append(&mut self, messages: Vec<Value>) {
        for message in messages {
            self.push(message);
        }
    }

    /// v2.86 接一条消息：历史里的会被折叠、压缩，appended 里的是原样
    fn push(&mut self, message: Value) {
        self.appended.push(message.clone());
        self.history.push(message);
    }

    /// v2.86 这个循环接上的消息（原样）
    pub fn appended(&self) -> &[Value] {
        &self.appended
    }

    /// 之前的历史都不再折叠（主智能体每轮开头）；v2.86 去掉已折叠步骤（folding::trim）也从这里算起
    pub fn fold_from_here(&mut self) {
        self.folded = self.folded.max(self.history.len());
        self.fold_start = self.folded;
        self.dropped = 0;
    }

    /// 只换设置（不动这一轮的计数；两端改了引擎上的属性、单独取请求体时用）
    pub fn configure(&mut self, settings: Settings) {
        self.settings = settings;
    }

    /// 一轮开始：换上这一轮的设置，计数清零
    pub fn begin(&mut self, settings: Settings) {
        self.settings = settings;
        self.passes = 0;
        self.auto_continues = 0;
        self.continuing = false;
        self.loops = 0;
        self.sayings.clear();
        self.awaiting_probe = false;
        self.pass = None;
    }

    fn tool_bytes(&self) -> i64 {
        if !self.config.supports_tools || self.settings.tools.is_empty() {
            return 0;
        }
        Value::Array(self.settings.tools.clone()).to_string().len() as i64
    }

    /// 整段请求（系统提示 + 工具定义 + 历史）按字节的估算 token（没乘校准系数）
    fn request_estimate(&self) -> i64 {
        let fixed = estimate_tokens(&self.settings.instructions) + (self.tool_bytes() as f64 / 3.5) as i64;
        fixed + folding::estimated_tokens(&self.history)
    }

    fn summaries(&self) -> Option<Summaries> {
        self.settings.folding.as_ref().map(|(_, echoes_reasoning, special)| Summaries {
            echoes_reasoning: *echoes_reasoning,
            special: Special { task_state: special.task_state.clone(), whole: special.whole.clone() },
        })
    }

    /// 估算超过上限（force：不看是否超过）就压历史；压了返回 (压的结果, 上限)
    fn fit(&mut self, force: bool, limit: Option<i64>) -> Option<(context::Compacted, i64)> {
        let key = self.context_key();
        let ratio = context::ratio(&key);
        let estimate = self.request_estimate();
        let tokens = estimate as f64 * ratio;
        let limit = limit?;
        if !force && tokens <= context::threshold(limit) as f64 {
            return None;
        }
        // 历史的预算 = 目标 − 系统提示和工具定义（都按估算，换回估算单位）
        let fixed = estimate - folding::estimated_tokens(&self.history);
        let budget = (context::target(limit) as f64 / ratio) as i64 - fixed;
        // 系统提示和工具定义就放不下：压历史也没用，照旧发、按报错处理
        if budget <= 0 {
            return None;
        }
        let summaries = self.summaries();
        let mut marks = [self.folded, self.fold_start];
        let result = context::compact_marks(&mut self.history, &mut marks, budget, summaries.as_ref());
        [self.folded, self.fold_start] = marks;
        (result.shortened > 0 || result.dropped > 0).then_some((result, limit))
    }

    /// 要不要先问本地大模型的上限：请求很小、离记下的上限还远就不问
    fn wants_probe(&self) -> bool {
        let key = self.context_key();
        let tokens = self.request_estimate() as f64 * context::ratio(&key);
        tokens >= context::SMALL_REQUEST as f64 && context::known_limit(&key).is_none_or(|limit| tokens > limit as f64 / 2.0)
    }

    /// 准备下一段请求。probed：问到的上限（Some(None) 是问过了没问到；None 是还没问）
    pub fn prepare(&mut self, probed: Option<Option<i64>>, override_id: Option<u64>) -> Value {
        let mut fitted = Value::Null;
        let mut folded = Value::Null;
        if !self.awaiting_probe {
            self.passes += 1;
            // 工具轮次到上限：不再给工具，让模型把目前的结果交出来，正常结束这一轮
            self.wrap_up = self.settings.max_tool_rounds > 0 && self.passes > self.settings.max_tool_rounds;
            if self.wrap_up {
                let prompt = text("wrap-up", &[("rounds", self.settings.max_tool_rounds.to_string())], override_id);
                self.push(user(prompt));
            }
            if let Some((budget, echoes, special)) = &self.settings.folding {
                let special = Special { task_state: special.task_state.clone(), whole: special.whole.clone() };
                let applied = folding::apply(&mut self.history, &mut self.folded, *budget, *echoes, &special);
                if applied.steps > 0 {
                    folded = json!({ "steps": applied.steps, "before": applied.before, "after": applied.after });
                }
                self.folded_note = folded.clone();
                // v2.86 折叠过的步骤也有上限：最早的整步去掉
                let turn = self.settings.history_turn;
                let note = move |count: usize| match turn {
                    Some(turn) => {
                        text("context-fold.dropped-steps", &[("count", count.to_string()), ("turn", turn.to_string())], override_id)
                    }
                    None => text("context-fold.dropped-steps.sub", &[("count", count.to_string())], override_id),
                };
                let trimmed = folding::trim(
                    &mut self.history,
                    self.fold_start,
                    &mut self.folded,
                    &mut self.dropped,
                    self.settings.folded_budget,
                    &note,
                );
                self.trimmed_note = if trimmed.steps > 0 {
                    json!({ "steps": trimmed.steps, "total": trimmed.total, "after": trimmed.after })
                } else {
                    Value::Null
                };
            }
            self.local = self.is_local();
            if self.local && probed.is_none() && self.wants_probe() {
                self.awaiting_probe = true;
                return json!({ "probe": true });
            }
        }
        self.awaiting_probe = false;
        if folded.is_null() {
            folded = std::mem::take(&mut self.folded_note);
        }
        self.folded_note = Value::Null;
        let trimmed = std::mem::take(&mut self.trimmed_note);
        let key = self.context_key();
        if self.local
            && let Some(Some(limit)) = probed
        {
            context::remember_limit(&key, limit);
        }
        // v2.11 本地大模型、v2.86 起知道上限的在线模型（超长报错里记的、服务商报的；设置里猜的不算）：按上限先压历史
        let limit = context::known_limit(&key)
            .or_else(|| (!self.local).then(|| context::provider_limit(&self.config.base_url, &self.config.model)).flatten());
        if let Some((result, limit)) = self.fit(false, limit) {
            fitted = compacted_json(&result, limit, context::ratio(&key));
        }
        // 这次请求的各部分（按字数估算，回来后按实际的 prompt_tokens 缩放）
        let tool_bytes = if self.wrap_up { 0 } else { self.tool_bytes() };
        self.parts = Parts::estimate(&self.settings.instructions, tool_bytes, &self.history);
        self.last_estimate = self.request_estimate();
        self.refitted = false;
        json!({
            "includeTools": !self.wrap_up,
            "showReasoning": !self.continuing,
            "wrapUp": self.wrap_up,
            "local": self.local,
            "fitted": fitted,
            "folded": folded,
            "trimmed": trimmed,
        })
    }

    /// 这段请求的请求体（遮过密钥）；x_friday：本地大模型的调度提示（两端拼好）
    pub fn body(&self, include_tools: bool, x_friday: Option<Value>) -> Value {
        let tools: &[Value] = if include_tools && self.config.supports_tools { &self.settings.tools } else { &[] };
        let options = BodyOptions {
            model: self.config.model.clone(),
            instructions: self.settings.instructions.clone(),
            history: &self.history,
            temperature: self.settings.temperature,
            max_tokens: self.settings.max_output_tokens,
            tools,
            base_url: self.config.base_url.clone(),
            thinking: self.config.thinking.clone(),
            enable_thinking: self.settings.enable_thinking,
            reasoning_effort: self.settings.reasoning_effort.clone(),
            extra_body: self.config.extra_body.clone(),
            engine_id: self.config.engine_id.clone(),
            x_friday,
        };
        chat_request::masked(chat_request::request_body(&options))
    }

    /// 请求被说超长：记下报错里的上限（和实际长度，校准估算），压完重发一次（v2.86 起在线模型也这样，以前在线模型直接失败）。
    /// 上限：报错里的、记下的，在线模型再看服务商报的；都没有就把这次请求的估算当成上限的 1.25 倍压（压掉约一半）、不记下来，
    /// 结果里的 limit 给 0（两端提示里不写数）。返回要不要重发、压了什么
    pub fn overflow(&mut self, prompt_tokens: i64, context_tokens: i64) -> Value {
        let key = self.context_key();
        if context_tokens > 0 {
            context::remember_limit(&key, context_tokens);
        }
        if self.refitted {
            return json!({ "retry": false });
        }
        if prompt_tokens > 0 && self.last_estimate > 0 {
            context::remember_ratio(&key, prompt_tokens as f64 / self.last_estimate as f64);
        }
        let known = context::known_limit(&key)
            .or_else(|| (!self.local).then(|| context::provider_limit(&self.config.base_url, &self.config.model)).flatten());
        let limit = known.unwrap_or_else(|| (self.request_estimate() as f64 * context::ratio(&key) * 0.8) as i64);
        if limit <= 0 {
            return json!({ "retry": false });
        }
        let Some((result, limit)) = self.fit(true, Some(limit)) else { return json!({ "retry": false }) };
        self.last_estimate = self.request_estimate();
        self.refitted = true;
        json!({ "retry": true, "fitted": compacted_json(&result, if known.is_some() { limit } else { 0 }, context::ratio(&key)) })
    }

    /// 收尾请求本身失败（个别接口不接受「历史里有工具调用但请求不带 tools」）：给一句固定说明，不让整轮报错
    pub fn wrap_up_failed(&mut self) -> String {
        let text = format!("（已连续调用工具 {} 轮，达到本轮上限，先停在这里；接着做请回复「继续」。）", self.settings.max_tool_rounds);
        self.push(json!({ "role": "assistant", "content": text }));
        text
    }

    /// 一段请求读完：算统计（和上下文进度环），记下结果等 decide。时间都是 Unix 毫秒；fallback：设置里的最大上下文长度
    pub fn pass_done(&mut self, pass: Pass, started: i64, finished: i64, first_token: Option<i64>, fallback: i64) -> Value {
        let key = self.context_key();
        // v2.86 起在线模型也校准
        if pass.prompt_tokens > 0 && self.last_estimate > 0 {
            context::remember_ratio(&key, pass.prompt_tokens as f64 / self.last_estimate as f64);
        }
        let mut generated = pass.completion_tokens;
        let cached = pass.cached_tokens.unwrap_or(0).clamp(0, pass.prompt_tokens.max(0));
        // 首个 token 之前是网络往返 + 服务端处理提示词，记作 promptTime；之后才是生成时间
        let first = first_token.unwrap_or(finished);
        let mut prompt_time = (first - started) as f64 / 1000.0;
        let mut generate_time = (finished - first) as f64 / 1000.0;
        // 服务端没有真正流式输出时首 token 就是最后一个，分不出等待和生成，整段按生成时间算
        if generate_time < 0.5 {
            generate_time += prompt_time;
            prompt_time = 0.0;
        }
        // 服务端没给 usage 时按文字粗估，别显示 0
        if generated == 0 {
            let arguments: String = pass.tool_calls.iter().map(|call| call.2.as_str()).collect();
            generated = estimate_tokens(&format!("{}{}{arguments}", pass.content, pass.reasoning));
        }
        let mut used = self.parts.scaled(if pass.prompt_tokens > 0 { pass.prompt_tokens } else { self.parts.total() });
        used.replies += generated;
        let (limit, guessed) = context::limit(&self.config.engine_id, &self.config.base_url, &self.config.model, fallback);
        // 自动压缩的线：v2.86 起在线模型知道上限时也有
        let compact_at = (!guessed).then(|| context::threshold(limit));
        self.pass = Some(pass.clone());
        json!({
            "promptTokens": pass.prompt_tokens,
            "generatedTokens": generated,
            "cachedTokens": cached,
            "promptTime": prompt_time,
            "generateTime": generate_time,
            "contextParts": used.to_json(),
            "contextLimit": { "tokens": limit, "guessed": guessed },
            "contextCompactAt": compact_at,
        })
    }

    /// 决定下一步（pass_done 之后）。keep_going：界面还要接着（Qt 的回调返回 false 就是不要了）
    pub fn decide(&mut self, keep_going: bool, override_id: Option<u64>) -> Value {
        let Some(mut pass) = self.pass.take() else { return json!({ "next": "finish" }) };
        if let Some((start, unit, in_reasoning)) = pass.repetition.clone() {
            // 复读：截掉重复的部分（第一遍留着）记进历史，提醒模型别再重复、按手上的工具接着做；提醒过几次还复读就收尾
            if in_reasoning {
                pass.reasoning = utf16_prefix(&pass.reasoning, start);
            } else {
                pass.content = utf16_prefix(&pass.content, start);
            }
            let mut assistant = json!({ "role": "assistant", "content": pass.content });
            if !pass.reasoning.is_empty() {
                assistant["reasoning_content"] = json!(pass.reasoning);
            }
            self.push(assistant);
            let kind = if in_reasoning { "思考" } else { "回复" };
            let sample = prefix(&simplified(&unit), 40);
            let repetition = json!({ "kind": kind, "sample": sample, "times": self.loops + 1 });
            if !keep_going {
                return json!({ "next": "finish", "repetition": repetition });
            }
            if self.loops >= repetition::MAX_RETRIES {
                let gave_up = format!("\n\n{}", text("repetition.gave-up", &[], override_id));
                self.push(json!({ "role": "assistant", "content": gave_up.trim() }));
                return json!({ "next": "finish", "repetition": repetition, "chunks": [gave_up], "extraDone": true });
            }
            self.loops += 1;
            self.auto_continues = 0;
            self.continuing = false;
            self.push(user(text("repetition.loop", &[("kind", kind.to_string()), ("sample", sample)], override_id)));
            return json!({ "next": "prepare", "repetition": repetition });
        }
        let mut assistant = json!({ "role": "assistant", "content": pass.content });
        if pass.tool_calls.is_empty() || !self.settings.has_dispatch || self.wrap_up {
            if !pass.reasoning.is_empty() {
                assistant["reasoning_content"] = json!(pass.reasoning);
            }
            self.push(assistant);
            // 没说完就被长度上限截断：自动接着写
            if pass.truncated()
                && !self.wrap_up
                && keep_going
                && self.settings.continues_when_truncated
                && self.auto_continues < MAX_AUTO_CONTINUES
            {
                self.auto_continues += 1;
                self.continuing = true;
                let key = if pass.content.is_empty() { "continue.reasoning" } else { "continue.output" };
                self.push(user(text(key, &[], override_id)));
                return json!({ "next": "prepare" });
            }
            return json!({ "next": "finish" });
        }
        // 界面不要了：不再执行工具（历史里也不留下没有结果的工具调用，免得下一轮请求不合法）
        if !keep_going {
            return json!({ "next": "finish" });
        }
        self.auto_continues = 0;
        self.continuing = false;
        let mut calls = Vec::new();
        let mut recorded = Vec::new();
        for (id, name, arguments) in &pass.tool_calls {
            let parsed = serde_json::from_str::<Value>(arguments).ok().filter(Value::is_object);
            // 参数不是合法 JSON（多半是输出被截断、没写完）时历史里记成 {}：llama-server 渲染聊天模板时要把历史里的参数解析成 JSON，
            // 原样带回去下一次请求会直接 HTTP 500，模型连纠正的机会都没有。退回的原因写在这次调用的工具结果里
            recorded.push(json!({
                "id": id, "type": "function",
                "function": { "name": name, "arguments": if parsed.is_some() { arguments.as_str() } else { "{}" } },
            }));
            let rejected = parsed.is_none().then(|| {
                if pass.truncated() {
                    text("rejected.truncated", &[], override_id)
                } else {
                    text("rejected.invalid", &[("arguments", prefix(arguments, 300))], override_id)
                }
            });
            calls.push(json!({ "id": id, "name": name, "arguments": parsed, "rejected": rejected }));
        }
        assistant["tool_calls"] = Value::Array(recorded);
        // DeepSeek 要求把 reasoning_content 原样带回
        if !pass.reasoning.is_empty() {
            assistant["reasoning_content"] = json!(pass.reasoning);
        }
        self.push(assistant);
        self.pass = Some(Pass { tool_calls: Vec::new(), ..pass });
        json!({ "next": "tools", "calls": calls })
    }

    /// 工具跑完：结果记进历史；截图那条用户消息（两端拼好的，images 是这一步拍了几张）只给最近两张留图；接上用户中途补充的话；
    /// 每步说差不多的话第 3、6…次提醒
    pub fn tool_results(
        &mut self,
        results: Vec<(String, String, String)>,
        images: usize,
        image_message: Option<Value>,
        interjection: Vec<Value>,
        override_id: Option<u64>,
    ) -> Value {
        for (id, name, content) in results {
            self.push(json!({ "role": "tool", "tool_call_id": id, "name": name, "content": content }));
        }
        if images > 0
            && self.config.supports_vision
            && let Some(message) = image_message
        {
            let mut kept = 0;
            for index in (0..self.history.len()).rev() {
                let entry = &self.history[index];
                let screenshot = entry.get("role").and_then(Value::as_str) == Some("user")
                    && entry.get("content").and_then(Value::as_array).is_some_and(|parts| {
                        parts.iter().any(|part| part.get("text").and_then(Value::as_str).is_some_and(|text| text.starts_with(IMAGE_MARKER)))
                    });
                if !screenshot {
                    continue;
                }
                kept += 1;
                if kept + images <= 2 {
                    continue;
                }
                self.history[index] = user(format!("{IMAGE_MARKER}（较早的截图，已省略）"));
            }
            self.push(message);
        }
        for message in interjection {
            self.push(message);
        }
        let content = self.pass.take().map(|pass| pass.content).unwrap_or_default();
        let similar = repetition::similar_count(&content, &self.sayings);
        self.sayings.push(content.clone());
        if similar >= 2 && (similar - 2).is_multiple_of(3) {
            let said = prefix(&simplified(&content), 60);
            self.push(user(text("repetition.saying", &[("times", (similar + 1).to_string()), ("said", said.clone())], override_id)));
            return json!({ "next": "prepare", "repetition": { "kind": "说了没做", "sample": said, "times": similar + 1 } });
        }
        json!({ "next": "prepare" })
    }
}

fn compacted_json(result: &context::Compacted, limit: i64, ratio: f64) -> Value {
    json!({ "shortened": result.shortened, "dropped": result.dropped, "before": result.before, "after": result.after, "limit": limit, "ratio": ratio })
}

// MARK: - 句柄（每个引擎实例一个）

type Shared = Arc<Mutex<Loop>>;

static LOOPS: LazyLock<Mutex<HashMap<u64, Shared>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT: AtomicU64 = AtomicU64::new(1);

pub fn open(config: Config, history: Vec<Value>) -> u64 {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    LOOPS.lock().unwrap().insert(id, Arc::new(Mutex::new(Loop::new(config, history))));
    id
}

pub fn close(id: u64) {
    LOOPS.lock().unwrap().remove(&id);
}

pub fn with<T>(id: u64, body: impl FnOnce(&mut Loop) -> T) -> Option<T> {
    let shared = LOOPS.lock().unwrap().get(&id).cloned()?;
    let mut state = shared.lock().unwrap();
    Some(body(&mut state))
}

/// 参数里的 pass：{content, reasoning, toolCalls: [{id, name, arguments}], promptTokens, completionTokens, cachedTokens?, finishReason?,
/// loop?: {start, unit, inReasoning}}（== stream.finish 的结果）
pub fn pass_from(value: &Map<String, Value>) -> Pass {
    let text = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let number = |key: &str| value.get(key).and_then(Value::as_i64).unwrap_or(0);
    Pass {
        content: text("content"),
        reasoning: text("reasoning"),
        tool_calls: value
            .get("toolCalls")
            .and_then(Value::as_array)
            .map(|calls| {
                calls
                    .iter()
                    .map(|call| {
                        let field = |key: &str| call.get(key).and_then(Value::as_str).unwrap_or("").to_string();
                        (field("id"), field("name"), field("arguments"))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        prompt_tokens: number("promptTokens"),
        completion_tokens: number("completionTokens"),
        cached_tokens: value.get("cachedTokens").and_then(Value::as_i64),
        finish_reason: value.get("finishReason").and_then(Value::as_str).filter(|reason| !reason.is_empty()).map(str::to_string),
        repetition: value.get("loop").and_then(Value::as_object).map(|found| {
            (
                found.get("start").and_then(Value::as_u64).unwrap_or(0) as usize,
                found.get("unit").and_then(Value::as_str).unwrap_or("").to_string(),
                found.get("inReasoning").and_then(Value::as_bool).unwrap_or(false),
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompts_installed() -> std::sync::MutexGuard<'static, ()> {
        let lock = prompts::TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = "@@ wrap-up\n到 {{rounds}} 轮了，收尾\n\n@@ continue.output\n接着写\n\n@@ continue.reasoning\n接着想\n\n@@ repetition.loop\n别在{{kind}}里复读：{{sample}}\n\n\
                    @@ repetition.gave-up\n放弃了\n\n@@ repetition.saying\n第 {{times}} 次：{{said}}\n\n@@ rejected.truncated\n截断了\n\n@@ rejected.invalid\n参数不对：{{arguments}}\n\n\
                    @@ context-fit.omitted\n省略了 {{count}} 条\n\n@@ context-fold.dropped-steps\n第 {{turn}} 轮较早的 {{count}} 步已省略\n\n\
                    @@ context-fold.dropped-steps.sub\n较早的 {{count}} 步已省略\n";
        prompts::setup(std::iter::once(("prompts-framework.md".to_string(), text.to_string())).collect(), None, Vec::new());
        lock
    }

    fn settings() -> Settings {
        Settings {
            tools: vec![json!({ "type": "function" })],
            temperature: 0.6,
            continues_when_truncated: true,
            has_dispatch: true,
            ..Settings::default()
        }
    }

    fn config() -> Config {
        Config {
            engine_id: "loop-test".into(),
            model: "m".into(),
            base_url: "https://x/v1".into(),
            supports_tools: true,
            ..Config::default()
        }
    }

    fn pass(content: &str) -> Pass {
        Pass { content: content.into(), finish_reason: Some("stop".into()), ..Pass::default() }
    }

    #[test]
    fn tools_then_answer() {
        let _prompts = prompts_installed();
        let mut engine = Loop::new(config(), Vec::new());
        engine.begin(settings());
        engine.append(vec![user("几点了".into())]);
        let prepared = engine.prepare(None, None);
        assert_eq!((prepared["includeTools"].clone(), prepared["showReasoning"].clone()), (json!(true), json!(true)));
        assert_eq!(prepared["folded"], Value::Null);
        assert_eq!(engine.body(true, None)["tools"], json!([{ "type": "function" }]));
        let calls = Pass {
            tool_calls: vec![("a".into(), "get_time".into(), "{}".into()), ("b".into(), "read".into(), "{\"pa".into())],
            finish_reason: Some("tool_calls".into()),
            ..pass("看一下")
        };
        let stats = engine.pass_done(calls, 0, 2_000, Some(1_000), 1_000_000);
        assert_eq!((stats["promptTime"].clone(), stats["generateTime"].clone()), (json!(1.0), json!(1.0)));
        assert_eq!(stats["contextLimit"], json!({ "tokens": 1_000_000, "guessed": true }));
        let decided = engine.decide(true, None);
        assert_eq!(decided["next"], "tools");
        assert_eq!(decided["calls"][1]["rejected"], "参数不对：{\"pa");
        assert_eq!(decided["calls"][0]["arguments"], json!({}));
        // 历史里坏参数记成 {}
        assert_eq!(engine.history()[1]["tool_calls"][1]["function"]["arguments"], "{}");
        let next = engine.tool_results(
            vec![("a".into(), "get_time".into(), "十点".into()), ("b".into(), "read".into(), "参数不对".into())],
            0,
            None,
            vec![user("补充一句".into())],
            None,
        );
        assert_eq!(next["next"], "prepare");
        assert_eq!(engine.history().len(), 5);
        engine.prepare(None, None);
        engine.pass_done(pass("十点了"), 0, 100, None, 1_000_000);
        assert_eq!(engine.decide(true, None)["next"], "finish");
        assert_eq!(engine.history().last().unwrap(), &json!({ "role": "assistant", "content": "十点了" }));
    }

    #[test]
    fn wrap_up_truncation_and_repetition() {
        let _prompts = prompts_installed();
        let mut engine = Loop::new(config(), Vec::new());
        engine.begin(Settings { max_tool_rounds: 1, ..settings() });
        engine.prepare(None, None);
        engine.pass_done(Pass { tool_calls: vec![("a".into(), "x".into(), "{}".into())], ..pass("") }, 0, 10, None, 100);
        assert_eq!(engine.decide(true, None)["next"], "tools");
        engine.tool_results(vec![("a".into(), "x".into(), "好".into())], 0, None, Vec::new(), None);
        // 第二段到上限：收尾、不带工具
        let prepared = engine.prepare(None, None);
        assert_eq!(prepared["includeTools"], false);
        assert_eq!(engine.history().last().unwrap()["content"], "到 1 轮了，收尾");
        assert!(engine.body(false, None).get("tools").is_none());
        assert!(engine.wrap_up_failed().starts_with("（已连续调用工具 1 轮"));
        // 截断了接着写，续写的那段不显示思考
        let mut engine = Loop::new(config(), Vec::new());
        engine.begin(settings());
        engine.prepare(None, None);
        engine.pass_done(Pass { finish_reason: Some("length".into()), ..pass("写了一半") }, 0, 10, None, 100);
        assert_eq!(engine.decide(true, None)["next"], "prepare");
        assert_eq!(engine.history().last().unwrap()["content"], "接着写");
        assert_eq!(engine.prepare(None, None)["showReasoning"], false);
        // 界面不要了：不续写
        engine.pass_done(Pass { finish_reason: Some("length".into()), ..pass("又一半") }, 0, 10, None, 100);
        assert_eq!(engine.decide(false, None)["next"], "finish");
        // 复读：截掉、提醒；提醒过两次还复读就收尾
        let mut engine = Loop::new(config(), Vec::new());
        engine.begin(settings());
        for round in 0..3 {
            engine.prepare(None, None);
            let repeated =
                Pass { repetition: Some((3, "一段话".into(), false)), finish_reason: None, ..pass("开头一段话一段话") };
            engine.pass_done(repeated, 0, 10, None, 100);
            let decided = engine.decide(true, None);
            if round < 2 {
                assert_eq!(decided["next"], "prepare");
                assert_eq!(engine.history().last().unwrap()["content"], "别在回复里复读：一段话");
            } else {
                assert_eq!(decided["next"], "finish");
                assert_eq!(decided["chunks"], json!(["\n\n放弃了"]));
                assert_eq!(decided["extraDone"], true);
            }
        }
        assert_eq!(engine.history()[0], json!({ "role": "assistant", "content": "开头一" }));
    }

    #[test]
    fn screenshots_and_sayings() {
        let _prompts = prompts_installed();
        let mut engine = Loop::new(Config { supports_vision: true, ..config() }, Vec::new());
        engine.begin(settings());
        let shot = |n: usize| json!({ "role": "user", "content": [{ "type": "image_url" }, { "type": "text", "text": format!("{IMAGE_MARKER}第 {n} 张") }] });
        let saying = "现在派子 Agent 审查三个客户端，我继续审查服务器的握手和调试日志";
        for step in 0..4 {
            engine.prepare(None, None);
            engine.pass_done(Pass { tool_calls: vec![("a".into(), "shot".into(), "{}".into())], ..pass(saying) }, 0, 10, None, 100);
            engine.decide(true, None);
            let next = engine.tool_results(vec![("a".into(), "shot".into(), "拍了".into())], 1, Some(shot(step)), Vec::new(), None);
            // 第 3 次说差不多的话提醒
            assert_eq!(next.get("repetition").is_some(), step == 2, "{step} {next}");
        }
        let shots: Vec<&Value> = engine.history().iter().filter(|entry| entry["role"] == "user" && entry["content"].is_array()).collect();
        assert_eq!(shots.len(), 2);
        assert!(engine.history().iter().any(|entry| entry["content"] == json!(format!("{IMAGE_MARKER}（较早的截图，已省略）"))));
    }

    fn special() -> Special {
        Special { task_state: "update_task_state".into(), whole: vec!["delegate_task".into()] }
    }

    /// v2.86 在线模型：不知道上限、被说超长（报错里写了上限）就记下、压完重发一次；之后知道上限，发之前就压；
    /// 报错里没写上限按这次请求估的压掉约一半、不记；压过还超长不再重发（== Swift SelfTests.contextFitEngine ④⑤⑥）
    #[test]
    fn online_overflow() {
        let _prompts = prompts_installed();
        let history: Vec<Value> =
            (1..=6).flat_map(|n| [user(format!("第 {n} 件事")), json!({ "role": "assistant", "content": "z".repeat(4_000) })]).collect();
        let online = |id: &str| Config { engine_id: id.into(), model: "online-model".into(), ..config() };
        let mut engine = Loop::new(online("online-1"), history.clone());
        engine.begin(settings());
        engine.append(vec![user("第 7 件事".into())]);
        let prepared = engine.prepare(None, None);
        assert_eq!(prepared["fitted"], Value::Null, "不知道上限：不压");
        let before = folding::estimated_tokens(engine.history());
        let overflow = engine.overflow(9_000, 4_096);
        assert_eq!(overflow["retry"], true);
        assert_eq!(overflow["fitted"]["limit"], 4_096);
        assert!(folding::estimated_tokens(engine.history()) < before / 2);
        assert_eq!(engine.history().last().unwrap()["content"], "第 7 件事", "最后一条用户消息不动");
        assert_eq!(context::known_limit("online-1|online-model"), Some(4_096));
        assert!(context::ratio("online-1|online-model") > 1.0, "按报错里的实际长度校准");
        assert_eq!(engine.overflow(9_000, 4_096)["retry"], false, "压过还超长：不再重发");
        // 知道上限了：发之前就压
        let mut again = Loop::new(online("online-1"), history.clone());
        again.begin(settings());
        again.append(vec![user("第 7 件事".into())]);
        let prepared = again.prepare(None, None);
        assert_eq!(prepared["fitted"]["limit"], 4_096);
        // 报错里没写上限：压掉约一半，不记上限，limit 给 0
        let mut vague = Loop::new(online("online-2"), history.clone());
        vague.begin(settings());
        vague.append(vec![user("第 7 件事".into())]);
        vague.prepare(None, None);
        let before = folding::estimated_tokens(vague.history());
        let overflow = vague.overflow(0, 0);
        assert_eq!((overflow["retry"].clone(), overflow["fitted"]["limit"].clone()), (json!(true), json!(0)));
        assert!(folding::estimated_tokens(vague.history()) < before * 2 / 3);
        assert_eq!(context::known_limit("online-2|online-model"), None);
        // 进度环：知道上限的在线模型也有自动压缩的线
        let stats = again.pass_done(pass("好"), 0, 10, None, 1_000_000);
        assert_eq!(stats["contextLimit"], json!({ "tokens": 4_096, "guessed": false }));
        assert_eq!(stats["contextCompactAt"], context::threshold(4_096));
    }

    /// v2.86 一轮里折过的步骤也有上限：最早的整步去掉、说明写第几轮；appended 里是原样（== Swift SelfTests.sessionTrim 的引擎那一半）
    #[test]
    fn trims_folded_steps() {
        let _prompts = prompts_installed();
        let earlier = vec![user("上一轮".into()), json!({ "role": "assistant", "content": "好" })];
        let mut engine = Loop::new(config(), earlier);
        engine.begin(Settings { folding: Some((4_000, false, special())), folded_budget: 1_500, history_turn: Some(7), ..settings() });
        engine.fold_from_here();
        engine.append(vec![user("这一轮的任务".into())]);
        let mut trimmed = 0;
        for step in 0..14 {
            let prepared = engine.prepare(None, None);
            if prepared["trimmed"].is_object() {
                trimmed += 1;
            }
            let id = format!("c{step}");
            engine.pass_done(
                Pass { tool_calls: vec![(id.clone(), "run_shell".into(), "{}".into())], ..pass(&format!("第 {step} 步")) },
                0,
                10,
                None,
                100,
            );
            assert_eq!(engine.decide(true, None)["next"], "tools");
            engine.tool_results(vec![(id, "run_shell".into(), "出".repeat(2_000))], 0, None, Vec::new(), None);
        }
        assert!(trimmed > 0, "去掉过");
        let history = engine.history();
        assert_eq!(history[..2], [user("上一轮".into()), json!({ "role": "assistant", "content": "好" })], "之前几轮不动");
        assert_eq!(history[2]["content"], "这一轮的任务");
        let carrier = history.iter().find(|entry| entry["content"].as_str().is_some_and(|text| text.starts_with("第 7 轮较早的"))).unwrap();
        assert_eq!(carrier["role"], "assistant");
        assert_eq!(history.last().unwrap()["content"].as_str().unwrap().chars().count(), 2_000, "最新的一步不动");
        // appended：这个循环接上的全部原样
        let appended = engine.appended();
        assert_eq!(appended.len(), 1 + 14 * 2);
        assert_eq!(appended[0]["content"], "这一轮的任务");
        assert!(
            appended
                .iter()
                .filter(|entry| entry["role"] == "tool")
                .all(|entry| entry["content"].as_str().unwrap().chars().count() == 2_000)
        );
        assert!(appended.iter().all(|entry| !entry["content"].as_str().unwrap_or("").contains("已省略")));
        // 子智能体（没给第几轮）：说明只说重新调用工具
        let mut sub = Loop::new(config(), Vec::new());
        sub.begin(Settings { folding: Some((4_000, false, special())), folded_budget: 1_500, ..settings() });
        sub.append(vec![user("任务".into())]);
        for step in 0..14 {
            sub.prepare(None, None);
            let id = format!("s{step}");
            sub.pass_done(Pass { tool_calls: vec![(id.clone(), "run_shell".into(), "{}".into())], ..pass("看") }, 0, 10, None, 100);
            sub.decide(true, None);
            sub.tool_results(vec![(id, "run_shell".into(), "出".repeat(2_000))], 0, None, Vec::new(), None);
        }
        assert!(sub.history().iter().any(|entry| entry["content"].as_str().is_some_and(|text| text.starts_with("较早的 "))));
    }
}
