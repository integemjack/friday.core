//! 能力（生图、语音、生视频…）的存档（== Swift Capabilities/CapabilityRegistry 的 load / save / saveJobs；Qt capabilities/CapabilityRegistry）：
//! - capabilities.json：提供方按类型分三组存——在线的放 entries、脚本提供方放 scriptEntries（旧版本不认识 script 类型，混在一起会整个
//!   文件读不出来）、本地能力（Power）放 powerEntries；disabledKinds 一直写空数组（旧版本解码时要求有这个键）。
//!   旧版本按能力整体关掉的（disabledKinds 非空）读入时交给两端迁移到每个提供方上
//! - jobs.json：生视频这类异步任务，只保留最近 50 个
//!
//! 每个条目的解码（认不出的类型跳过）、注册表、后端留在两端；这里只管文件的结构和写法（键排序、两空格缩进）。

use std::path::Path;

use serde_json::{Value, json};

use crate::{swift_json, writer};

/// 读 capabilities.json：{entries: [三组合在一起的条目], legacyDisabledKinds: [旧版本按能力整体关掉的]}；读不出来是空的
pub fn load_config(path: &Path) -> Value {
    let file = std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let Some(Value::Object(file)) = file else { return json!({ "entries": [], "legacyDisabledKinds": [] }) };
    let mut entries = Vec::new();
    for key in ["entries", "scriptEntries", "powerEntries"] {
        if let Some(Value::Array(items)) = file.get(key) {
            entries.extend(items.iter().filter(|item| item.is_object()).cloned());
        }
    }
    let legacy: Vec<Value> =
        file.get("disabledKinds").and_then(Value::as_array).into_iter().flatten().filter(|kind| kind.is_string()).cloned().collect();
    json!({ "entries": entries, "legacyDisabledKinds": legacy })
}

/// capabilities.json 要写的内容（按 type 分三组）
pub fn config_text(entries: &[Value]) -> String {
    let group = |wanted: Option<&str>| -> Vec<Value> {
        entries
            .iter()
            .filter(|entry| {
                let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
                match wanted {
                    Some(wanted) => kind == wanted,
                    None => kind != "script" && kind != "power",
                }
            })
            .cloned()
            .collect()
    };
    let file = json!({ "entries": group(None), "disabledKinds": [], "scriptEntries": group(Some("script")), "powerEntries": group(Some("power")) });
    swift_json::pretty(&file)
}

/// 写 capabilities.json（就地写完再返回：两端紧接着可能再读）
pub fn save_config(path: &Path, entries: &[Value]) -> Result<(), String> {
    writer::write_now(path, config_text(entries).as_bytes()).map_err(|error| error.to_string())
}

/// 读 jobs.json（不是数组是空的）
pub fn load_jobs(path: &Path) -> Vec<Value> {
    match std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str::<Value>(&text).ok()) {
        Some(Value::Array(items)) => items.into_iter().filter(Value::is_object).collect(),
        _ => Vec::new(),
    }
}

/// 写 jobs.json：只保留最近 50 个（两端的数组新的在前）
pub fn save_jobs(path: &Path, jobs: &[Value]) -> Result<(), String> {
    let kept: Vec<Value> = jobs.iter().take(50).cloned().collect();
    writer::write_now(path, swift_json::pretty(&Value::Array(kept)).as_bytes()).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trip() {
        let dir = std::env::temp_dir().join(format!("friday-capabilities-{}", std::process::id()));
        let path = dir.join("capabilities.json");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load_config(&path), json!({ "entries": [], "legacyDisabledKinds": [] }));
        let entries =
            vec![json!({ "id": "a", "type": "dashscope" }), json!({ "id": "b", "type": "script" }), json!({ "id": "c", "type": "power" })];
        save_config(&path, &entries).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("{\n  \"disabledKinds\" : [\n\n  ],\n  \"entries\" : ["));
        let loaded = load_config(&path);
        assert_eq!(loaded["entries"].as_array().unwrap().len(), 3);
        std::fs::write(&path, r#"{"entries":[{"id":"x","type":"old"},3],"disabledKinds":["video"]}"#).unwrap();
        assert_eq!(load_config(&path), json!({ "entries": [{ "id": "x", "type": "old" }], "legacyDisabledKinds": ["video"] }));
        let jobs: Vec<Value> = (0..60).map(|index| json!({ "id": index })).collect();
        let jobs_path = dir.join("jobs.json");
        save_jobs(&jobs_path, &jobs).unwrap();
        let saved = load_jobs(&jobs_path);
        assert_eq!((saved.len(), saved[0]["id"].as_i64()), (50, Some(0)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
