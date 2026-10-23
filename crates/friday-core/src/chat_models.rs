//! 会话的数据模型：Conversation、ChatMessage、MessageBlock、ToolEvent……，JSON 的写法和 Swift 的 Codable 一致
//! （Qt 的 models/ChatModels 也按它读写，见 Friday 的 qt/docs/ARCHITECTURE.md §5.2–5.3）：
//! - 键名 camelCase；可选字段为空时不写键；带关联值的枚举写成 `{"text": {"_0": "…"}}`、`{"tool": {"_0": {…}}}`
//! - 日期（`.iso8601`，`2026-10-01T02:15:00Z`）、UUID（大写）保留原文：核心不改写它们，两端读到的和写下的一样
//! - **认不出的字段原样保留**（各结构的 `extra`）：以后某一端先加的字段、只为读旧存档保留的字段，在核心里转一圈也不会丢
//!
//! 这一步只把核心要用到的字段定成类型（读入时的规整、以后搬过来的逻辑要用的），其余都在 `extra` 里；用到哪个再挪出来。

use serde::de::{self, Deserializer};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// 一段会话。消息存成一棵树（重新输入会产生分叉），nodes 是全部节点（含所有分支），按创建顺序
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Conversation {
    pub id: String,
    /// 没有时两端按「新对话」
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<Value>,
    #[serde(default)]
    pub nodes: Vec<ChatMessage>,
    /// 旧存档：线性消息数组（读入时补成 nodes，不再写回）
    #[serde(default, skip_serializing)]
    pub messages: Option<Vec<ChatMessage>>,
    /// 每个分叉点当前选中的孩子：父消息 id（根用全零 UUID）→ 子消息 id
    #[serde(default)]
    pub selected_children: Map<String, Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub id: String,
    /// user / assistant
    pub role: String,
    #[serde(default)]
    pub blocks: Vec<MessageBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<GenerationStats>,
    /// 分叉树里的父消息；没有表示根下的第一层（旧存档没有，读入时按顺序补链）
    #[serde(rename = "parentID", default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationStats {
    /// 还在生成（上次运行中途退出时会留下）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// 助手消息按时间顺序由多个块组成：思考 → 工具调用 → 正文 …
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageBlock {
    pub id: String,
    pub kind: BlockKind,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// == Swift `MessageBlock.Kind`（带关联值的枚举：`{"<case>": {"_0": 负载}}`）。认不出的 case 原样保留
#[derive(Debug, Clone, PartialEq)]
pub enum BlockKind {
    Text(String),
    Thinking(String),
    Tool(Box<ToolEvent>),
    Media(Value),
    Other(Map<String, Value>),
}

impl BlockKind {
    pub fn text(text: impl Into<String>) -> BlockKind {
        BlockKind::Text(text.into())
    }
}

impl Serialize for BlockKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let (name, payload) = match self {
            BlockKind::Text(text) => ("text", Value::from(text.clone())),
            BlockKind::Thinking(text) => ("thinking", Value::from(text.clone())),
            BlockKind::Tool(event) => ("tool", serde_json::to_value(event).map_err(serde::ser::Error::custom)?),
            BlockKind::Media(media) => ("media", media.clone()),
            BlockKind::Other(object) => return object.serialize(serializer),
        };
        let mut map = serializer.serialize_map(Some(1))?;
        let mut inner = Map::new();
        inner.insert("_0".to_string(), payload);
        map.serialize_entry(name, &inner)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for BlockKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let object = Map::<String, Value>::deserialize(deserializer)?;
        let payload = |name: &str| object.get(name).and_then(|case| case.get("_0")).cloned();
        if object.len() == 1 {
            if let Some(Value::String(text)) = payload("text") {
                return Ok(BlockKind::Text(text));
            }
            if let Some(Value::String(text)) = payload("thinking") {
                return Ok(BlockKind::Thinking(text));
            }
            if let Some(event) = payload("tool") {
                return serde_json::from_value(event).map(|event| BlockKind::Tool(Box::new(event))).map_err(de::Error::custom);
            }
            if let Some(media) = payload("media") {
                return Ok(BlockKind::Media(media));
            }
        }
        Ok(BlockKind::Other(object))
    }
}

/// 一次工具调用（可能是派子 Agent 的，transcript 里是它的运行记录）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolEvent {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: String,
    /// awaitingApproval / running / succeeded / failed / denied
    pub status: String,
    #[serde(default)]
    pub result: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<Vec<MessageBlock>>,
    /// 子 Agent 在排队等待运行
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ToolEvent {
    pub fn is_running(&self) -> bool {
        self.status == "running" || self.status == "awaitingApproval"
    }
}

/// 应用退出时还在跑的派子 Agent 卡片的结果（开头和普通调用一样是「应用退出时仍在运行，已中断」）
pub const INTERRUPTED_AGENT_TEXT: &str = "应用退出时仍在运行，已中断：子智能体不会再交回结果，要接着做就重新派发（先核对它已经做了的改动）";
pub const INTERRUPTED_TEXT: &str = "应用退出时仍在运行，已中断";
pub const INTERRUPTED_APPROVAL_TEXT: &str = "等待确认时应用退出，已中断";
/// 上次生成中途退出、什么都没写出来的回复
pub const INTERRUPTED_REPLY: &str = "（生成被中断）";

/// 有没有还标着「运行中 / 等确认」的调用（递归进子 Agent 记录）
pub fn contains_running(blocks: &[MessageBlock]) -> bool {
    blocks.iter().any(|block| match &block.kind {
        BlockKind::Tool(event) => event.is_running() || event.transcript.as_deref().is_some_and(contains_running),
        _ => false,
    })
}

/// 应用退出时还在运行或等确认的调用：标成失败并说明原因（递归进子 Agent 记录）。team_tools：会跑子 Agent 的工具名（两端按注册表给）
pub fn mark_interrupted(blocks: &mut [MessageBlock], team_tools: &[String]) {
    for block in blocks {
        let BlockKind::Tool(event) = &mut block.kind else { continue };
        if event.is_running() {
            // 派子 Agent 的卡片：说清它不会再交回结果，主 Agent 下一轮看到才不会以为它还在后台跑
            event.result = if event.status == "awaitingApproval" {
                INTERRUPTED_APPROVAL_TEXT.to_string()
            } else if team_tools.contains(&event.name) {
                INTERRUPTED_AGENT_TEXT.to_string()
            } else {
                INTERRUPTED_TEXT.to_string()
            };
            event.status = "failed".to_string();
            event.queued = None;
        }
        if let Some(transcript) = &mut event.transcript {
            mark_interrupted(transcript, team_tools);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn block_kinds() {
        let blocks = json!([
            {"id": "A", "kind": {"text": {"_0": "正文"}}},
            {"id": "B", "kind": {"thinking": {"_0": "想"}}},
            {"id": "C", "kind": {"tool": {"_0": {"id": "T", "name": "run_shell", "arguments": "{}", "status": "running", "result": "", "symbol": "terminal"}}}},
            {"id": "D", "kind": {"media": {"_0": {"id": "M", "kind": "image", "relativePath": "a.png"}}}},
            {"id": "E", "kind": {"future": {"_0": 1}}, "later": true},
        ]);
        let decoded: Vec<MessageBlock> = serde_json::from_value(blocks.clone()).unwrap();
        assert!(matches!(&decoded[0].kind, BlockKind::Text(text) if text == "正文"));
        assert!(matches!(&decoded[2].kind, BlockKind::Tool(event) if event.extra["symbol"] == json!("terminal")));
        assert!(matches!(&decoded[4].kind, BlockKind::Other(_)));
        // 写回去一字不差（认不出的 case、字段都在）
        assert_eq!(serde_json::to_value(&decoded).unwrap(), blocks);
    }

    #[test]
    fn interrupted() {
        let mut blocks: Vec<MessageBlock> = serde_json::from_value(json!([
            {"id": "A", "kind": {"tool": {"_0": {"id": "1", "name": "run_shell", "arguments": "", "status": "running", "result": ""}}}},
            {"id": "B", "kind": {"tool": {"_0": {"id": "2", "name": "delegate_task", "arguments": "", "status": "running", "result": "", "queued": true,
                "transcript": [{"id": "C", "kind": {"tool": {"_0": {"id": "3", "name": "write_file", "arguments": "", "status": "awaitingApproval", "result": ""}}}}]}}}},
            {"id": "D", "kind": {"tool": {"_0": {"id": "4", "name": "read_file", "arguments": "", "status": "succeeded", "result": "ok"}}}},
        ]))
        .unwrap();
        assert!(contains_running(&blocks));
        mark_interrupted(&mut blocks, &["delegate_task".to_string()]);
        assert!(!contains_running(&blocks));
        let event = |index: usize| match &blocks[index].kind {
            BlockKind::Tool(event) => event.as_ref().clone(),
            _ => unreachable!(),
        };
        assert_eq!(event(0).result, INTERRUPTED_TEXT);
        assert_eq!((event(1).result.as_str(), event(1).queued), (INTERRUPTED_AGENT_TEXT, None));
        let BlockKind::Tool(nested) = &event(1).transcript.unwrap()[0].kind else { unreachable!() };
        assert_eq!((nested.status.as_str(), nested.result.as_str()), ("failed", INTERRUPTED_APPROVAL_TEXT));
        assert_eq!(event(2).result, "ok");
    }
}
