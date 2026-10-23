//! 会话存档 conversations.json 的落盘（== 两端 ConversationStore 的 saveNow）和会话的 fork。
//!
//! 核心留一份存档（句柄）：两端读入时由它读（`archive.open`，同 `conversations.load` 的规整），之后每次落盘只交**改过的那几段**
//! 和全部会话的顺序，核心在后台写盘线程里拼整份写下。以前两端每次落盘都把整份存档编码一遍（存档大了要几百毫秒，生成过程中
//! 至少 3 秒一次），现在只编码改过的会话。
//! - 空白会话不落盘，只剩草稿的空白会话要留着（新会话里打了字还没发就退出，重启后要能接着写）
//! - 两端可以在后台线程交（Swift 在后台任务里编码）：每次带递增的序号，序号比已收到的旧的那段 / 那份顺序不用，先后乱了也不会写回旧内容
//! - 落盘间隔（生成过程中强制落盘的间隔）：至少 3 秒，编码一次越慢间隔越长，让保存占用的 CPU 不超过几个百分点；最多 30 秒
//!
//! 会话的增删改、防抖计时、界面观察的数组和正在生成的那条（LiveMessage）留在两端：流式生成时每个字都改一次正在写的消息，
//! 搬进核心的话每个字都要过一次 C 接口（见 docs/PORTING.md）。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

use serde_json::{Map, Value};

use crate::chat_models::{ChatMessage, Conversation};
use crate::{conversations, writer};

/// 父消息 id 为空（根下的第一层）记在这个键下（== Swift Conversation.rootKey）
pub const ROOT_KEY: &str = "00000000-0000-0000-0000-000000000000";

struct Archive {
    path: PathBuf,
    /// id → (交来时的序号, 内容)
    items: HashMap<String, (u64, Arc<Conversation>)>,
    order: Vec<String>,
    order_seq: u64,
    /// 上一次编码 + 拼整份花的秒数（后台写盘线程里量的）
    cost: Arc<Mutex<f64>>,
}

static ARCHIVES: LazyLock<Mutex<HashMap<u64, Archive>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT: AtomicU64 = AtomicU64::new(1);

/// 打开存档：读文件并规整（同 conversations.load），返回句柄和读到的会话
pub fn open(path: PathBuf, team_tools: &[String]) -> (u64, Vec<Conversation>) {
    let loaded = conversations::load(&path, team_tools);
    let id = open_with(path, loaded.clone());
    (id, loaded)
}

/// 用两端已有的会话打开（不读文件：两端自己读的、测试用的）
pub fn open_with(path: PathBuf, items: Vec<Conversation>) -> u64 {
    let order = items.iter().map(|conversation| conversation.id.clone()).collect();
    let items = items.into_iter().map(|conversation| (conversation.id.clone(), (0, Arc::new(conversation)))).collect();
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let archive = Archive { path, items, order, order_seq: 0, cost: Arc::new(Mutex::new(0.0)) };
    ARCHIVES.lock().unwrap().insert(id, archive);
    id
}

pub fn close(id: u64) {
    ARCHIVES.lock().unwrap().remove(&id);
}

/// 有没有这个句柄
pub fn exists(id: u64) -> bool {
    ARCHIVES.lock().unwrap().contains_key(&id)
}

/// 交一次落盘：changed 是改过的会话（含正在生成的那条的当前内容），order 是全部会话的 id（两端数组的顺序）。
/// encode_seconds：两端编码 changed 花的时间（算落盘间隔）。返回下一次强制落盘的间隔（秒）和核心还没有内容的 id（两端漏交的，
/// 这次写不进去）
pub fn save(id: u64, seq: u64, order: Vec<String>, changed: Vec<Conversation>, encode_seconds: f64) -> Option<(f64, Vec<String>)> {
    let mut archives = ARCHIVES.lock().unwrap();
    let archive = archives.get_mut(&id)?;
    for conversation in changed {
        let newer = archive.items.get(&conversation.id).is_none_or(|(known, _)| *known <= seq);
        if newer {
            archive.items.insert(conversation.id.clone(), (seq, Arc::new(conversation)));
        }
    }
    if seq >= archive.order_seq {
        archive.order_seq = seq;
        archive.order = order;
        // 不在顺序里的（删掉的）不再留着
        let keep: HashSet<&String> = archive.order.iter().collect();
        archive.items.retain(|key, _| keep.contains(key));
    }
    let mut missing = Vec::new();
    let mut snapshot: Vec<Arc<Conversation>> = Vec::with_capacity(archive.order.len());
    for key in &archive.order {
        match archive.items.get(key) {
            Some((_, conversation)) if is_saved(conversation) => snapshot.push(conversation.clone()),
            Some(_) => {}
            None => missing.push(key.clone()),
        }
    }
    let cost = archive.cost.clone();
    writer::write(archive.path.clone(), move || {
        let start = Instant::now();
        let items: Vec<&Conversation> = snapshot.iter().map(Arc::as_ref).collect();
        let data = serde_json::to_vec(&items).unwrap_or_default();
        *cost.lock().unwrap() = encode_seconds + start.elapsed().as_secs_f64();
        data
    });
    let last = *archive.cost.lock().unwrap();
    Some((interval(last.max(encode_seconds)), missing))
}

/// 生成过程中强制落盘的间隔：至少 3 秒；编码一次越慢越长（×30，保存占用的 CPU 不超过约 3%），最多 30 秒
pub fn interval(cost_seconds: f64) -> f64 {
    (cost_seconds * 30.0).clamp(3.0, 30.0)
}

/// 空白会话一般不存；里面还有没发出去的草稿就得存下来
pub fn is_saved(conversation: &Conversation) -> bool {
    !conversation.nodes.is_empty() || conversation.extra.get("draft").and_then(Value::as_str).is_some_and(|draft| !draft.is_empty())
}

// MARK: - 消息树（== Swift Conversation.messages）

/// 当前路径在 nodes 里的下标：从根开始，每个分叉点取选中的孩子（没选过取最新的）
pub fn path_indices(conversation: &Conversation) -> Vec<usize> {
    let nodes = &conversation.nodes;
    // 没分叉过（每条都接在前一条后面）：路径就是全部节点
    let linear =
        nodes.iter().enumerate().all(|(i, node)| node.parent_id.as_deref() == if i == 0 { None } else { Some(nodes[i - 1].id.as_str()) });
    if linear {
        return (0..nodes.len()).collect();
    }
    let mut index: HashMap<&str, Vec<usize>> = HashMap::with_capacity(nodes.len());
    for (i, node) in nodes.iter().enumerate() {
        index.entry(node.parent_id.as_deref().unwrap_or(ROOT_KEY)).or_default().push(i);
    }
    let mut path = Vec::new();
    let mut key = ROOT_KEY;
    let mut visited: HashSet<&str> = HashSet::new();
    while let Some(kids) = index.get(key).filter(|kids| !kids.is_empty()) {
        let selected = conversation.selected_children.get(key).and_then(Value::as_str);
        let chosen = selected.and_then(|id| kids.iter().copied().find(|&i| nodes[i].id == id)).unwrap_or(kids[kids.len() - 1]);
        let id = nodes[chosen].id.as_str();
        if !visited.insert(id) {
            break;
        }
        path.push(chosen);
        key = id;
    }
    path
}

/// 追加到当前路径末尾（== Swift Conversation.append）
pub fn append(conversation: &mut Conversation, mut message: ChatMessage) {
    let parent = path_indices(conversation).last().map(|&i| conversation.nodes[i].id.clone());
    message.parent_id = parent.clone();
    let id = message.id.clone();
    conversation.nodes.push(message);
    conversation.selected_children.insert(parent.unwrap_or_else(|| ROOT_KEY.to_string()), Value::String(id));
}

/// 新的 UUID（大写，同两端）
pub fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string().to_uppercase()
}

/// 现在的时间，写法同 Swift 的 .iso8601（`2026-10-01T02:15:00Z`）
pub fn now_iso8601() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// 从某条消息处 fork 出新会话：复制当前路径到该消息为止（消息换新 id），标题加「（fork）」，大脑、目录、权限、深度思考、
/// 任务模式跟原来的（任务模式 Swift 原来没带，按 Qt 的带上）；没有这条消息时复制整条路径。new_id：远程客户端指定的新会话 id
pub fn fork(source: &Conversation, through: &str, new_id: Option<String>) -> Conversation {
    let now = Value::String(now_iso8601());
    let mut extra = Map::new();
    for key in ["engineID", "workingDirectory", "permissionMode", "thinking", "teamMode"] {
        if let Some(value) = source.extra.get(key) {
            extra.insert(key.to_string(), value.clone());
        }
    }
    extra.insert("pinned".to_string(), Value::Bool(false));
    let mut copy = Conversation {
        id: new_id.unwrap_or_else(new_uuid),
        title: Some(format!("{}（fork）", source.title.as_deref().unwrap_or("新对话"))),
        created_at: Some(now.clone()),
        updated_at: Some(now),
        nodes: Vec::new(),
        messages: None,
        selected_children: Map::new(),
        extra,
    };
    let path = path_indices(source);
    let end = path.iter().position(|&i| source.nodes[i].id == through).map_or(path.len(), |position| position + 1);
    for &i in &path[..end] {
        let mut message = source.nodes[i].clone();
        message.id = new_uuid();
        append(&mut copy, message);
    }
    copy
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn conversation(value: Value) -> Conversation {
        serde_json::from_value(value).unwrap()
    }

    fn message(id: &str, parent: Option<&str>, text: &str) -> Value {
        let mut value = json!({ "id": id, "role": "user", "blocks": [{ "id": format!("B{id}"), "kind": { "text": { "_0": text } } }] });
        if let Some(parent) = parent {
            value["parentID"] = json!(parent);
        }
        value
    }

    #[test]
    fn path_follows_selected_children() {
        // A → B、A → C（选中 B）→ 路径 A B；没选过取最新的
        let mut forked = conversation(
            json!({ "id": "X", "nodes": [message("A", None, "a"), message("B", Some("A"), "b"), message("C", Some("A"), "c")],
            "selectedChildren": { "A": "B" } }),
        );
        let ids = |c: &Conversation| path_indices(c).into_iter().map(|i| c.nodes[i].id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&forked), ["A", "B"]);
        forked.selected_children.clear();
        assert_eq!(ids(&forked), ["A", "C"]);
        let linear = conversation(json!({ "id": "Y", "nodes": [message("A", None, "a"), message("B", Some("A"), "b")] }));
        assert_eq!(ids(&linear), ["A", "B"]);
        // 环：不死循环
        let cyclic = conversation(
            json!({ "id": "Z", "nodes": [message("A", Some("B"), "a"), message("B", Some("A"), "b"), message("C", None, "c")] }),
        );
        assert_eq!(ids(&cyclic), ["C"]);
    }

    #[test]
    fn fork_copies_path_prefix() {
        let source =
            conversation(json!({ "id": "X", "title": "原来", "engineID": "e1", "workingDirectory": "/w", "permissionMode": "plan",
            "teamMode": "auto", "pinned": true, "draft": "草稿",
            "nodes": [message("A", None, "a"), message("B", Some("A"), "b"), message("C", Some("A"), "c"), message("D", Some("C"), "d")],
            "selectedChildren": { "A": "C" } }));
        let copy = fork(&source, "C", Some("NEW".into()));
        assert_eq!(copy.id, "NEW");
        assert_eq!(copy.title.as_deref(), Some("原来（fork）"));
        assert_eq!(copy.extra["engineID"], "e1");
        assert_eq!(copy.extra["teamMode"], "auto");
        assert_eq!(copy.extra["pinned"], false);
        assert!(!copy.extra.contains_key("draft"));
        // A、C 两条，换了新 id，父子链接好
        assert_eq!(copy.nodes.len(), 2);
        assert!(copy.nodes.iter().all(|node| node.id != "A" && node.id != "C" && node.id.len() == 36));
        assert_eq!(copy.nodes[0].parent_id, None);
        assert_eq!(copy.nodes[1].parent_id.as_deref(), Some(copy.nodes[0].id.as_str()));
        assert_eq!(copy.nodes[1].blocks, source.nodes[2].blocks);
        assert_eq!(copy.selected_children[ROOT_KEY], copy.nodes[0].id.as_str());
        // 没有这条消息：整条路径
        assert_eq!(fork(&source, "nope", None).nodes.len(), 3);
        assert!(copy.created_at.as_ref().and_then(Value::as_str).is_some_and(|date| date.ends_with('Z') && date.len() == 20));
    }

    #[test]
    fn save_writes_changed_and_keeps_rest() {
        let dir = std::env::temp_dir().join(format!("friday-archive-{}", std::process::id()));
        let path = dir.join("conversations.json");
        let _ = std::fs::remove_dir_all(&dir);
        let one = conversation(json!({ "id": "A", "title": "一", "nodes": [message("M1", None, "x")], "future": { "kept": 1 } }));
        let two = conversation(json!({ "id": "B", "title": "二", "nodes": [message("M2", None, "y")] }));
        let blank = conversation(json!({ "id": "C", "nodes": [] }));
        let drafted = conversation(json!({ "id": "D", "nodes": [], "draft": "还没发" }));
        let id = open_with(path.clone(), vec![one.clone(), two.clone(), blank.clone()]);
        let order = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect::<Vec<_>>();
        // 只改了 B：A 用核心留着的（认不出的字段也在）；空白的 C 不写；D 有草稿要写
        let mut changed = two.clone();
        changed.title = Some("二改".into());
        let (interval, missing) = save(id, 1, order(&["B", "A", "C", "D"]), vec![changed, drafted], 0.0).unwrap();
        assert_eq!(interval, 3.0);
        assert!(missing.is_empty());
        writer::flush();
        let written: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let ids: Vec<&str> = written.as_array().unwrap().iter().map(|item| item["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["B", "A", "D"]);
        assert_eq!(written[0]["title"], "二改");
        assert_eq!(written[1]["future"]["kept"], 1);
        // 旧序号交来的不覆盖新的；删掉的（不在顺序里）不再写
        let mut stale = two.clone();
        stale.title = Some("旧的".into());
        save(id, 0, order(&["A", "B"]), vec![stale], 0.0).unwrap();
        let (_, missing) = save(id, 2, order(&["B", "E"]), Vec::new(), 0.0).unwrap();
        assert_eq!(missing, ["E"]);
        writer::flush();
        let written: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(written.as_array().unwrap().len(), 1);
        assert_eq!(written[0]["title"], "二改");
        // 编码慢：间隔放大，最多 30 秒
        assert_eq!(interval_of(id, 0.5), 15.0);
        assert_eq!(super::interval(5.0), 30.0);
        close(id);
        assert!(save(id, 3, Vec::new(), Vec::new(), 0.0).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn interval_of(id: u64, encode: f64) -> f64 {
        save(id, 10, vec!["B".into()], Vec::new(), encode).unwrap().0
    }

    #[test]
    fn open_reads_and_normalizes() {
        let dir = std::env::temp_dir().join(format!("friday-archive-open-{}", std::process::id()));
        let path = dir.join("conversations.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            r#"[{"id":"A","updatedAt":"2026-10-01T00:00:00Z","messages":[{"id":"M1","role":"user"},{"id":"M2","role":"assistant"}]}]"#,
        )
        .unwrap();
        let (id, loaded) = open(path.clone(), &[]);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].nodes[1].parent_id.as_deref(), Some("M1"));
        save(id, 1, vec!["A".into()], Vec::new(), 0.0).unwrap();
        writer::flush();
        let written: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(written[0]["nodes"][1]["parentID"], "M1");
        close(id);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
