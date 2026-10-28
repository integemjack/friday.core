//! 上下文折叠：历史太长时，把较早轮次的工具结果、长参数、长回复压成摘要。
//! - 主会话（两端的 AgentRunner.foldHistoryIfNeeded，要改会话存档，留在两端）：还没折叠的那部分超过 [`BUDGET`] 才触发，
//!   一触发就把最近一轮之前的全部标成折叠；重建历史时用这里的 [`text`]、[`arguments`]、[`tool_result`] 写摘要
//! - 子 Agent 会话（v1.19，在线引擎）：[`apply`] 在请求历史上直接折叠，预算是主会话的两倍；
//!   v2.86 折过的步骤也有上限：[`trim`] 把最早的整步去掉（== Swift / Qt 的 SessionFolding.trim）
//!
//! 两次折叠之间历史一直是同一个样子——不会每轮改写历史，让在线接口的前缀缓存反复失效。
//! 字数按字形簇计（同 Swift 的 String.count / prefix / suffix：组合 emoji 算一个字）。
//!
//! 原来是 Swift `Agent/ContextFolding.swift`、`LLM/Engine/SessionFolding.swift` 和 Qt 的同名文件各写一份，文案、阈值逐字照搬。

use serde_json::{Map, Value};
use unicode_segmentation::UnicodeSegmentation;

/// 还没折叠的历史（不含系统提示和工具说明）超过多少估算 token 就折叠（主会话；子 Agent 会话是它的两倍）
pub const BUDGET: i64 = 48_000;

/// v2.86 一次运行（子 Agent、主 Agent 的一轮）里已折叠的步骤最多留多少（会话预算的四分之一）：超过就把最早的整步去掉（[`trim`]）
pub const SESSION_FOLDED_BUDGET: i64 = BUDGET * 2 / 4;

/// 结果不能截的工具：两端按各自的注册表给（任务状态工具的名字、会跑子 Agent 并返回它结果的工具）
#[derive(Debug, Clone, Default)]
pub struct Special {
    pub task_state: String,
    pub whole: Vec<String>,
}

fn graphemes(text: &str) -> Vec<&str> {
    text.graphemes(true).collect()
}

/// 较早的回复正文：太长（超过 1500 字）只留开头 1000 字和结尾 300 字
pub fn text(text: &str) -> String {
    let parts = graphemes(text);
    if parts.len() <= 1_500 {
        return text.to_string();
    }
    format!("{}\n…（较早的回复，已折叠，原 {} 字）…\n{}", parts[..1_000].concat(), parts.len(), parts[parts.len() - 300..].concat())
}

/// 较早的工具调用参数：长字符串（超过 300 字：写入的文件内容、脚本源码等）只留开头 120 字；只看顶层
pub fn arguments(arguments: &Map<String, Value>) -> Map<String, Value> {
    arguments
        .iter()
        .map(|(key, value)| {
            let folded = match value {
                Value::String(text) => {
                    let parts = graphemes(text);
                    if parts.len() > 300 {
                        Value::String(format!("{}…（已折叠，原 {} 字）", parts[..120].concat(), parts.len()))
                    } else {
                        value.clone()
                    }
                }
                _ => value.clone(),
            };
            (key.clone(), folded)
        })
        .collect()
}

/// 参数的 JSON 文本（在线引擎历史里的写法）：解得开就按 [`arguments`] 折叠后重新编码（紧凑）；没有要折叠的、解不开的原样
pub fn arguments_json(json: &str) -> String {
    let Ok(Value::Object(object)) = serde_json::from_str::<Value>(json) else { return json.to_string() };
    let folded = arguments(&object);
    if folded == object { json.to_string() } else { Value::Object(folded).to_string() }
}

/// 较早的工具结果：任务状态只说记下了；子 Agent 的结果不截（v1.18 起只有它的最终结果，是主 Agent 往下做的依据，和汇报一样不能丢）；
/// 其他长结果（超过 500 字）留开头 300 字和结尾 150 字（报错、退出码常在结尾）
pub fn tool_result(name: &str, text: &str, special: &Special) -> String {
    if name == special.task_state {
        return "（已记下任务状态）".to_string();
    }
    if special.whole.iter().any(|whole| whole == name) {
        return text.to_string();
    }
    let parts = graphemes(text);
    if parts.len() <= 500 {
        return text.to_string();
    }
    format!(
        "{}\n…（较早的工具结果，已折叠，原 {} 字；需要细节就重新调用工具）…\n{}",
        parts[..300].concat(),
        parts.len(),
        parts[parts.len() - 150..].concat()
    )
}

// MARK: - 估算

/// 一条请求历史（在线接口的写法）的字节数：正文（多模态取文字部分，每张图按 1000 字节算）、思考、工具调用的名字和参数
fn bytes(entry: &Value) -> usize {
    let mut total = 0;
    match entry.get("content") {
        Some(Value::String(content)) => total += content.len(),
        Some(Value::Array(parts)) if parts.iter().all(Value::is_object) => {
            total += parts.iter().map(|part| part.get("text").and_then(Value::as_str).map_or(1_000, str::len)).sum::<usize>();
        }
        _ => {}
    }
    total += entry.get("reasoning_content").and_then(Value::as_str).map_or(0, str::len);
    if let Some(Value::Array(calls)) = entry.get("tool_calls") {
        for call in calls {
            let function = call.get("function");
            let field = |key: &str| function.and_then(|function| function.get(key)).and_then(Value::as_str).map_or(0, str::len);
            total += field("name") + field("arguments");
        }
    }
    total
}

/// 粗估 token 数：中文约 1 字 1 token，英文约 4 字节 1 token，按 UTF-8 字节 / 3.5（舍去小数）
pub fn estimated_tokens(entries: &[Value]) -> i64 {
    (entries.iter().map(bytes).sum::<usize>() as f64 / 3.5) as i64
}

fn entry_tokens(entry: &Value) -> i64 {
    (bytes(entry) as f64 / 3.5) as i64
}

// MARK: - 子 Agent 会话

/// 折叠了一次的结果
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Applied {
    /// 折了几步（助手消息数；0 = 这次没折）
    pub steps: usize,
    /// 没折叠部分折叠前、整段历史折叠后的估算 token（没折时都是 0）
    pub before: i64,
    pub after: i64,
}

/// v1.19 一个会话里工具循环越跑越长时，把较早的步骤折叠成摘要（在线引擎的子 Agent 用）。
/// 还没折叠的部分（folded 之后）估算超过 budget 时，除最近约半个预算的步骤外一次性折叠，切在一条助手消息前面；
/// 最新的一步无论多长都留着（模型还没看过它的结果）。用户消息一字不动；工具结果、长参数、长正文换成摘要；
/// 思考去掉——echoes_reasoning 的接口（DeepSeek）要求原样带回，留着。folded 是前面多少条已经折叠过（调用方保存，下次接着用）
pub fn apply(history: &mut [Value], folded: &mut usize, budget: i64, echoes_reasoning: bool, special: &Special) -> Applied {
    if *folded >= history.len() {
        return Applied::default();
    }
    let unfolded = estimated_tokens(&history[*folded..]);
    if unfolded <= budget {
        return Applied::default();
    }
    let mut kept = 0;
    let mut cut = history.len();
    for index in (*folded..history.len()).rev() {
        kept += entry_tokens(&history[index]);
        if history[index].get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if cut < history.len() && kept > budget / 2 {
            break;
        }
        cut = index;
    }
    if cut <= *folded {
        return Applied::default();
    }
    let mut steps = 0;
    for entry in &mut history[*folded..cut] {
        let Value::Object(entry) = entry else { continue };
        match entry.get("role").and_then(Value::as_str) {
            Some("tool") => {
                if let Some(Value::String(content)) = entry.get("content") {
                    let name = entry.get("name").and_then(Value::as_str).unwrap_or("");
                    let summary = tool_result(name, content, special);
                    entry.insert("content".to_string(), Value::String(summary));
                }
            }
            Some("assistant") => {
                steps += 1;
                if let Some(Value::String(content)) = entry.get("content") {
                    let summary = text(content);
                    entry.insert("content".to_string(), Value::String(summary));
                }
                if !echoes_reasoning {
                    entry.remove("reasoning_content");
                }
                if let Some(Value::Array(calls)) = entry.get_mut("tool_calls") {
                    for call in calls.iter_mut() {
                        if let Some(Value::Object(function)) = call.get_mut("function")
                            && let Some(Value::String(json)) = function.get("arguments")
                        {
                            let summary = arguments_json(json);
                            function.insert("arguments".to_string(), Value::String(summary));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    *folded = cut;
    let after = if steps > 0 { estimated_tokens(history) } else { 0 };
    Applied { steps, before: if steps > 0 { unfolded } else { 0 }, after }
}

/// 去掉了一次的结果
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Trimmed {
    /// 这次去掉几步（0 = 没去）
    pub steps: usize,
    /// 这次运行一共去掉了几步
    pub total: usize,
    /// 整段历史去掉后的估算 token
    pub after: i64,
}

/// 两条挨着的用户消息并成一条（正文接在后面；有图片的按多模态的写法拼）
fn merged(first: &Value, second: &Value) -> Value {
    let mut result = first.clone();
    if let (Some(Value::String(a)), Some(Value::String(b))) = (first.get("content"), second.get("content")) {
        result["content"] = Value::String(format!("{a}\n\n{b}"));
        return result;
    }
    let parts = |entry: &Value| -> Vec<Value> {
        match entry.get("content") {
            Some(Value::Array(parts)) => parts.clone(),
            other => vec![serde_json::json!({ "type": "text", "text": other.and_then(Value::as_str).unwrap_or("") })],
        }
    };
    let mut all = parts(first);
    all.extend(parts(second));
    result["content"] = Value::Array(all);
    result
}

/// v2.86 [start, folded) 里折叠过的部分估算超过 folded_budget 时，从最早的起整步去掉（助手消息连同它的工具结果；
/// 用户消息一字不动），去到不到一半——成批做，两次之间历史不变。去掉的地方留一句说明（note 给的，参数是一共去掉几步），写在紧跟着的
/// 那条助手消息的正文前面：下次最先去掉的就是它，说明跟着往后挪。去掉后挨在一起的两条用户消息并成一条（有的接口不认连着两条）。
/// start：从哪条起归这次运行（主智能体是这一轮的开头，之前几轮归两端）；dropped：一共去掉了几步（调用方保存）
pub fn trim(
    history: &mut Vec<Value>,
    start: usize,
    folded: &mut usize,
    dropped: &mut usize,
    folded_budget: i64,
    note: &dyn Fn(usize) -> String,
) -> Trimmed {
    if folded_budget <= 0 || start >= *folded || *folded > history.len() {
        return Trimmed::default();
    }
    let mut total = estimated_tokens(&history[start..*folded]);
    if total <= folded_budget {
        return Trimmed::default();
    }
    let role = |entry: &Value| entry.get("role").and_then(Value::as_str).unwrap_or("").to_string();
    let mut drop = vec![false; history.len()];
    let mut steps = 0;
    let mut index = start;
    while index < *folded && total > folded_budget / 2 {
        if role(&history[index]) != "assistant" {
            index += 1;
            continue;
        }
        let mut end = index + 1;
        while end < *folded && role(&history[end]) == "tool" {
            end += 1;
        }
        for k in index..end {
            drop[k] = true;
            total -= entry_tokens(&history[k]);
        }
        steps += 1;
        index = end;
    }
    if steps == 0 {
        return Trimmed::default();
    }
    *dropped += steps;
    let text = note(*dropped);
    let mut rebuilt: Vec<Value> = Vec::new();
    let mut new_folded = 0;
    let mut placed = false;
    for (k, entry) in history.iter().enumerate() {
        if k == *folded {
            new_folded = rebuilt.len();
        }
        if drop[k] {
            continue;
        }
        let mut entry = entry.clone();
        // 最后一段去掉的之后第一条助手消息：说明写在它前面
        if !placed && k >= index && role(&entry) == "assistant" {
            let body = entry.get("content").and_then(Value::as_str).unwrap_or("").to_string();
            entry["content"] = Value::String(if body.is_empty() { text.clone() } else { format!("{text}\n\n{body}") });
            placed = true;
        }
        if k > start && k <= index && role(&entry) == "user" && rebuilt.last().is_some_and(|last| role(last) == "user") {
            let last = rebuilt.pop().unwrap();
            rebuilt.push(merged(&last, &entry));
            continue;
        }
        rebuilt.push(entry);
    }
    if *folded >= history.len() {
        new_folded = rebuilt.len();
    }
    *history = rebuilt;
    *folded = new_folded;
    Trimmed { steps, total: *dropped, after: estimated_tokens(history) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn special() -> Special {
        Special {
            task_state: "update_task_state".to_string(),
            whole: ["delegate_task", "run_parallel_tasks", "rework_agent", "run_workflow"].map(str::to_string).to_vec(),
        }
    }

    /// == Qt TestContextAssist::foldingSummaries
    #[test]
    fn summaries() {
        let exactly = "长".repeat(1_500);
        assert_eq!(text(&exactly), exactly);
        let long = "甲".repeat(1_000) + &"长".repeat(1_700) + &"乙".repeat(300);
        assert_eq!(text(&long), "甲".repeat(1_000) + "\n…（较早的回复，已折叠，原 3000 字）…\n" + &"乙".repeat(300));
        // 按字形计：组合 emoji 算一个字
        let family = "👨‍👩‍👧‍👦";
        let folded = text(&family.repeat(2_000));
        assert!(folded.starts_with(&(family.repeat(1_000) + "\n…（较早的回复，已折叠，原 2000 字）…\n")));
        assert!(folded.ends_with(&("…\n".to_string() + &family.repeat(300))));
        assert_eq!(text(&family.repeat(1_500)), family.repeat(1_500));

        let original = json!({"path": "a.py", "content": "x".repeat(2_000), "limit": 300, "edge": "y".repeat(300), "nested": {"code": "z".repeat(500)}});
        let folded = arguments(original.as_object().unwrap());
        assert_eq!(folded["path"], json!("a.py"));
        assert_eq!(folded["content"], json!("x".repeat(120) + "…（已折叠，原 2000 字）"));
        assert_eq!(folded["limit"], json!(300));
        assert_eq!(folded["edge"], json!("y".repeat(300)));
        assert_eq!(folded["nested"], original["nested"]);
        // JSON 文本：没有要折叠的、解不开的原样
        assert_eq!(arguments_json(r#"{"path": "a"}"#), r#"{"path": "a"}"#);
        assert_eq!(arguments_json("[1, 2]"), "[1, 2]");
        assert_eq!(arguments_json("{"), "{");
        assert!(arguments_json(&json!({"content": "x".repeat(400)}).to_string()).contains("已折叠，原 400 字"));

        let special = special();
        assert_eq!(tool_result("update_task_state", "已记下。当前任务状态：\n目标：g", &special), "（已记下任务状态）");
        for team in ["delegate_task", "run_parallel_tasks"] {
            assert_eq!(tool_result(team, &"r".repeat(5_000), &special), "r".repeat(5_000));
        }
        assert_eq!(tool_result("run_shell", &"o".repeat(500), &special), "o".repeat(500));
        let result = "开头".to_string() + &"中".repeat(5_000) + "结尾退出码 1";
        let shortened = tool_result("run_shell", &result, &special);
        let parts = graphemes(&result);
        assert_eq!(
            shortened,
            parts[..300].concat()
                + "\n…（较早的工具结果，已折叠，原 5009 字；需要细节就重新调用工具）…\n"
                + &parts[parts.len() - 150..].concat()
        );
        assert!(shortened.ends_with("结尾退出码 1"));
        // read_agent_log 不是派发类工具：按普通工具结果折叠
        assert_eq!(tool_result("read_agent_log", &result, &special), shortened);
    }

    /// == Qt TestContextAssist::foldingBudget 的估算部分
    #[test]
    fn estimates() {
        assert_eq!(estimated_tokens(&[json!({"role": "user", "content": "a".repeat(3_500)})]), 1_000);
        assert_eq!(estimated_tokens(&[json!({"role": "user", "content": "a".repeat(3_499)})]), 999);
        assert_eq!(estimated_tokens(&[json!({"role": "user", "content": "中".repeat(1_000)})]), 857);
        assert_eq!(estimated_tokens(&[]), 0);
        // 工具调用：名字 + 参数的 JSON 文本（9 + 15 = 24 字节 → 6）
        let call = json!({"role": "assistant", "content": "", "tool_calls": [{"function": {"name": "read_file", "arguments": r#"{"path":"a\/b"}"#}}]});
        assert_eq!(estimated_tokens(std::slice::from_ref(&call)), 6);
        // 多模态：文字部分按字节，图片按 1000；思考也算
        let parts =
            json!({"role": "user", "content": [{"type": "text", "text": "abc"}, {"type": "image_url", "image_url": {"url": "data:"}}]});
        assert_eq!(estimated_tokens(&[parts]), (1_003.0 / 3.5) as i64);
        assert_eq!(estimated_tokens(&[json!({"role": "assistant", "content": "", "reasoning_content": "x".repeat(35)})]), 10);
    }

    fn step(index: usize, result: &str, tool: &str) -> Vec<Value> {
        vec![
            json!({"role": "assistant", "content": format!("第 {index} 步"), "reasoning_content": "想".repeat(500),
                   "tool_calls": [{"id": format!("c{index}"), "type": "function",
                                   "function": {"name": tool, "arguments": json!({"command": format!("{:<400}", format!("cat a{index}"))}).to_string()}}]}),
            json!({"role": "tool", "tool_call_id": format!("c{index}"), "name": tool, "content": result}),
        ]
    }

    /// == Swift 自检 sessionFolding 和 Qt TestContextAssist 的会话折叠
    #[test]
    fn session() {
        let brief = json!({"role": "user", "content": "任务：看看 a".to_string() + &"。".repeat(2_000)});
        let mut history = vec![brief.clone()];
        for index in 0..6 {
            history.extend(step(index, &("开头".to_string() + &"中".repeat(3_000) + "结尾"), "run_shell"));
        }
        history.insert(5, json!({"role": "user", "content": "（上级追加）也看看 b"}));
        history.extend(step(6, &("子 Agent 的结论".to_string() + &"论".repeat(3_000)), "delegate_task"));
        history.extend(step(7, &("最新".to_string() + &"新".repeat(20_000)), "run_shell"));
        let special = special();

        let mut folded = 0;
        let mut copy = history.clone();
        assert_eq!(apply(&mut copy, &mut folded, 1_000_000, false, &special), Applied::default(), "没超预算不折");
        assert_eq!(folded, 0);

        let before = estimated_tokens(&copy);
        let applied = apply(&mut copy, &mut folded, 30_000, false, &special);
        let after = estimated_tokens(&copy);
        assert!(applied.steps > 0 && after < before * 3 / 4, "折了 {} 步：{before} → {after}", applied.steps);
        assert_eq!(applied.after, after);
        assert_eq!(copy[0], brief, "用户消息一字不动");
        assert_eq!(copy[5]["content"], json!("（上级追加）也看看 b"));
        let first = copy[2]["content"].as_str().unwrap();
        assert!(first.starts_with("开头") && first.contains("已折叠，原 3004 字") && first.ends_with("结尾"));
        assert!(copy[1].get("reasoning_content").is_none() && copy[1]["content"] == json!("第 0 步"), "较早的思考去掉、短正文留着");
        let arguments = copy[1]["tool_calls"][0]["function"]["arguments"].as_str().unwrap();
        assert!(arguments.contains("cat a0") && arguments.contains("已折叠，原 400 字"), "{arguments}");
        let delegate = copy.iter().rposition(|entry| entry["name"] == json!("delegate_task")).unwrap();
        assert_eq!(copy[delegate]["content"], history[delegate]["content"], "子 Agent 的结果不截");
        assert_eq!(copy.last(), history.last(), "最新的一步原样");
        assert!(copy[copy.len() - 2].get("reasoning_content").is_some());
        assert!(folded > 0 && folded < copy.len() - 1);
        let again = copy.clone();
        assert_eq!(apply(&mut copy, &mut folded, 30_000, false, &special).steps, 0);
        assert_eq!(copy, again, "刚折过、没新内容：不再改（前缀缓存不失效）");

        // 要原样带回思考的接口：折叠时也留着
        let mut echo = history.clone();
        let mut echo_folded = 0;
        apply(&mut echo, &mut echo_folded, 30_000, true, &special);
        assert_eq!(echo[1]["reasoning_content"], history[1]["reasoning_content"]);
    }

    /// v2.86 == Swift SelfTests.sessionTrim / Qt TestContextAssist::sessionTrim：最早的整步去掉、用户消息不动（挨在一起的并成一条）、
    /// 说明写在后面第一条回答前面、位置跟着挪、没新折叠不再动
    #[test]
    fn trims() {
        let step = |index: usize| {
            vec![
                json!({ "role": "assistant", "content": format!("第 {index} 步"),
                        "tool_calls": [{ "id": format!("c{index}"), "type": "function", "function": { "name": "run_shell", "arguments": format!(r#"{{"command": "cat a{index}"}}"#) } }] }),
                json!({ "role": "tool", "tool_call_id": format!("c{index}"), "name": "run_shell", "content": "出".repeat(2_000) }),
            ]
        };
        let user = |text: &str| json!({ "role": "user", "content": text });
        let mut history = vec![user("上一轮"), json!({ "role": "assistant", "content": "好" }), user("这一轮的任务")];
        history.extend(step(0));
        history.extend(step(1));
        history.push(user("（工作中补充）也看看 b"));
        for index in 2..12 {
            history.extend(step(index));
        }
        let special = special();
        let mut folded = 2;
        let mut dropped = 0;
        apply(&mut history, &mut folded, 4_000, false, &special);
        let count = history.len();
        let note = |total: usize| format!("（第 7 轮较早的 {total} 步已省略，用 read_history 看）");
        let result = trim(&mut history, 2, &mut folded, &mut dropped, 1_500, &note);
        assert!(result.steps > 1 && dropped == result.steps && result.total == dropped && history.len() < count, "{result:?}");
        assert_eq!(history[0]["content"], "上一轮");
        assert_eq!(history[1]["content"], "好");
        assert_eq!(history[2]["content"], "这一轮的任务\n\n（工作中补充）也看看 b", "去掉后挨着的两条用户消息并成一条");
        assert_eq!(history[3]["role"], "assistant");
        assert!(history[3]["content"].as_str().unwrap().starts_with(&format!("（第 7 轮较早的 {} 步已省略", result.steps)));
        assert!(folded > 3 && folded < history.len() && history[folded]["role"] == "assistant", "折到哪跟着挪：{folded}");
        assert_eq!(history.last().unwrap()["content"].as_str().unwrap().chars().count(), 2_000, "最新的一步不动");
        let again = history.clone();
        assert_eq!(trim(&mut history, 2, &mut folded, &mut dropped, 1_500, &note).steps, 0, "没新折叠不再动");
        assert_eq!(history, again);
        // 上限 0 不去
        let mut untouched = again.clone();
        let mut zero = folded;
        assert_eq!(trim(&mut untouched, 2, &mut zero, &mut 0, 0, &note), Trimmed::default());
    }
}
