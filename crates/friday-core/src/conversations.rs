//! 读 conversations.json 时的规整（== 两端 ConversationStore 读入时做的；会话的增删改、落盘的防抖还在两端，见 docs/PORTING.md）：
//! - 一段会话解不出来就跳过它（== Qt；Swift 原来是整份读不出来，存档里一段坏了全部会话都看不到）
//! - 旧存档没有 nodes、只有线性的 messages：按顺序补上父子关系
//! - 上次运行在中途退出：还标着「进行中」的统计清掉，什么都没写出来的回复写成「（生成被中断）」；还在「运行中 / 待确认」的
//!   工具调用（含嵌套的子 Agent）标成中断——刚启动时不可能有东西真在跑，留着会是永远转圈的卡片和失效的确认按钮
//! - 按更新时间从新到旧排（同一时间的保持原来的顺序）

use std::path::Path;

use chrono::DateTime;
use serde_json::Value;

use crate::chat_models::{self, BlockKind, Conversation, MessageBlock};

/// 读存档并规整；文件不在、不是 JSON 数组返回空
pub fn load(path: &Path, team_tools: &[String]) -> Vec<Conversation> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Array(items)) => normalize(items, team_tools),
        _ => Vec::new(),
    }
}

/// 存档里的每一段会话解出来、规整好
pub fn normalize(items: Vec<Value>, team_tools: &[String]) -> Vec<Conversation> {
    let mut conversations: Vec<Conversation> =
        items.into_iter().filter(Value::is_object).filter_map(|item| serde_json::from_value(item).ok()).collect();
    for conversation in &mut conversations {
        adopt_legacy_messages(conversation);
        interrupt_stale(conversation, team_tools);
    }
    // 稳定排序：同一时间的保持存档里的顺序
    conversations.sort_by(|a, b| timestamp(&b.updated_at).total_cmp(&timestamp(&a.updated_at)));
    conversations
}

/// 旧存档：线性消息数组，按顺序补上父子关系
fn adopt_legacy_messages(conversation: &mut Conversation) {
    let Some(messages) = conversation.messages.take() else { return };
    if !conversation.nodes.is_empty() {
        return;
    }
    let mut previous: Option<String> = None;
    conversation.nodes = messages
        .into_iter()
        .map(|mut message| {
            message.parent_id = previous.replace(message.id.clone());
            message
        })
        .collect();
}

fn interrupt_stale(conversation: &mut Conversation, team_tools: &[String]) {
    for message in &mut conversation.nodes {
        if message.stats.as_ref().is_some_and(|stats| stats.live == Some(true)) {
            message.stats = None;
            if message.blocks.is_empty() {
                message.blocks = vec![MessageBlock {
                    id: uuid_like(&message.id),
                    kind: BlockKind::text(chat_models::INTERRUPTED_REPLY),
                    extra: Default::default(),
                }];
            }
        }
        if chat_models::contains_running(&message.blocks) {
            chat_models::mark_interrupted(&mut message.blocks, team_tools);
        }
    }
}

/// 补出来的那个块要有 id：由消息 id 推出来（同一份存档每次读出来一样；格式同 Swift 的大写 UUID）
fn uuid_like(seed: &str) -> String {
    let digest = crate::secret_guard::hash(&format!("interrupted:{seed}")).to_uppercase();
    format!("{}-{}-{}-{}-{}", &digest[0..8], &digest[8..12], &digest[12..16], &digest[16..20], &digest[20..32])
}

/// 存档里的日期：`.iso8601` 文本，或 Swift 默认策略的「距 2001-01-01 的秒数」；都不是按最早算
fn timestamp(value: &Option<Value>) -> f64 {
    match value {
        Some(Value::String(text)) => DateTime::parse_from_rfc3339(text).map_or(f64::MIN, |date| date.timestamp_millis() as f64 / 1000.0),
        Some(Value::Number(number)) => number.as_f64().map_or(f64::MIN, |seconds| seconds + 978_307_200.0),
        _ => f64::MIN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizing() {
        let items = vec![
            json!({"id": "OLD", "title": "旧", "updatedAt": "2026-10-01T00:00:00Z",
                   "messages": [{"id": "M1", "role": "user", "blocks": []}, {"id": "M2", "role": "assistant", "blocks": [], "stats": {"live": true, "tokenCount": 3, "tokensPerSecond": 1.5}}]}),
            json!({"id": "NEW", "title": "新", "updatedAt": "2026-10-02T00:00:00Z", "pinned": true, "nodes": [
                {"id": "N1", "role": "assistant", "parentID": "X", "blocks": [
                    {"id": "B", "kind": {"tool": {"_0": {"id": "T", "name": "run_shell", "arguments": "", "status": "running", "result": ""}}}}]}]}),
            json!("不是对象"),
            json!({"title": "没有 id，解不出来"}),
            json!({"id": "SECONDS", "updatedAt": 812_000_000.0}),
        ];
        let conversations = normalize(items, &[]);
        assert_eq!(conversations.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["NEW", "OLD", "SECONDS"]);
        let old = &conversations[1];
        assert!(old.messages.is_none());
        assert_eq!(old.nodes[0].parent_id, None);
        assert_eq!(old.nodes[1].parent_id.as_deref(), Some("M1"));
        assert!(old.nodes[1].stats.is_none());
        assert!(matches!(&old.nodes[1].blocks[0].kind, BlockKind::Text(text) if text == "（生成被中断）"));
        assert_eq!(old.nodes[1].blocks[0].id.len(), 36);
        let BlockKind::Tool(event) = &conversations[0].nodes[0].blocks[0].kind else { unreachable!() };
        assert_eq!(event.status, "failed");
        // 认不出的字段（pinned 在这一步没定成类型）写回去还在；旧的 messages 不再写
        let written = serde_json::to_value(&conversations[0]).unwrap();
        assert_eq!(written["pinned"], json!(true));
        assert!(serde_json::to_value(old).unwrap().get("messages").is_none());
    }

    /// 没有要规整的存档：读进来再写出去，和原文（按 JSON 值比）完全一样
    #[test]
    fn round_trip() {
        // 太深的 json! 会超出宏展开的层数，用原文
        let item: Value = serde_json::from_str(
            r#"{"id": "C", "title": "t", "createdAt": "2026-10-01T02:15:00Z", "updatedAt": "2026-10-01T02:16:00Z",
                "selectedChildren": {"00000000-0000-0000-0000-000000000000": "M"}, "permissionMode": "auto", "pinned": false, "draft": "草稿",
                "nodes": [{"id": "M", "role": "assistant", "createdAt": "2026-10-01T02:15:00Z", "taskNote": "目标",
                           "stats": {"tokensPerSecond": 12.5, "tokenCount": 7, "startedAt": "2026-10-01T02:15:00Z"},
                           "blocks": [{"id": "B", "kind": {"tool": {"_0": {"id": "T", "name": "delegate_task", "arguments": "{\"task\":\"x\"}",
                               "status": "succeeded", "result": "ok", "linesAdded": 3, "usage": {"model": "m", "thinking": "高", "temperature": 0.6},
                               "transcript": [{"id": "B2", "kind": {"media": {"_0": {"id": "P", "kind": "image", "relativePath": "a/b.png", "byteCount": 12}}}}]}}}}]}]}"#,
        )
        .unwrap();
        let conversations = normalize(vec![item.clone()], &[]);
        assert_eq!(serde_json::to_value(&conversations[0]).unwrap(), item);
    }
}
