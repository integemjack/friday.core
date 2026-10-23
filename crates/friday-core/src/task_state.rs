//! 一段对话的任务状态（== Swift Agent/TaskState；Qt agent/TaskState）：目标、用户的约束、已确认的决定、进度、之后要用的关键信息。
//! 主智能体用 update_task_state 维护；每条用户消息发出时把当时的状态附在后面，对话再长、较早的轮次折叠了也不会忘。
//! 状态不单独存：沿当前路径重放 update_task_state 调用得到。
//!
//! 这里是规则：一次调用怎么改状态、渲染给模型的写法、两份之间改了什么、附在消息后面的样子、子智能体背景里附的用户原话。
//! 消息历史、状态的数据结构留在两端：两端把要用的（调用的参数、用户消息的文字）抽出来交过来。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use unicode_segmentation::UnicodeSegmentation;

use crate::agents::{as_object, string_value};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub text: String,
    /// todo / doing / done
    pub status: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskState {
    pub goal: String,
    pub constraints: Vec<String>,
    pub decisions: Vec<String>,
    pub steps: Vec<Step>,
    pub facts: Vec<String>,
}

fn count(text: &str) -> usize {
    text.graphemes(true).count()
}

fn prefix(text: &str, limit: usize) -> String {
    text.graphemes(true).take(limit).collect()
}

impl TaskState {
    pub fn is_empty(&self) -> bool {
        self.goal.is_empty() && self.constraints.is_empty() && self.decisions.is_empty() && self.steps.is_empty() && self.facts.is_empty()
    }

    /// 应用一次 update_task_state：goal、steps 整体替换；constraints、decisions、facts 只追加，
    /// remove 里的条目先从这三项里删掉（同一次调用里先删后加，就是「改一条」）
    pub fn apply(&mut self, args: &Map<String, Value>) {
        if let Some(goal) = string_value(args.get("goal")).map(|goal| goal.trim().to_string()).filter(|goal| !goal.is_empty()) {
            self.goal = goal;
        }
        let removals: Vec<String> = strings(args.get("remove")).into_iter().filter(|item| count(item) >= 2).collect();
        if !removals.is_empty() {
            let keep = |item: &String| !removals.iter().any(|removal| item == removal || item.contains(removal.as_str()));
            self.constraints.retain(keep);
            self.decisions.retain(keep);
            self.facts.retain(keep);
        }
        append(strings(args.get("constraints")), &mut self.constraints);
        append(strings(args.get("decisions")), &mut self.decisions);
        append(strings(args.get("facts")), &mut self.facts);
        if let Some(steps) = steps(args.get("steps")) {
            self.steps = steps;
        }
    }

    /// 展示给模型的样子
    pub fn rendered(&self) -> String {
        let mut lines = Vec::new();
        if !self.goal.is_empty() {
            lines.push(format!("目标：{}", self.goal));
        }
        let mut list = |title: &str, items: &[String]| {
            if !items.is_empty() {
                lines.push(title.to_string());
                lines.extend(items.iter().map(|item| format!("- {item}")));
            }
        };
        list("约束（用户的要求，必须遵守）：", &self.constraints);
        list("已确认的决定：", &self.decisions);
        if !self.steps.is_empty() {
            lines.push("进度：".to_string());
            lines.extend(self.steps.iter().map(|step| format!("- [{}] {}", label(&step.status), step.text)));
        }
        if !self.facts.is_empty() {
            lines.push("关键信息：".to_string());
            lines.extend(self.facts.iter().map(|fact| format!("- {fact}")));
        }
        lines.join("\n")
    }
}

fn label(status: &str) -> &'static str {
    match status {
        "done" => "完成",
        "doing" => "进行中",
        _ => "待办",
    }
}

/// 按换行切开、丢掉空段（== Swift 的 split(whereSeparator: \.isNewline)：\n、\r、\u{0B}、\u{0C}、\u{85}、\u{2028}、\u{2029} 都算）
pub fn split_lines(text: &str) -> Vec<&str> {
    text.split(['\n', '\r', '\u{0B}', '\u{0C}', '\u{85}', '\u{2028}', '\u{2029}']).filter(|line| !line.is_empty()).collect()
}

fn append(items: Vec<String>, list: &mut Vec<String>) {
    for item in items {
        if !list.contains(&item) {
            list.push(item);
        }
    }
}

/// 字符串数组；有的模型有时把数组写成 JSON 字符串或多行文字，一并解开（去掉开头的「- 」）
fn strings(value: Option<&Value>) -> Vec<String> {
    let items: Vec<String> = match value {
        Some(Value::Array(values)) => values.iter().filter_map(|value| string_value(Some(value))).collect(),
        Some(Value::String(text)) => match serde_json::from_str::<Vec<String>>(text) {
            Ok(decoded) => decoded,
            Err(_) => split_lines(text).into_iter().map(str::to_string).collect(),
        },
        _ => Vec::new(),
    };
    items
        .into_iter()
        .map(|item| {
            let trimmed = item.trim();
            trimmed.strip_prefix("- ").unwrap_or(trimmed).trim_matches([' ', '\t']).to_string()
        })
        .filter(|item| !item.is_empty())
        .collect()
}

/// 步骤：[{text, status}]；也接受纯字符串（「[完成] …」「✅ …」这类前缀表示状态）
fn steps(value: Option<&Value>) -> Option<Vec<Step>> {
    let values: Vec<Value> = match value {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::String(text)) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Array(items)) => items,
            _ => split_lines(text).into_iter().map(|line| Value::String(line.to_string())).collect(),
        },
        _ => return None,
    };
    Some(
        values
            .iter()
            .filter(|item| !item.is_null())
            .filter_map(|item| {
                if let Some(object) = as_object(item) {
                    let text = ["text", "step", "title"]
                        .iter()
                        .find_map(|key| object.get(*key))
                        .and_then(|value| string_value(Some(value)))
                        .map(|text| text.trim().to_string())
                        .unwrap_or_default();
                    if text.is_empty() {
                        return None;
                    }
                    return Some(Step { text, status: status(&string_value(object.get("status")).unwrap_or_default()).to_string() });
                }
                let mut text = string_value(Some(item))?.trim().to_string();
                if text.is_empty() {
                    return None;
                }
                let mut state = "todo";
                let markers: [(&[&str], &str); 3] = [
                    (&["[x]", "[X]", "✅", "[完成]", "[done]"], "done"),
                    (&["[进行中]", "[doing]", "▶"], "doing"),
                    (&["[ ]", "☐", "[待办]", "[todo]"], "todo"),
                ];
                for (prefixes, value) in markers {
                    if let Some(found) = prefixes.iter().find(|found| has_character_prefix(&text, found)) {
                        state = value;
                        text = text[found.len()..].trim_matches([' ', '\t']).to_string();
                        break;
                    }
                }
                (!text.is_empty()).then(|| Step { text, status: state.to_string() })
            })
            .collect(),
    )
}

/// 按字符（字位簇）比前缀（== Swift 的 hasPrefix）：「✅️」（带变体选择符，是一个字符）不算以「✅」开头
fn has_character_prefix(text: &str, prefix: &str) -> bool {
    if !text.starts_with(prefix) {
        return false;
    }
    let mut length = 0;
    for grapheme in text.graphemes(true) {
        length += grapheme.len();
        if length >= prefix.len() {
            return length == prefix.len();
        }
    }
    length == prefix.len()
}

fn status(raw: &str) -> &'static str {
    match raw.to_lowercase().trim_matches([' ', '\t']) {
        "done" | "completed" | "complete" | "finished" | "完成" | "已完成" => "done",
        "doing" | "in_progress" | "in progress" | "active" | "进行中" => "doing",
        _ => "todo",
    }
}

/// 依次重放成功的 update_task_state 调用（参数是 JSON 文本，解不出来的跳过）
pub fn replay(calls: &[String]) -> TaskState {
    let mut state = TaskState::default();
    for call in calls {
        if let Ok(Value::Object(args)) = serde_json::from_str::<Value>(call) {
            state.apply(&args);
        }
    }
    state
}

// MARK: - 改动（v1.18）

/// 附注 / 渲染出来的状态按段拆开：段标题 → 条目（「- 」开头的行）。「目标：…」整行算一段；附注开头那行 […] 跳过
fn sections(text: &str) -> Vec<(String, Vec<String>)> {
    let mut result: Vec<(String, Vec<String>)> = Vec::new();
    for line in text.split('\n').filter(|line| !line.is_empty()) {
        if line.starts_with('[') && line.ends_with(']') {
            continue;
        }
        if let Some(item) = line.strip_prefix("- ")
            && let Some(last) = result.last_mut()
        {
            last.1.push(item.to_string());
        } else {
            result.push((line.to_string(), Vec::new()));
        }
    }
    result
}

/// 两份状态之间改了什么（只列改了的段）：目标整行；进度整段；其余的列新增、删掉的条目。没改是空的
pub fn delta_lines(old: &str, new: &str) -> Vec<String> {
    let before = sections(old);
    let after = sections(new);
    let key = |title: &str| if title.starts_with("目标：") { "目标".to_string() } else { title.to_string() };
    let name = |title: &str| title.strip_suffix('：').unwrap_or(title).to_string();
    let mut lines = Vec::new();
    for (title, items) in &after {
        let previous = before.iter().find(|(other, _)| key(other) == key(title));
        if key(title) == "目标" {
            if previous.map(|(other, _)| other) != Some(title) {
                lines.push(title.clone());
            }
            continue;
        }
        let old_items: &[String] = previous.map(|(_, items)| items.as_slice()).unwrap_or(&[]);
        if old_items == items.as_slice() {
            continue;
        }
        if title == "进度：" {
            lines.push(title.clone());
            lines.extend(items.iter().map(|item| format!("- {item}")));
            continue;
        }
        let added: Vec<&String> = items.iter().filter(|item| !old_items.contains(item)).collect();
        let removed: Vec<&String> = old_items.iter().filter(|item| !items.contains(item)).collect();
        if !added.is_empty() {
            lines.push(format!("{}，新增：", name(title)));
            lines.extend(added.iter().map(|item| format!("- {item}")));
        }
        if !removed.is_empty() {
            lines.push(format!("{}，删掉：", name(title)));
            lines.extend(removed.iter().map(|item| format!("- {item}")));
        }
    }
    for (title, _) in &before {
        if !after.iter().any(|(other, _)| key(other) == key(title)) {
            lines.push(if key(title) == "目标" {
                "目标：（清空了）".to_string()
            } else {
                format!("{}：（这一项现在没有了）", name(title))
            });
        }
    }
    lines
}

/// 附在用户消息后面交给模型的样子：前面没有附过的给整份；之后只给和上一次附的相比改了什么，没改是 None
pub fn rendered_note(note: &str, previous: Option<&str>) -> Option<String> {
    let Some(previous) = previous else { return Some(note.to_string()) };
    let lines = delta_lines(previous, note);
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "[任务状态有改动（Friday 自动附上，用户看不到这段；和用户最新的话冲突时以用户为准）：只列和上一次附的相比改了的，其余不变]\n{}",
        lines.join("\n")
    ))
}

/// 历史里和任务状态有关的一步（两端沿历史抽出来的）
pub enum Event {
    /// 一条用户消息附的状态（folded：在折叠掉的部分里）
    Note { note: Option<String>, folded: bool },
    /// 一次成功的 update_task_state（shown：在没折叠的部分里，模型看到了它回的改动）
    Call { arguments: String, shown: bool },
}

/// 模型在没折叠的部分里已经看到的任务状态（== Swift TaskState.Visible）：沿历史走一遍，返回每条用户消息交给模型的附注
/// （和 events 里的 Note 一一对应）和最后模型看到的整份
pub fn visible(events: &[Event]) -> (Vec<Option<String>>, Option<String>) {
    let mut text: Option<String> = None;
    let mut replayed = TaskState::default();
    let mut notes = Vec::new();
    for event in events {
        match event {
            Event::Note { note, folded } => {
                let shown = match note {
                    Some(note) if !folded => {
                        let rendered = rendered_note(note, text.as_deref());
                        text = Some(note.clone());
                        rendered
                    }
                    _ => None,
                };
                notes.push(shown);
            }
            Event::Call { arguments, shown } => {
                if let Ok(Value::Object(args)) = serde_json::from_str::<Value>(arguments) {
                    visible_apply(&mut text, &mut replayed, &args, *shown);
                }
            }
        }
    }
    (notes, text)
}

// MARK: - 附注、子智能体的背景

/// 沿路重放一次成功的 update_task_state（shown：模型看到了它回的改动，模型看到的整份跟着改）。返回改之前的状态
pub fn visible_apply(text: &mut Option<String>, replayed: &mut TaskState, args: &Map<String, Value>, shown: bool) -> TaskState {
    let before = replayed.clone();
    replayed.apply(args);
    if shown && let Some(current) = text.as_ref() {
        // 还没记进状态的用户原话那段留着（约束还空着时它在附注里）
        let marker = "\n用户之前提过的要求";
        let pending = if replayed.constraints.is_empty() { current.split_once(marker).map(|(_, rest)| rest.to_string()) } else { None };
        *text = Some(replayed.rendered() + &pending.map(|rest| format!("{marker}{rest}")).unwrap_or_default());
    }
    before
}

/// 两端交来的一条用户消息
pub struct UserMessage {
    pub text: String,
    /// 子智能体送回来的汇报（不是用户说的话）
    pub from_agent: bool,
    /// 工作中补充的
    pub interjected: bool,
    /// 附带的文件：「文件名：路径」
    pub files: Vec<String>,
}

const REQUIREMENT: &[&str] = &[
    "记住",
    "记得",
    "要求",
    "以后",
    "今后",
    "之后都",
    "每次",
    "一律",
    "统一",
    "都要",
    "都用",
    "都放",
    "都写",
    "不要",
    "别再",
    "不许",
    "禁止",
    "严禁",
    "不得",
    "必须",
    "务必",
    "规定",
    "约定",
    "偏好",
    "默认用",
    "remember",
    "always",
    "never",
    "must",
    "don't",
    "do not",
];
/// 关键信息里的规矩：只认禁止和必须这类词，「每次」「统一」在记事实时太常见
const RULE: &[&str] = &["不要", "别再", "不许", "禁止", "严禁", "不得", "必须", "务必", "never", "must", "don't", "do not"];

pub fn looks_like_requirement(text: &str) -> bool {
    let lowered = text.to_lowercase();
    REQUIREMENT.iter().any(|marker| lowered.contains(marker))
}

pub fn looks_like_rule(text: &str) -> bool {
    let lowered = text.to_lowercase();
    RULE.iter().any(|marker| lowered.contains(marker))
}

/// 像是在提要求的用户消息（最近 3 条，每条最多 300 字）。子智能体送回来的汇报不算
pub fn requirement_messages(messages: &[UserMessage]) -> Vec<String> {
    let texts: Vec<&String> = messages
        .iter()
        .filter(|message| !message.from_agent)
        .map(|message| &message.text)
        .filter(|text| looks_like_requirement(text))
        .collect();
    texts[texts.len().saturating_sub(3)..]
        .iter()
        .map(|text| if count(text) > 300 { prefix(text, 300) + "…" } else { text.to_string() })
        .collect()
}

/// 附在用户消息后面的那段：任务状态，加上还没记进状态的用户要求原话。两样都没有是 None
pub fn prompt_note(state: &TaskState, messages: &[UserMessage]) -> Option<String> {
    let pending = if state.constraints.is_empty() { requirement_messages(messages) } else { Vec::new() };
    if state.is_empty() && pending.is_empty() {
        return None;
    }
    let mut parts = vec!["[任务状态（Friday 自动附上，用户看不到这段；和用户最新的话冲突时以用户为准）]".to_string()];
    if !state.is_empty() {
        parts.push(state.rendered());
    }
    if !pending.is_empty() {
        let lines: Vec<String> = pending.iter().map(|text| format!("- 「{text}」")).collect();
        parts.push(format!("用户之前提过的要求（原话，还没记进任务状态，先用 update_task_state 记下来）：\n{}", lines.join("\n")));
    }
    Some(parts.join("\n"))
}

/// 子智能体背景里用户说过的话（原话，按时间；太多时最新的原样附约 4000 字，更早的只附第一条和像要求的）；没有是 None
pub fn user_words(messages: &[UserMessage]) -> Option<String> {
    let said: Vec<String> = messages
        .iter()
        .filter(|message| !message.from_agent)
        .filter_map(|message| {
            let mut text = message.text.trim().to_string();
            if !message.files.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text += &format!("（附带的文件：{}）", message.files.join("；"));
            }
            if text.is_empty() {
                return None;
            }
            Some(if message.interjected { format!("（工作中补充）{text}") } else { text })
        })
        .collect();
    if said.is_empty() {
        return None;
    }
    let clip = |text: &str, limit: usize| {
        let total = count(text);
        if total > limit {
            format!("{}…（这里截到 {limit} 字，原话共 {total} 字）", prefix(text, limit))
        } else {
            text.to_string()
        }
    };
    let shown: Vec<(usize, String)> = if said.iter().map(|text| count(text)).sum::<usize>() <= 6000 {
        said.iter().cloned().enumerate().collect()
    } else {
        // 最新的原样（最新那条再长也给，截到 4000 字）；更早的只给第一条和像要求的
        let mut used = 0;
        let mut start = said.len();
        for index in (0..said.len()).rev() {
            let length = count(&said[index]);
            if index != said.len() - 1 && used + length > 4000 {
                break;
            }
            used += length;
            start = index;
        }
        (0..start)
            .filter(|&index| index == 0 || looks_like_requirement(&said[index]))
            .map(|index| (index, clip(&said[index], 500)))
            .chain((start..said.len()).map(|index| (index, clip(&said[index], 4000))))
            .collect()
    };
    let omitted = said.len() - shown.len();
    let lines: Vec<String> = shown.iter().map(|(index, text)| format!("{}. 「{text}」", index + 1)).collect();
    Some(format!(
        "用户在这段对话里说的话（原话，按时间先后；前后冲突时以后面的为准）：\n{}{}",
        lines.join("\n"),
        if omitted > 0 { format!("\n（另有 {omitted} 条更早的话不像要求，没附）") } else { String::new() }
    ))
}

/// 附在子智能体任务后面的主任务背景：用户原话 + 主智能体记下的要求和决定（约束、决定、关键信息里的规矩）。都没有是 None
pub fn sub_agent_note(state: &TaskState, messages: &[UserMessage]) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(said) = user_words(messages) {
        parts.push(said);
    }
    let notes: Vec<&String> =
        state.constraints.iter().chain(state.decisions.iter()).chain(state.facts.iter().filter(|fact| looks_like_rule(fact))).collect();
    if !notes.is_empty() {
        let lines: Vec<String> = notes.iter().map(|note| format!("- {note}")).collect();
        parts.push(format!(
            "主 Agent 记下的要求和决定（它对用户的话的理解和它定的做法；和你的任务冲突时以任务为准，和用户原话冲突时以原话为准）：\n{}",
            lines.join("\n")
        ));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("[主任务的背景（Friday 自动附上）]\n{}", parts.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn applies_and_renders() {
        let mut state = TaskState::default();
        state.apply(&args(
            json!({ "goal": " 做个网站 ", "constraints": "[\"用 Go\", \"不要 Docker\"]", "facts": "- 端口 8080\n- 端口 8080",
            "steps": ["[完成] 建仓库", "✅ 写首页", { "step": "部署", "status": "in progress" }, null, { "text": "" }] }),
        ));
        assert_eq!(state.goal, "做个网站");
        assert_eq!(state.constraints, ["用 Go", "不要 Docker"]);
        assert_eq!(state.facts, ["端口 8080"]);
        assert_eq!(state.steps.iter().map(|step| step.status.as_str()).collect::<Vec<_>>(), ["done", "done", "doing"]);
        let mut emoji = TaskState::default();
        emoji.apply(&args(json!({ "steps": ["✅\u{fe0f} 带变体的不算完成", "✅ 完成"] })));
        assert_eq!((emoji.steps[0].status.as_str(), emoji.steps[0].text.as_str()), ("todo", "✅\u{fe0f} 带变体的不算完成"));
        assert_eq!((emoji.steps[1].status.as_str(), emoji.steps[1].text.as_str()), ("done", "完成"));
        state.apply(&args(json!({ "remove": ["Docker", "x"], "constraints": ["可以用 Docker"] })));
        assert_eq!(state.constraints, ["用 Go", "可以用 Docker"]);
        assert_eq!(
            state.rendered(),
            "目标：做个网站\n约束（用户的要求，必须遵守）：\n- 用 Go\n- 可以用 Docker\n进度：\n- [完成] 建仓库\n- [完成] 写首页\n- [进行中] 部署\n关键信息：\n- 端口 8080"
        );
        let replayed = replay(&["{\"goal\":\"a\"}".into(), "不是 JSON".into(), "{\"facts\":[\"b\"]}".into()]);
        assert_eq!((replayed.goal.as_str(), replayed.facts.len()), ("a", 1));
    }

    #[test]
    fn deltas_and_notes() {
        let old = "目标：a\n约束（用户的要求，必须遵守）：\n- x\n- y\n进度：\n- [待办] s";
        let new = "目标：b\n约束（用户的要求，必须遵守）：\n- x\n- z\n进度：\n- [完成] s\n关键信息：\n- k";
        assert_eq!(
            delta_lines(old, new),
            [
                "目标：b",
                "约束（用户的要求，必须遵守），新增：",
                "- z",
                "约束（用户的要求，必须遵守），删掉：",
                "- y",
                "进度：",
                "- [完成] s",
                "关键信息，新增：",
                "- k"
            ]
        );
        assert_eq!(delta_lines("目标：a\n关键信息：\n- k", ""), ["目标：（清空了）", "关键信息：（这一项现在没有了）"]);
        assert_eq!(rendered_note("目标：a", None).as_deref(), Some("目标：a"));
        assert!(rendered_note("目标：a", Some("[头]\n目标：a")).is_none());
        assert!(rendered_note("目标：b", Some("目标：a")).unwrap().starts_with("[任务状态有改动"));
    }

    #[test]
    fn visible_follows_history() {
        let events = vec![
            Event::Note { note: Some("目标：a".into()), folded: true },
            Event::Note { note: Some("目标：a".into()), folded: false },
            Event::Call { arguments: "{\"goal\":\"b\"}".into(), shown: true },
            Event::Note { note: Some("目标：b".into()), folded: false },
            Event::Note { note: None, folded: false },
        ];
        let (notes, text) = visible(&events);
        assert_eq!(notes, [None, Some("目标：a".into()), None, None]);
        assert_eq!(text.as_deref(), Some("目标：b"));
    }

    fn user(text: &str) -> UserMessage {
        UserMessage { text: text.into(), from_agent: false, interjected: false, files: Vec::new() }
    }

    #[test]
    fn notes_for_prompt_and_sub_agents() {
        let messages = vec![user("以后都用中文"), UserMessage { from_agent: true, ..user("必须汇报") }, user("你好")];
        assert_eq!(
            prompt_note(&TaskState::default(), &messages).unwrap(),
            "[任务状态（Friday 自动附上，用户看不到这段；和用户最新的话冲突时以用户为准）]\n用户之前提过的要求（原话，还没记进任务状态，先用 update_task_state 记下来）：\n- 「以后都用中文」"
        );
        assert!(prompt_note(&TaskState::default(), &[user("你好")]).is_none());
        let state =
            TaskState {
                decisions: vec!["用 Go".into()], facts: vec!["不要删日志".into(), "端口 80".into()], ..TaskState::default()
            };
        let note =
            sub_agent_note(&state, &[UserMessage { interjected: true, files: vec!["a.png：/tmp/a.png".into()], ..user("看看这个") }])
                .unwrap();
        assert_eq!(
            note,
            "[主任务的背景（Friday 自动附上）]\n用户在这段对话里说的话（原话，按时间先后；前后冲突时以后面的为准）：\n1. 「（工作中补充）看看这个\n（附带的文件：a.png：/tmp/a.png）」\n主 Agent 记下的要求和决定（它对用户的话的理解和它定的做法；和你的任务冲突时以任务为准，和用户原话冲突时以原话为准）：\n- 用 Go\n- 不要删日志"
        );
        // 太多：最新的原样，更早的只留第一条和像要求的
        let mut many: Vec<UserMessage> = (0..10).map(|index| user(&format!("{index}{}", "字".repeat(1000)))).collect();
        many[3].text = format!("必须{}", "字".repeat(1000));
        let words = user_words(&many).unwrap();
        assert!(words.contains("1. 「0") && words.contains("4. 「必须") && !words.contains("2. 「1"));
        assert!(words.contains("（这里截到 500 字") && words.ends_with("（另有 5 条更早的话不像要求，没附）"));
    }
}
