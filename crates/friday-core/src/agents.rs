//! 智能体团队里两端共有的规则（== Swift Agent/SubAgentRunner、Workflow；Qt agent/AgentRunner、Workflow）：
//! - 派子智能体的参数（task、name、role、engine、work、thinking、subagent_type）
//! - 子智能体的名字（给的 / 现成子智能体的 / 从角色「你是研究员」/ 从任务第一句取，重名加编号）和角色（现成子智能体的角色接上 role）
//! - v2.75 工作流（run_workflow）：计划的校验（步骤 id 不重复、依赖都在、没有环、每步都有验收标准）、按依赖推进（前置没做成的不做、
//!   依赖都做完的开始、全部结束交回）、交给主智能体的几段文字
//!
//! 跑子智能体、卡片、团队状态留在两端：两端问核心「下一步做哪些」，照着执行。

use std::collections::HashSet;

use regex::Regex;
use serde_json::{Map, Value, json};
use std::sync::LazyLock;

use crate::prompts::{self, Args};

/// JSON 值当文字（== Swift JSONValue.stringValue：字符串原样，数字、布尔写成文字）
pub fn string_value(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(match (number.as_i64(), number.as_f64()) {
            (Some(int), _) => int.to_string(),
            (None, Some(float)) if float.fract() == 0.0 && float.abs() < 1e15 => format!("{float:.1}"),
            (None, Some(float)) => float.to_string(),
            _ => number.to_string(),
        }),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// 对象参数（有的模型把对象写成 JSON 字符串，这里解开；== Swift JSONValue.asObject）
pub fn as_object(value: &Value) -> Option<Map<String, Value>> {
    match value {
        Value::Object(object) => Some(object.clone()),
        Value::String(text) => match serde_json::from_str(text) {
            Ok(Value::Object(object)) => Some(object),
            _ => None,
        },
        _ => None,
    }
}

fn trimmed(object: &Map<String, Value>, key: &str) -> Option<String> {
    string_value(object.get(key)).map(|text| text.trim().to_string()).filter(|text| !text.is_empty())
}

/// v2.6 说的活的种类（look / change / hard；中文也认）
pub fn parse_work(raw: &str) -> Option<&'static str> {
    match raw.trim().to_lowercase().as_str() {
        "look" | "查看" => Some("look"),
        "change" | "动手" => Some("change"),
        "hard" | "难题" => Some("hard"),
        _ => None,
    }
}

/// v2.5 点的深度思考（off / low / medium / high；中文也认）
pub fn parse_thinking(raw: &str) -> Option<&'static str> {
    match raw.trim().to_lowercase().as_str() {
        "off" | "关" | "关闭" | "none" => Some("off"),
        "low" | "低" => Some("low"),
        "medium" | "中" => Some("medium"),
        "high" | "高" => Some("high"),
        _ => None,
    }
}

/// 派子智能体的参数 → {task, name?, role?, engine?, work?, thinking?, agentType?}；不对时是给模型看的说明
pub fn task_spec(args: &Map<String, Value>) -> Result<Value, String> {
    let task = trimmed(args, "task").ok_or("缺少参数 task")?;
    let mut spec = json!({ "task": task });
    if let Some(raw) = trimmed(args, "work") {
        let work = parse_work(&raw).ok_or_else(|| format!("work 只能是 look、change、hard（不填跟随你自己的大脑）：{raw}"))?;
        spec["work"] = json!(work);
    }
    if let Some(raw) = trimmed(args, "thinking") {
        let thinking = parse_thinking(&raw).ok_or_else(|| format!("thinking 只能是 off、low、medium、high（不填按 work 定）：{raw}"))?;
        spec["thinking"] = json!(thinking);
    }
    // 旧字段 agent 当作名字用
    if let Some(name) = trimmed(args, "name").or_else(|| trimmed(args, "agent")) {
        spec["name"] = json!(name);
    }
    for (key, field) in [("role", "role"), ("engine", "engine"), ("subagent_type", "agentType")] {
        if let Some(value) = trimmed(args, key) {
            spec[field] = json!(value);
        }
    }
    Ok(spec)
}

// MARK: - 名字

static ROLE_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"你是(?:一[个名位])?([^，。,.：:、\s]{1,8})").unwrap());

/// 从「你是研究员，…」这类角色描述里取一个短名字
pub fn short_name(role: &str) -> Option<String> {
    ROLE_NAME.captures(role).map(|captures| captures[1].to_string())
}

/// v2.11 从任务的第一句取一个短名字（到第一个标点或换行，最多 8 个字）
pub fn task_name(task: &str) -> Option<String> {
    let mut first = task.trim().to_string();
    if let Some(end) = first.find(|c: char| "，。,：:；;！!？?（(\n".contains(c)) {
        first.truncate(end);
    }
    first = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(rest) = first.strip_prefix('请') {
        first = rest.trim().to_string();
    }
    if first.is_empty() {
        return None;
    }
    if first.chars().count() > 8 {
        return Some(first.chars().take(8).collect::<String>().trim().to_string());
    }
    Some(first)
}

/// v2.11 和 used 重名时加编号：「调研员」→「调研员2」「调研员3」…
pub fn unique_name(name: &str, used: &HashSet<String>) -> String {
    if !used.contains(name) {
        return name.to_string();
    }
    let mut index = 2;
    while used.contains(&format!("{name}{index}")) {
        index += 1;
    }
    format!("{name}{index}")
}

/// 子智能体的名字：给的 > 现成子智能体的 > 角色里的 > 任务第一句 > 「通才」，最多 12 个字，和 used 重名时加编号
pub fn agent_name(given: Option<&str>, definition: Option<&str>, role: Option<&str>, task: &str, used: &HashSet<String>) -> String {
    let name = given
        .map(str::to_string)
        .or_else(|| definition.map(str::to_string))
        .or_else(|| role.and_then(short_name))
        .or_else(|| task_name(task))
        .unwrap_or_else(|| "通才".to_string());
    unique_name(&name.chars().take(12).collect::<String>(), used)
}

/// v2.16 子智能体的角色：现成子智能体的角色（role 接在后面），否则 role，都没有是通才（提示词 sub.default-role）
pub fn composed_role(definition_role: Option<&str>, role: Option<&str>, override_id: Option<u64>) -> String {
    let default_role = || prompts::text("sub.default-role", &Args::new(), override_id);
    let Some(definition) = definition_role else { return role.map(str::to_string).unwrap_or_else(default_role) };
    let trimmed = definition.trim();
    let base = if trimmed.is_empty() { default_role() } else { trimmed.to_string() };
    match role {
        Some(role) => format!("{base}\n\n{role}"),
        None => base,
    }
}

/// 运行记录 id 的短写法：UUID 的前 8 位小写（== AgentLog.shortID）
pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect::<String>().to_lowercase()
}

// MARK: - 工作流

/// after：数组，或者写成「a, b」的字符串（去空白、去重）
fn step_keys(value: Option<&Value>) -> Vec<String> {
    let raw: Vec<String> = match value {
        Some(Value::Array(items)) => items.iter().filter_map(|item| string_value(Some(item))).collect(),
        Some(Value::String(text)) => text.split(|c: char| ",，、 ".contains(c)).map(str::to_string).collect(),
        Some(Value::Number(number)) => number.as_i64().map(|n| vec![n.to_string()]).unwrap_or_default(),
        _ => Vec::new(),
    };
    let mut keys: Vec<String> = Vec::new();
    for key in raw {
        let key = key.trim().to_string();
        if !key.is_empty() && !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// 解出来的一步：key、参数、依赖、验收标准、原参数
type PlannedStep = (String, Value, Vec<String>, String, Map<String, Value>);

/// run_workflow 的参数 → {goal, steps: [{key, spec, after, check, arguments}]}；不对时是给模型看的说明
pub fn workflow_plan(args: &Map<String, Value>) -> Result<Value, String> {
    let goal = trimmed(args, "goal").ok_or("缺少 goal：用户要的最终成品、怎样算完成")?;
    let items = match args.get("steps") {
        Some(Value::Array(items)) if !items.is_empty() => items,
        _ => return Err("steps 需要是非空数组，每项 {id, task, check, after?, name?, role?, work?}".into()),
    };
    let mut steps: Vec<PlannedStep> = Vec::new();
    for (offset, item) in items.iter().enumerate() {
        let object = as_object(item).ok_or("steps 的每一项都要是对象")?;
        let key = trimmed(&object, "id").unwrap_or_else(|| (offset + 1).to_string());
        if steps.iter().any(|step| step.0 == key) {
            return Err(format!("步骤 id「{key}」重复了：每一步的 id 要不一样"));
        }
        let spec = task_spec(&object)?;
        let check = trimmed(&object, "check").ok_or_else(|| format!("步骤「{key}」缺少 check：写清这一步怎样算合格（验收时按它检查）"))?;
        let after = step_keys(object.get("after"));
        steps.push((key, spec, after, check, object));
    }
    let keys: Vec<&str> = steps.iter().map(|step| step.0.as_str()).collect();
    for (key, _, after, _, _) in &steps {
        if let Some(missing) = after.iter().find(|other| !keys.contains(&other.as_str())) {
            return Err(format!("步骤「{key}」的 after 里有「{missing}」，没有这一步（现有：{}）", keys.join("、")));
        }
        if after.contains(key) {
            return Err(format!("步骤「{key}」的 after 里写了它自己"));
        }
    }
    // 有环就永远开始不了：按依赖一层层剥，剥不完的就是环
    let mut done: HashSet<&str> = HashSet::new();
    while done.len() < steps.len() {
        let ready: Vec<&str> = steps
            .iter()
            .filter(|step| !done.contains(step.0.as_str()) && step.2.iter().all(|key| done.contains(key.as_str())))
            .map(|step| step.0.as_str())
            .collect();
        if ready.is_empty() {
            let stuck: Vec<&str> = steps.iter().map(|step| step.0.as_str()).filter(|key| !done.contains(key)).collect();
            return Err(format!("步骤之间的依赖绕成了圈，开始不了：{}", stuck.join("、")));
        }
        done.extend(ready);
    }
    let steps: Vec<Value> = steps
        .into_iter()
        .map(|(key, spec, after, check, arguments)| json!({ "key": key, "spec": spec, "after": after, "check": check, "arguments": arguments }))
        .collect();
    Ok(json!({ "goal": goal, "steps": steps }))
}

/// 一步的结果
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    Done,
    Failed,
    Skipped,
}

impl Outcome {
    pub fn parse(text: &str) -> Option<Outcome> {
        match text {
            "done" => Some(Outcome::Done),
            "failed" => Some(Outcome::Failed),
            "skipped" => Some(Outcome::Skipped),
            _ => None,
        }
    }
}

/// 推进一次：前置没做成的（或工作流被停了）标成没做，依赖都做完的开始；返回
/// {skip: [{index, reason}], launch: [index], finished}（finished：算上这次标的，全部步骤都有结果了）
pub fn workflow_advance(
    keys: &[String],
    names: &[String],
    after: &[Vec<String>],
    outcomes: &mut [Option<Outcome>],
    started: &HashSet<usize>,
    stopped: bool,
) -> Value {
    let index_of = |key: &str| keys.iter().position(|other| other == key);
    let mut skip = Vec::new();
    let mut changed = true;
    while changed {
        changed = false;
        for index in 0..keys.len() {
            if outcomes[index].is_some() || started.contains(&index) {
                continue;
            }
            let blocker = after[index]
                .iter()
                .filter_map(|key| index_of(key))
                .find(|&other| outcomes[other].is_some_and(|outcome| outcome != Outcome::Done));
            if !stopped && blocker.is_none() {
                continue;
            }
            let reason = match blocker {
                Some(other) => format!("前置步骤 {}「{}」没做成，这一步没有做", keys[other], names.get(other).map_or("", String::as_str)),
                None => "工作流被停止了，这一步没有做".to_string(),
            };
            outcomes[index] = Some(Outcome::Skipped);
            skip.push(json!({ "index": index, "reason": reason }));
            changed = true;
        }
    }
    let launch: Vec<usize> = (0..keys.len())
        .filter(|&index| outcomes[index].is_none() && !started.contains(&index))
        .filter(|&index| after[index].iter().all(|key| index_of(key).is_some_and(|other| outcomes[other] == Some(Outcome::Done))))
        .collect();
    let finished = outcomes.iter().all(Option::is_some);
    json!({ "skip": skip, "launch": launch, "finished": finished })
}

/// 交给主智能体的「已启动」：每步什么时候开始、id。steps：[{key, name, card, after}]
pub fn workflow_started(id: &str, steps: &[Value], override_id: Option<u64>) -> String {
    let lines: Vec<String> = steps
        .iter()
        .map(|step| {
            let after: Vec<&str> = step["after"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let when = if after.is_empty() { "马上开始".to_string() } else { format!("等 {} 做完", after.join("、")) };
            format!(
                "- {}「{}」（id：{}）：{when}",
                step["key"].as_str().unwrap_or(""),
                step["name"].as_str().unwrap_or(""),
                short_id(step["card"].as_str().unwrap_or(""))
            )
        })
        .collect();
    let args = Args::from([
        ("id".to_string(), short_id(id)),
        ("count".to_string(), steps.len().to_string()),
        ("steps".to_string(), lines.join("\n")),
    ]);
    prompts::text("workflow.started", &args, override_id)
}

/// 一步的任务：目标、这一步的任务和验收标准，要用前面产出的附上它们的结果。upstream：[{key, name, result}]
pub fn workflow_step_task(goal: &str, task: &str, check: &str, upstream: &[Value], override_id: Option<u64>) -> String {
    let parts: Vec<String> = upstream
        .iter()
        .map(|item| {
            let args = Args::from([
                ("key".to_string(), item["key"].as_str().unwrap_or("").to_string()),
                ("name".to_string(), item["name"].as_str().unwrap_or("").to_string()),
                ("result".to_string(), item["result"].as_str().unwrap_or("").to_string()),
            ]);
            prompts::text("workflow.step.upstream", &args, override_id)
        })
        .collect();
    let upstream_text = if parts.is_empty() {
        String::new()
    } else {
        prompts::text("workflow.step.upstream-head", &Args::new(), override_id) + "\n\n" + &parts.join("\n\n")
    };
    let args = Args::from([
        ("goal".to_string(), goal.to_string()),
        ("task".to_string(), task.to_string()),
        ("check".to_string(), check.to_string()),
        ("upstream".to_string(), upstream_text),
    ]);
    prompts::text("workflow.step", &args, override_id)
}

/// 交回主智能体的验收材料。steps：[{key, name, card, outcome: done|failed|skipped|null, check, result}]
pub fn workflow_report(goal: &str, steps: &[Value], override_id: Option<u64>) -> String {
    let sections: Vec<String> = steps
        .iter()
        .map(|step| {
            let state = match step["outcome"].as_str().and_then(Outcome::parse) {
                Some(Outcome::Done) => "做完了",
                Some(Outcome::Failed) => "没做成",
                _ => "没有做",
            };
            let args = Args::from([
                ("key".to_string(), step["key"].as_str().unwrap_or("").to_string()),
                ("name".to_string(), step["name"].as_str().unwrap_or("").to_string()),
                ("id".to_string(), short_id(step["card"].as_str().unwrap_or(""))),
                ("state".to_string(), state.to_string()),
                ("check".to_string(), step["check"].as_str().unwrap_or("").to_string()),
                ("result".to_string(), step["result"].as_str().unwrap_or("").to_string()),
            ]);
            prompts::text("workflow.done.step", &args, override_id)
        })
        .collect();
    let args = Args::from([("goal".to_string(), goal.to_string()), ("steps".to_string(), sections.join("\n\n"))]);
    prompts::text("workflow.done", &args, override_id)
}

/// 工作流卡片定稿时的一句：全部做完 / 几步做完几步没做成
pub fn workflow_summary(done: usize, total: usize) -> String {
    if done == total {
        format!("工作流 {done} 步全部做完，等验收")
    } else {
        format!("工作流结束：{done} 步做完，{} 步没做成", total - done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn task_specs() {
        let spec = task_spec(&object(
            json!({ "task": " 查资料 ", "agent": "老张", "work": "难题", "thinking": "HIGH", "subagent_type": "reviewer" }),
        ))
        .unwrap();
        assert_eq!(spec, json!({ "task": "查资料", "name": "老张", "work": "hard", "thinking": "high", "agentType": "reviewer" }));
        assert_eq!(task_spec(&object(json!({ "task": "  " }))).unwrap_err(), "缺少参数 task");
        assert!(task_spec(&object(json!({ "task": "x", "work": "fast" }))).unwrap_err().starts_with("work 只能是"));
        assert!(task_spec(&object(json!({ "task": "x", "thinking": "max" }))).unwrap_err().starts_with("thinking 只能是"));
        assert_eq!(task_spec(&object(json!({ "task": 42 }))).unwrap()["task"], "42");
    }

    #[test]
    fn names() {
        assert_eq!(short_name("你是一名研究员，负责…").as_deref(), Some("研究员"));
        assert_eq!(short_name("你是审查员"), Some("审查员".to_string()));
        assert_eq!(task_name("请 帮我查一下天气，然后…").as_deref(), Some("帮我查一下天气"));
        assert_eq!(task_name("调研一下全球市场上所有的竞品情况").as_deref(), Some("调研一下全球市场"));
        assert_eq!(task_name("（只有括号"), None);
        let used: HashSet<String> = ["通才".to_string(), "通才2".to_string()].into();
        assert_eq!(unique_name("通才", &used), "通才3");
        assert_eq!(agent_name(None, None, None, "。", &used), "通才3");
        assert_eq!(agent_name(Some("一个特别特别特别长的名字超过十二个"), None, None, "", &HashSet::new()), "一个特别特别特别长的名字");
        assert_eq!(agent_name(None, Some("reviewer"), Some("你是研究员"), "t", &HashSet::new()), "reviewer");
        assert_eq!(agent_name(None, None, Some("你是研究员"), "t", &HashSet::new()), "研究员");
        assert_eq!(short_id("ABCDEF12-3456"), "abcdef12");
        assert_eq!(composed_role(Some("  审查  "), Some("看测试"), None), "审查\n\n看测试");
        assert_eq!(composed_role(None, Some("r"), None), "r");
    }

    #[test]
    fn workflow_plans() {
        let plan = workflow_plan(&object(json!({ "goal": "做完", "steps": [
            { "id": "a", "task": "查", "check": "有结果" },
            { "task": "写", "check": "能跑", "after": "a， a" },
            "{\"id\":\"c\",\"task\":\"测\",\"check\":\"过\",\"after\":[\"a\",2]}",
        ] })))
        .unwrap();
        assert_eq!(plan["steps"][1]["key"], "2");
        assert_eq!(plan["steps"][1]["after"], json!(["a"]));
        assert_eq!(plan["steps"][2]["after"], json!(["a", "2"]));
        let error = |args: Value| workflow_plan(&object(args)).unwrap_err();
        assert_eq!(error(json!({ "steps": [] })), "缺少 goal：用户要的最终成品、怎样算完成");
        assert!(error(json!({ "goal": "g", "steps": [] })).starts_with("steps 需要是非空数组"));
        assert_eq!(error(json!({ "goal": "g", "steps": [3] })), "steps 的每一项都要是对象");
        assert_eq!(
            error(json!({ "goal": "g", "steps": [{ "id": "a", "task": "t", "check": "c" }, { "id": "a", "task": "t", "check": "c" }] })),
            "步骤 id「a」重复了：每一步的 id 要不一样"
        );
        assert_eq!(
            error(json!({ "goal": "g", "steps": [{ "id": "a", "task": "t" }] })),
            "步骤「a」缺少 check：写清这一步怎样算合格（验收时按它检查）"
        );
        assert_eq!(
            error(json!({ "goal": "g", "steps": [{ "id": "a", "task": "t", "check": "c", "after": "z" }] })),
            "步骤「a」的 after 里有「z」，没有这一步（现有：a）"
        );
        assert_eq!(
            error(json!({ "goal": "g", "steps": [{ "id": "a", "task": "t", "check": "c", "after": "a" }] })),
            "步骤「a」的 after 里写了它自己"
        );
        assert_eq!(
            error(json!({ "goal": "g", "steps": [
                { "id": "a", "task": "t", "check": "c", "after": "b" }, { "id": "b", "task": "t", "check": "c", "after": "a" },
                { "id": "c", "task": "t", "check": "c" }] })),
            "步骤之间的依赖绕成了圈，开始不了：a、b"
        );
    }

    #[test]
    fn workflow_advances() {
        let keys: Vec<String> = ["a", "b", "c", "d"].map(String::from).to_vec();
        let names: Vec<String> = ["甲", "乙", "丙", "丁"].map(String::from).to_vec();
        let after = vec![vec![], vec!["a".to_string()], vec!["a".to_string()], vec!["b".to_string(), "c".to_string()]];
        let mut outcomes = vec![None; 4];
        let mut started = HashSet::new();
        let first = workflow_advance(&keys, &names, &after, &mut outcomes, &started, false);
        assert_eq!(first, json!({ "skip": [], "launch": [0], "finished": false }));
        started.insert(0);
        outcomes[0] = Some(Outcome::Done);
        assert_eq!(workflow_advance(&keys, &names, &after, &mut outcomes, &started, false)["launch"], json!([1, 2]));
        started.extend([1, 2]);
        outcomes[1] = Some(Outcome::Failed);
        // b 没做成：d 不做；c 还在跑，没结束
        let next = workflow_advance(&keys, &names, &after, &mut outcomes, &started, false);
        assert_eq!(next["skip"], json!([{ "index": 3, "reason": "前置步骤 b「乙」没做成，这一步没有做" }]));
        assert_eq!(next["finished"], false);
        outcomes[2] = Some(Outcome::Done);
        assert_eq!(workflow_advance(&keys, &names, &after, &mut outcomes, &started, false)["finished"], true);
        // 被停了：没开始的都不做
        let mut fresh = vec![None; 4];
        let stopped = workflow_advance(&keys, &names, &after, &mut fresh, &HashSet::from([0]), true);
        assert_eq!(stopped["skip"].as_array().unwrap().len(), 3);
        assert_eq!(stopped["skip"][0]["reason"], "工作流被停止了，这一步没有做");
        assert_eq!(stopped["finished"], false);
        assert_eq!(workflow_summary(2, 2), "工作流 2 步全部做完，等验收");
        assert_eq!(workflow_summary(1, 3), "工作流结束：1 步做完，2 步没做成");
    }
}
