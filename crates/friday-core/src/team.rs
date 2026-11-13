//! 团队调度里两端共有的规则（== Swift Agent/AgentTeamControl；Qt agent/AgentTeamControl）：
//! - 派发前的检查：和这一级之前没做成的那次几乎一样（角色 + 任务）就拒绝、和这一级正在跑的几乎一样不再派一份、
//!   只派一个下级时不能把自己的任务原样往下传
//! - 交给父级的汇报、完成、失败、被停的写法；交给子智能体的补充消息前面的说明
//! - 第 2 层起附在任务后面的「你在整个任务里的位置」；id 对不上时列出现在有哪些子智能体
//!
//! 团队状态（在跑的、等验收的、走不通的路）留在两端：两端把要比的交过来。

use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;

use crate::agents::short_id;
use crate::liveness;
use crate::prompts::{self, Args};
use crate::repetition::similarity;

/// 给模型看的说明取提示词（prompts-framework.md 的 dispatch.*、agent-message.*、ancestry；以前写死在这里）
fn prompt(key: &str, pairs: &[(&str, String)]) -> String {
    let args: Args = pairs.iter().map(|(name, value)| (name.to_string(), value.clone())).collect();
    prompts::text(key, &args, None)
}

/// 只派一个下级、且它的任务和自己的任务相似到这个程度，就是原样往下传
pub const SHRINK_THRESHOLD: f64 = 0.8;
/// 重派没做成的事：角色 + 任务和失败那次相似到这个程度，就是没换提示词
pub const RETRY_THRESHOLD: f64 = 0.9;

/// 这次要派的一项
pub struct Item {
    pub role: String,
    pub task: String,
    pub engine: String,
}

/// 这一级之前没做成的一次（被打回、失败的）
pub struct Attempt {
    pub id: String,
    pub name: String,
    pub role: String,
    pub task: String,
    pub engine: String,
}

/// 这一级正在跑的一个
pub struct Running {
    pub id: String,
    pub name: String,
    pub role: String,
    pub assignment: String,
}

fn score(a: &str, b: &str) -> f64 {
    if a == b { 1.0 } else { similarity(a, b) }
}

/// 派发前的检查：不通过时是给模型看的说明。own：只派一个下级时（它的任务, 调用方（子智能体）自己的任务）
pub fn check_dispatch(items: &[Item], attempts: &[Attempt], running: &[Running], own: Option<(&str, &str)>) -> Option<String> {
    for item in items {
        for attempt in attempts {
            // v1.20：明确换了和那次不同的大脑，也算换了做法
            if !item.engine.is_empty() && !attempt.engine.is_empty() && item.engine != attempt.engine {
                continue;
            }
            let role = score(&item.role, &attempt.role);
            let task = score(&item.task, &attempt.task);
            if role < RETRY_THRESHOLD || task < RETRY_THRESHOLD {
                continue;
            }
            return Some(prompt(
                "dispatch.repeat-failed",
                &[
                    ("name", attempt.name.clone()),
                    ("role", ((role * 100.0) as i64).to_string()),
                    ("task", ((task * 100.0) as i64).to_string()),
                    ("id", short_id(&attempt.id)),
                ],
            ));
        }
        // v2.11 和这一级正在跑的几乎一样：不再派一份
        for run in running {
            let task = score(&item.task, &run.assignment);
            let role = if run.role.is_empty() { 1.0 } else { score(&item.role, &run.role) };
            if task < RETRY_THRESHOLD || role < RETRY_THRESHOLD {
                continue;
            }
            return Some(prompt(
                "dispatch.duplicate",
                &[("name", run.name.clone()), ("id", short_id(&run.id)), ("task", ((task * 100.0) as i64).to_string())],
            ));
        }
    }
    if let Some((single, own)) = own {
        let score = similarity(single, own);
        if score >= SHRINK_THRESHOLD {
            return Some(prompt("dispatch.passed-down", &[("score", ((score * 100.0) as i64).to_string())]));
        }
    }
    None
}

/// 交给子智能体的补充消息前面的说明（from：父级的名字；没有是用户补充的）
pub fn message_note(from: Option<&str>) -> String {
    match from {
        Some(from) => prompt("agent-message.parent", &[("from", from.to_string())]) + "\n",
        None => prompt("agent-message.user", &[]) + "\n",
    }
}

/// 汇报 / 完成通知交给父级的文字。kind：report / finished / failed / stopped / workflow / command / commandFinished
/// （v2.88 run_in_terminal 开的命令主动发来的：name 是终端标题、id 是终端 id）
pub fn notice_text(kind: &str, name: &str, id: &str, text: &str) -> String {
    let who = format!("子 Agent「{name}」（id：{}）", short_id(id));
    match kind {
        "report" => format!("[{who}中途汇报，它还在继续]\n{text}"),
        "finished" => format!("[{who}完成了]\n{text}"),
        "failed" => format!("[{who}失败了]\n{text}"),
        "workflow" => format!("[工作流（id：{}）的步骤全部结束了，等你验收]\n{text}", short_id(id)),
        "command" => format!("[终端 {}「{name}」里的命令发来消息，它还在跑]\n{text}", short_id(id)),
        "commandFinished" => format!("[终端 {}「{name}」里的命令跑完了]\n{text}", short_id(id)),
        // 「[子 Agent「X」被用户停止了，没做完。]」+ 做到一半的结果 + 建议
        _ => {
            let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
            let rest = rest.trim();
            let middle = if rest.is_empty() { String::new() } else { format!("\n{rest}") };
            format!("[{who}{first}]{middle}\n{}", liveness::advice(text))
        }
    }
}

/// 第 2 层起附在任务后面：从主智能体往下每一层派的任务（各截一段）。chain：[{giver?, name, assignment}]（从上往下；giver 为空是主智能体）
pub fn ancestry_note(chain: &[Value], me: &str) -> Option<String> {
    if chain.is_empty() {
        return None;
    }
    let clip = |text: &str| {
        let line = text.replace('\n', " ");
        let line = line.trim_matches([' ', '\t']);
        let parts: Vec<&str> = line.graphemes(true).collect();
        if parts.len() > 120 { format!("{}…", parts[..120].concat()) } else { line.to_string() }
    };
    let lines: Vec<String> = chain
        .iter()
        .map(|step| {
            let giver = step.get("giver").and_then(Value::as_str).map_or("主 Agent".to_string(), |giver| format!("「{giver}」"));
            let name = step.get("name").and_then(Value::as_str).unwrap_or("");
            format!("{giver}派给「{name}」：{}", clip(step.get("assignment").and_then(Value::as_str).unwrap_or("")))
        })
        .collect();
    Some(prompt("ancestry", &[("lines", lines.join("\n")), ("me", me.to_string()), ("depth", (chain.len() + 1).to_string())]))
}

/// id 对不上时给它看现在有哪些：running / waiting：[{id, name}]
pub fn children_summary(running: &[Value], waiting: &[Value]) -> String {
    let names = |items: &[Value]| {
        let mut list: Vec<String> = items
            .iter()
            .map(|item| {
                format!(
                    "「{}」（id {}）",
                    item.get("name").and_then(Value::as_str).unwrap_or(""),
                    short_id(item.get("id").and_then(Value::as_str).unwrap_or(""))
                )
            })
            .collect();
        list.sort();
        list
    };
    let mut parts = Vec::new();
    let running = names(running);
    let waiting = names(waiting);
    if !running.is_empty() {
        parts.push(format!("还在跑的：{}", running.join("、")));
    }
    if !waiting.is_empty() {
        parts.push(format!("交回了在等验收的：{}", waiting.join("、")));
    }
    if parts.is_empty() { "没有在跑、也没有在等验收的".to_string() } else { parts.join("；") }
}

/// 两端交来的检查参数 → 结构
pub fn check_from_json(params: &serde_json::Map<String, Value>) -> Option<String> {
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let array = |key: &str| params.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
    let items: Vec<Item> = array("items")
        .iter()
        .map(|item| Item { role: text(item, "role"), task: text(item, "task"), engine: text(item, "engine") })
        .collect();
    let attempts: Vec<Attempt> = array("attempts")
        .iter()
        .map(|item| Attempt {
            id: text(item, "id"),
            name: text(item, "name"),
            role: text(item, "role"),
            task: text(item, "task"),
            engine: text(item, "engine"),
        })
        .collect();
    let running: Vec<Running> = array("running")
        .iter()
        .map(|item| Running {
            id: text(item, "id"),
            name: text(item, "name"),
            role: text(item, "role"),
            assignment: text(item, "assignment"),
        })
        .collect();
    let single = params.get("single").and_then(Value::as_str);
    check_dispatch(&items, &attempts, &running, single.zip(params.get("own").and_then(Value::as_str)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 这里的说明都取提示词：装一份只有这几段的（== prompts-framework.md 里的原文）
    fn prompts_installed() -> std::sync::MutexGuard<'static, ()> {
        prompts::install_for_tests(
            "@@ dispatch.repeat-failed\n这次派发和之前没做成的「{{name}}」几乎一样（角色相似度 {{role}}%、任务相似度 {{task}}%），先 read_agent_log {{id}}\n\n\
             @@ dispatch.duplicate\n「{{name}}」（id {{id}}）正在做几乎一样的事（任务相似度 {{task}}%），这次没派\n\n\
             @@ dispatch.passed-down\n交给下级的任务和你自己的任务几乎一样（相似度 {{score}}%），这是把整个任务原样往下传。\n\n\
             @@ agent-message.parent\n（{{from}}在你工作时追加了下面的指示：和你的任务一起考虑，有冲突以它为准。）\n\n\
             @@ agent-message.user\n（用户在你工作时补充了下面这段话：和你的任务一起考虑，有冲突以它为准。）\n\n\
             @@ ancestry\n[你在整个任务里的位置（Friday 自动附上，只帮你理解背景；你只做上面交给你的那部分）]\n{{lines}}\n再往下就是你「{{me}}」（第 {{depth}} 层）。\n\n\
             @@ agent-ended.stopped\n不要重新派发同样的任务；按已有信息继续。\n",
        )
    }

    #[test]
    fn dispatch_checks() {
        let _prompts = prompts_installed();
        let item = |task: &str| Item { role: "调研员".into(), task: task.into(), engine: String::new() };
        let attempt = Attempt {
            id: "ABCDEF12-0".into(),
            name: "甲".into(),
            role: "调研员".into(),
            task: "查一下全球市场的竞品".into(),
            engine: "e1".into(),
        };
        let refused = check_dispatch(&[item("查一下全球市场的竞品")], std::slice::from_ref(&attempt), &[], None).unwrap();
        assert!(
            refused.starts_with("这次派发和之前没做成的「甲」几乎一样（角色相似度 100%、任务相似度 100%）")
                && refused.contains("read_agent_log abcdef12")
        );
        // 换了大脑：算换了做法
        let other = Item { engine: "e2".into(), ..item("查一下全球市场的竞品") };
        assert!(check_dispatch(&[other], &[attempt], &[], None).is_none());
        let running = Running { id: "11112222-x".into(), name: "乙".into(), role: String::new(), assignment: "整理 docs 目录".into() };
        assert!(
            check_dispatch(&[item("整理 docs 目录")], &[], &[running], None)
                .unwrap()
                .starts_with("「乙」（id 11112222）正在做几乎一样的事")
        );
        assert!(
            check_dispatch(&[item("整理 docs 目录")], &[], &[], Some(("整理 docs 目录", "整理 docs 目录")))
                .unwrap()
                .starts_with("交给下级的任务和你自己的任务几乎一样（相似度 100%）")
        );
        assert!(check_dispatch(&[item("a"), item("b")], &[], &[], None).is_none());
        assert!(check_from_json(json!({ "items": [{ "role": "", "task": "x" }] }).as_object().unwrap()).is_none());
    }

    #[test]
    fn texts() {
        let _prompts = prompts_installed();
        assert_eq!(message_note(Some("组长")), "（组长在你工作时追加了下面的指示：和你的任务一起考虑，有冲突以它为准。）\n");
        assert_eq!(notice_text("finished", "甲", "ABCDEF123", "好了"), "[子 Agent「甲」（id：abcdef12）完成了]\n好了");
        assert_eq!(
            notice_text("command", "构建", "ABCDEF123", "第 3 步好了"),
            "[终端 abcdef12「构建」里的命令发来消息，它还在跑]\n第 3 步好了"
        );
        assert_eq!(notice_text("commandFinished", "构建", "ABCDEF123", "退出码 0"), "[终端 abcdef12「构建」里的命令跑完了]\n退出码 0");
        let stopped = notice_text("stopped", "甲", "ABCDEF123", "被用户停止了，没做完。\n\n做到一半的结果");
        assert!(stopped.starts_with("[子 Agent「甲」（id：abcdef12）被用户停止了，没做完。]\n做到一半的结果\n不要重新派发"));
        let chain = vec![
            json!({ "name": "组长", "assignment": "整理文档\n全部" }),
            json!({ "giver": "组长", "name": "组员", "assignment": "docs/" }),
        ];
        assert_eq!(
            ancestry_note(&chain, "我").unwrap(),
            "[你在整个任务里的位置（Friday 自动附上，只帮你理解背景；你只做上面交给你的那部分）]\n主 Agent派给「组长」：整理文档 全部\n「组长」派给「组员」：docs/\n再往下就是你「我」（第 3 层）。"
        );
        assert!(ancestry_note(&[], "我").is_none());
        assert_eq!(
            children_summary(&[json!({ "id": "BBBB0000", "name": "乙" })], &[json!({ "id": "AAAA0000", "name": "甲" })]),
            "还在跑的：「乙」（id bbbb0000）；交回了在等验收的：「甲」（id aaaa0000）"
        );
        assert_eq!(children_summary(&[], &[]), "没有在跑、也没有在等验收的");
    }
}
