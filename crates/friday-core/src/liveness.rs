//! 子智能体一直不返回时怎么办（== Swift Agent/AgentLiveness、AgentResults；Qt agent/AgentLiveness、AgentResults）：
//! - 看门狗：只有「生成中」长时间没有任何输出才算卡住（工具有自己的超时，等下级、等确认、排队都不算），停掉并交回半成品
//! - 期限（可选）：派发时给 deadline_seconds，下级的期限不超过父级剩下的时间；到点先提醒它交付，宽限一会儿还没交就停掉
//! - 原地打转：同样的参数连续调用同一个工具（轮询类的不算），第 3 次提醒，第 5 次拦下
//! - 被停止、失败时交回做到一半的结果：已经写出的内容、下级交回的结果、执行过的操作；写得长的全文存成文件
//! - 交给上级的结果太长时存全文、只交开头和结尾 + 文件路径
//!
//! 定时巡查、停掉、发提醒、团队状态留在两端：两端每次巡查把在跑的子智能体交过来，核心说停哪个、提醒哪个。

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};
use unicode_segmentation::UnicodeSegmentation;

use crate::agents::short_id;
use crate::chat_models::{BlockKind, MessageBlock};

/// 期限到了之后再给这么久交付（秒）
pub const DEADLINE_GRACE: f64 = 60.0;
/// 同样的调用连续第几次提醒、第几次拦下
pub const REPEAT_WARN: i64 = 3;
pub const REPEAT_BLOCK: i64 = 5;
/// 子智能体每调用这么多次工具提醒一次检查进度
pub const STEP_NUDGE: i64 = 40;
/// 交给上级的结果最多这么多字（再长存全文）
pub const RESULT_LIMIT: usize = 4000;
/// 存的全文保留几天
const KEEP_DAYS: u64 = 7;

/// 重复调用不算打转的工具：本来就是轮询（v2.2 read_terminal：等开发服务器起来时会连着读几次）
pub const POLLING_TOOLS: [&str; 6] =
    ["wait_agents", "check_job", "get_current_time", "read_terminal", "browser_read", "browser_screenshot"];

/// 被停掉、失败了：写在卡片上、交给父级的说明
pub fn ended_text(stopped_by: Option<&str>, message: &str, partial: Option<&str>) -> String {
    let head = match stopped_by {
        Some(by) => format!("被{by}停止了，没做完。"),
        None => format!("失败了：{message}"),
    };
    match partial {
        Some(partial) => format!("{head}\n\n{partial}"),
        None => head,
    }
}

/// 给父级的建议（按卡片文字的开头判断）：用户 / 父级停的别再派；看门狗、期限停的可以换个做法或收尾；失败的看原因
pub fn advice(card_text: &str) -> &'static str {
    let failed = card_text.starts_with("失败了");
    // v2.11 模型服务出错、会话留着：打回接着做，不从头派；还没开始做、大脑歇着没派：等一会儿原样重派
    if failed && card_text.contains("它的会话还留着") {
        return "是模型服务出错：等一会儿用 rework_agent 打回它接着做（写清从哪接着做），一直不行就换个模型重派，或告诉用户。";
    }
    if failed && card_text.contains("原样重派就行") {
        return "是模型服务出错：等一会儿原样重派（不用改提示词），一直不行就换个模型，或告诉用户。";
    }
    if failed {
        return "判断失败原因：能修就换个做法（拆小一点、换个大脑）再派，不行就自己做或告诉用户。";
    }
    if card_text.starts_with("被看门狗") {
        return "它卡住了：上面做到一半的结果可以直接用；剩下的换个做法（拆小一点、换个大脑）再派，或者自己做。";
    }
    if card_text.starts_with("被期限") {
        return "时间到了：用上面做到一半的结果收尾，没做完的在回答里说明。";
    }
    "不要重新派发同样的任务；按已有信息继续（上面做到一半的结果可以直接用），或告诉用户需要什么。"
}

/// 时长的写法：N 秒 / N 分 M 秒 / N 小时 M 分
pub fn duration(seconds: f64) -> String {
    let total = seconds.max(0.0) as i64;
    if total < 60 {
        return format!("{total} 秒");
    }
    if total < 3600 {
        return format!("{} 分 {} 秒", total / 60, total % 60);
    }
    format!("{} 小时 {} 分", total / 3600, total % 3600 / 60)
}

/// 巡查时一个在跑的子智能体
pub struct Watched {
    pub id: String,
    pub name: String,
    /// 期限（Unix 毫秒）
    pub deadline: Option<f64>,
    pub warned: bool,
    pub generating: bool,
    /// 最近一次输出（Unix 毫秒）
    pub last_output: Option<f64>,
}

/// 巡查一次：过了期限 + 宽限的停掉；到了期限的提醒一次；生成中太久没有输出的停掉。
/// 返回 [{id, action: stop | warn, by?, text?, journal?}]（stop 的 by 是「被谁停的」，journal 是诊断日志的标题）
pub fn watchdog(now: f64, stall_limit: f64, grace: f64, runs: &[Watched]) -> Vec<Value> {
    let mut actions = Vec::new();
    for run in runs {
        if let Some(deadline) = run.deadline
            && now >= deadline
        {
            if now >= deadline + grace * 1000.0 {
                actions.push(json!({ "id": run.id, "action": "stop", "by": format!("期限（到点后又等了 {} 秒）", grace as i64),
                    "journal": format!("子 Agent「{}」过了期限还没交付，已停掉", run.name) }));
                continue;
            }
            if !run.warned {
                actions.push(json!({ "id": run.id, "action": "warn", "text": format!(
                    "派发时给的时间到了：立刻停下手上的事，交付已有结果，没做完的写进「未完成」。{} 秒内不交就会被停掉。", grace as i64) }));
            }
        }
        if run.generating
            && let Some(last) = run.last_output
            && (now - last) / 1000.0 > stall_limit
        {
            let span = duration(stall_limit);
            actions.push(json!({ "id": run.id, "action": "stop", "by": format!("看门狗（{span}没有任何输出）"),
                "journal": format!("子 Agent「{}」{span}没有任何输出，看门狗停掉了它", run.name) }));
        }
    }
    actions
}

/// 下级的期限：自己给的秒数和父级剩下的时间取早的；都没有就不限时（Unix 毫秒）
pub fn child_deadline(seconds: Option<f64>, inherited: Option<f64>, now: f64) -> Option<f64> {
    let own = seconds.filter(|seconds| *seconds > 0.0).map(|seconds| now + seconds * 1000.0);
    match (own, inherited) {
        (Some(own), Some(inherited)) => Some(own.min(inherited)),
        (own, inherited) => own.or(inherited),
    }
}

/// 记下这次调用，返回同样的调用连续出现了几次和这次的签名（轮询类工具不算，返回 0；主智能体的 wait_agents 不是真在等，照常算）
pub fn repeat_count(name: &str, arguments: &str, depth: i64, last: Option<(&str, i64)>) -> (i64, String) {
    let main_waits = name == "wait_agents" && depth == 0;
    if POLLING_TOOLS.contains(&name) && !main_waits {
        return (0, String::new());
    }
    let signature = format!("{name}\u{1}{arguments}");
    let count = match last {
        Some((previous, count)) if previous == signature => count + 1,
        _ => 1,
    };
    (count, signature)
}

/// 等它做完再返回的子智能体：它中途的汇报并进最终结果
pub fn with_reports(text: &str, reports: &[String]) -> String {
    if reports.is_empty() {
        return text.to_string();
    }
    let lines: Vec<String> = reports.iter().map(|report| format!("- {report}")).collect();
    format!("{text}\n\n（它工作中途向你汇报过：\n{}）", lines.join("\n"))
}

fn graphemes(text: &str) -> Vec<&str> {
    text.graphemes(true).collect()
}

/// 去首尾空白，超过 limit 字取「…」+ 最后 limit 字
fn clip_tail(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    let parts = graphemes(trimmed);
    if parts.len() > limit { format!("…{}", parts[parts.len() - limit..].concat()) } else { trimmed.to_string() }
}

/// 全文存成 <运行记录 id 前 8 位>.md（同一个子智能体打回后再交，覆盖上一次的），顺手删掉 7 天前的；返回路径，写不了是 None
pub fn save_result(text: &str, id: &str, directory: &Path) -> Option<String> {
    std::fs::create_dir_all(directory).ok()?;
    if let Ok(entries) = std::fs::read_dir(directory) {
        let cutoff = SystemTime::now().checked_sub(Duration::from_secs(KEEP_DAYS * 86_400));
        for entry in entries.flatten() {
            let path = entry.path();
            let old =
                entry.metadata().and_then(|meta| meta.modified()).ok().zip(cutoff).is_some_and(|(modified, cutoff)| modified < cutoff);
            if path.extension().is_some_and(|ext| ext == "md") && old {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    let path = directory.join(format!("{}.md", short_id(id)));
    std::fs::write(&path, text).ok()?;
    Some(path.to_string_lossy().into_owned())
}

/// 交给上级的正文：不超过 RESULT_LIMIT 字原样返回；超过就存全文、交回开头 3/4 和结尾 1/4，中间写明全文在哪
pub fn clip_result(text: &str, id: &str, directory: &Path) -> String {
    let parts = graphemes(text);
    if parts.len() <= RESULT_LIMIT {
        return text.to_string();
    }
    let head = RESULT_LIMIT * 3 / 4;
    let tail = RESULT_LIMIT - head;
    let place = match save_result(text, id, directory) {
        Some(path) => format!("全文存在 {path}——要细节就用 read_file 分段读，后面派的子 Agent 要用就把这个路径写进它的 task"),
        None => "全文没能存成文件，要细节用 read_agent_log 看它的运行记录".to_string(),
    };
    format!(
        "{}\n…（结果共 {} 字，中间省略 {} 字；{place}）…\n{}",
        parts[..head].concat(),
        parts.len(),
        parts.len() - head - tail,
        parts[parts.len() - tail..].concat()
    )
}

/// 团队之间来回的工具（记进「执行过的操作」没意义）
const TEAM_CHATTER: [&str; 6] = ["agent_message", "report_to_parent", "read_agent_log", "wait_agents", "message_agent", "stop_agent"];

/// 子智能体停下时做到哪了：已经写出的内容、下级交回的结果、执行过的操作。什么都没有是 None
pub fn partial_result(id: &str, blocks: &[MessageBlock], final_text: &str, directory: &Path) -> Option<String> {
    let mut children = Vec::new();
    let mut actions: BTreeMap<String, i64> = BTreeMap::new();
    fn collect(blocks: &[MessageBlock], children: &mut Vec<String>, actions: &mut BTreeMap<String, i64>) {
        for block in blocks {
            let BlockKind::Tool(event) = &block.kind else { continue };
            if event.status != "succeeded" {
                continue;
            }
            match event.name.as_str() {
                "delegate_task" => {
                    let name = event.extra.get("agentName").and_then(Value::as_str).unwrap_or("子 Agent");
                    children.push(format!("「{name}」：{}", clip_tail(&event.result, 400)));
                }
                "run_parallel_tasks" | "run_workflow" => collect(event.transcript.as_deref().unwrap_or(&[]), children, actions),
                name if TEAM_CHATTER.contains(&name) => {}
                name => {
                    let title = event.extra.get("title").and_then(Value::as_str).unwrap_or(name);
                    *actions.entry(title.to_string()).or_default() += 1;
                }
            }
        }
    }
    collect(blocks, &mut children, &mut actions);
    let mut parts = Vec::new();
    let trimmed = final_text.trim();
    if !trimmed.is_empty() {
        // v1.20：写得长的（比如快写完的报告）全文存成文件，上级要细节按路径读，不只剩最后一段
        let count = graphemes(trimmed).len();
        let path = if count > 1500 { save_result(trimmed, id, directory) } else { None };
        let head = match path {
            Some(path) => format!("它停下前已经写出的内容（共 {count} 字，全文存在 {path}；下面是最后一段）：\n"),
            None => "它停下前已经写出的内容：\n".to_string(),
        };
        parts.push(head + &clip_tail(final_text, 1500));
    }
    if !children.is_empty() {
        let lines: Vec<String> = children.iter().map(|child| format!("- {child}")).collect();
        parts.push(format!("它的下级已经交回的结果：\n{}", lines.join("\n")));
    }
    if !actions.is_empty() {
        let list: Vec<String> =
            actions.iter().map(|(name, count)| if *count > 1 { format!("{name} ×{count}") } else { name.clone() }).collect();
        parts.push(format!("它已经执行过的操作：{}（改动可能只做了一半，要核对）", list.join("、")));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("做到一半的结果（运行记录 id：{}）：\n{}", short_id(id), parts.join("\n\n")))
}

/// 两端交来的工具名集合里有没有轮询类的（测试用）
pub fn is_polling(name: &str) -> bool {
    POLLING_TOOLS.iter().copied().collect::<HashSet<_>>().contains(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts() {
        assert_eq!(ended_text(Some("用户"), "", None), "被用户停止了，没做完。");
        assert_eq!(ended_text(None, "网络断了", Some("做了一半")), "失败了：网络断了\n\n做了一半");
        assert!(advice("失败了：x\n\n它的会话还留着").contains("rework_agent"));
        assert!(advice("失败了：x 原样重派就行").contains("原样重派"));
        assert!(advice("被看门狗（5 分 0 秒没有任何输出）停止了").starts_with("它卡住了"));
        assert!(advice("被期限停了").starts_with("时间到了"));
        assert!(advice("被用户停止了").starts_with("不要重新派发"));
        assert_eq!(duration(59.9), "59 秒");
        assert_eq!(duration(300.0), "5 分 0 秒");
        assert_eq!(duration(3725.0), "1 小时 2 分");
        assert_eq!(with_reports("结果", &["进度 1".into()]), "结果\n\n（它工作中途向你汇报过：\n- 进度 1）");
    }

    #[test]
    fn watchdog_rules() {
        let run = |deadline: Option<f64>, warned: bool, generating: bool, last: Option<f64>| Watched {
            id: "A".into(),
            name: "甲".into(),
            deadline,
            warned,
            generating,
            last_output: last,
        };
        let now = 1_000_000.0;
        assert!(watchdog(now, 300.0, 60.0, &[run(None, false, true, Some(now - 10_000.0))]).is_empty());
        let stalled = watchdog(now, 300.0, 60.0, &[run(None, false, true, Some(now - 301_000.0))]);
        assert_eq!(stalled[0]["by"], "看门狗（5 分 0 秒没有任何输出）");
        assert_eq!(stalled[0]["journal"], "子 Agent「甲」5 分 0 秒没有任何输出，看门狗停掉了它");
        // 在跑工具（不是生成中）不算卡住
        assert!(watchdog(now, 300.0, 60.0, &[run(None, false, false, Some(0.0))]).is_empty());
        let warn = watchdog(now, 300.0, 60.0, &[run(Some(now - 1.0), false, false, None)]);
        assert_eq!(warn[0]["action"], "warn");
        assert!(watchdog(now, 300.0, 60.0, &[run(Some(now - 1.0), true, false, None)]).is_empty());
        let late = watchdog(now, 300.0, 60.0, &[run(Some(now - 61_000.0), true, true, Some(0.0))]);
        assert_eq!(late.len(), 1);
        assert_eq!(late[0]["by"], "期限（到点后又等了 60 秒）");
        assert_eq!(child_deadline(Some(10.0), Some(5_000.0), 0.0), Some(5_000.0));
        assert_eq!(child_deadline(Some(0.0), None, 0.0), None);
        assert_eq!(child_deadline(Some(1.0), None, 0.0), Some(1000.0));
    }

    #[test]
    fn repeats() {
        assert_eq!(repeat_count("read_terminal", "{}", 1, None).0, 0);
        assert_eq!(repeat_count("wait_agents", "{}", 1, None).0, 0);
        let (first, signature) = repeat_count("wait_agents", "{}", 0, None);
        assert_eq!(first, 1);
        assert_eq!(repeat_count("wait_agents", "{}", 0, Some((&signature, 2))).0, 3);
        assert_eq!(repeat_count("read_file", "{\"a\":1}", 1, Some((&signature, 2))).0, 1);
        assert!(is_polling("browser_read"));
    }

    #[test]
    fn partial_and_clip() {
        let dir = std::env::temp_dir().join(format!("friday-results-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let blocks: Vec<MessageBlock> = serde_json::from_value(json!([
            { "id": "B1", "kind": { "tool": { "_0": { "id": "T1", "name": "delegate_task", "arguments": "{}", "status": "succeeded",
                "agentName": "乙", "result": "查完了" } } } },
            { "id": "B2", "kind": { "tool": { "_0": { "id": "T2", "name": "run_shell", "arguments": "{}", "status": "succeeded", "title": "跑命令" } } } },
            { "id": "B3", "kind": { "tool": { "_0": { "id": "T3", "name": "run_shell", "arguments": "{}", "status": "succeeded", "title": "跑命令" } } } },
            { "id": "B4", "kind": { "tool": { "_0": { "id": "T4", "name": "wait_agents", "arguments": "{}", "status": "succeeded" } } } },
            { "id": "B5", "kind": { "tool": { "_0": { "id": "T5", "name": "read_file", "arguments": "{}", "status": "failed" } } } },
        ]))
        .unwrap();
        let text = partial_result("ABCDEF123456", &blocks, "  写了一点  ", &dir).unwrap();
        assert_eq!(
            text,
            "做到一半的结果（运行记录 id：abcdef12）：\n它停下前已经写出的内容：\n写了一点\n\n它的下级已经交回的结果：\n- 「乙」：查完了\n\n它已经执行过的操作：跑命令 ×2（改动可能只做了一半，要核对）"
        );
        assert!(partial_result("X", &[], "  ", &dir).is_none());
        let long = "字".repeat(2000);
        let saved = partial_result("ABCDEF123456", &[], &long, &dir).unwrap();
        assert!(saved.contains("共 2000 字，全文存在 ") && saved.contains("abcdef12.md"));
        let clipped = clip_result(&"x".repeat(5000), "12345678-AAAA", &dir);
        assert!(clipped.starts_with(&"x".repeat(3000)) && clipped.contains("结果共 5000 字，中间省略 1000 字；全文存在 "));
        assert_eq!(clip_result("短", "id", &dir), "短");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
