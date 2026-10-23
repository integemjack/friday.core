//! 诊断日志（数据目录的 journal.json）：记录工具失败、框架错误、MCP 连接失败等，供 Agent 用 get_diagnostics 自查。
//! 新的在前，最多 [`LIMIT`] 条，detail 截到 3000 字（按字形簇计）。条目：id（大写 UUID）、date（`.iso8601`，不带毫秒）、
//! kind（tool / framework / mcp / recovered）、title、detail —— 和两端原来的写法一样。
//!
//! 一个日志一个句柄（同密钥库）：两端各持一个，界面要的条目每次取。写盘在后台（[`crate::writer`]，退出前 `core.flush`）。任意线程都能调。
//!
//! 原来是 Swift `Agent/AgentJournal.swift` 和 Qt `agent/AgentJournal.cpp` 各写一份。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use unicode_segmentation::UnicodeSegmentation;

pub const LIMIT: usize = 200;
pub const DETAIL_LIMIT: usize = 3000;
pub const KINDS: &[&str] = &["tool", "framework", "mcp", "recovered"];

#[derive(Debug, Default)]
pub struct Journal {
    /// None：只在内存里（远程副本、测试）
    file: Option<PathBuf>,
    /// 存档里的样子（新的在前）
    entries: Vec<Value>,
}

impl Journal {
    /// 读 journal.json（认不出 kind 的条目跳过）
    pub fn load(file: Option<PathBuf>) -> Journal {
        let entries = file
            .as_deref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| if let Value::Array(items) = value { Some(items) } else { None })
            .unwrap_or_default()
            .into_iter()
            .filter(|item| item.get("kind").and_then(Value::as_str).is_some_and(|kind| KINDS.contains(&kind)))
            .collect();
        Journal { file, entries }
    }

    /// 记一条：插到最前、截到 LIMIT 条、落盘。kind 认不出返回 false
    pub fn record(&mut self, kind: &str, title: &str, detail: &str) -> bool {
        if !KINDS.contains(&kind) {
            return false;
        }
        let detail: String = detail.graphemes(true).take(DETAIL_LIMIT).collect();
        let entry = json!({
            "id": uuid::Uuid::new_v4().to_string().to_uppercase(),
            "date": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            "kind": kind,
            "title": title,
            "detail": detail,
        });
        self.entries.insert(0, entry);
        self.entries.truncate(LIMIT);
        self.save();
        true
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.save();
    }

    /// 最新的 limit 条（不给 = 全部）
    pub fn entries(&self, limit: Option<usize>) -> &[Value] {
        &self.entries[..limit.unwrap_or(self.entries.len()).min(self.entries.len())]
    }

    /// 交给后台写（== 两端原来的后台写盘）：记一条很频繁，满了的文件快 2 MB，不在调用方的线程上写
    fn save(&self) {
        let Some(path) = &self.file else { return };
        let snapshot = self.entries.clone();
        crate::writer::write(path.clone(), move || Value::Array(snapshot).to_string().into_bytes());
    }
}

static JOURNALS: LazyLock<Mutex<HashMap<u64, Journal>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT: AtomicU64 = AtomicU64::new(1);

/// 打开数据目录里的日志（directory 为 None：只在内存里），返回句柄
pub fn open(directory: Option<&Path>) -> u64 {
    let journal = Journal::load(directory.map(|directory| directory.join("journal.json")));
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    JOURNALS.lock().unwrap().insert(id, journal);
    id
}

pub fn close(id: u64) {
    JOURNALS.lock().unwrap().remove(&id);
}

pub fn with<T>(id: u64, body: impl FnOnce(&mut Journal) -> T) -> Option<T> {
    JOURNALS.lock().unwrap().get_mut(&id).map(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording() {
        let directory = std::env::temp_dir().join(format!("friday-core-journal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let id = open(Some(&directory));
        with(id, |journal| {
            assert!(!journal.record("nope", "x", "y"));
            for index in 0..205 {
                assert!(journal.record("tool", &format!("第 {index} 条"), &"长".repeat(3500)));
            }
            assert_eq!(journal.entries(None).len(), LIMIT);
            assert_eq!(journal.entries(Some(15)).len(), 15);
            let newest = &journal.entries(None)[0];
            assert_eq!(newest["title"], json!("第 204 条"));
            assert_eq!(newest["detail"].as_str().unwrap().chars().count(), DETAIL_LIMIT);
            let id = newest["id"].as_str().unwrap();
            assert!(id.len() == 36 && id == id.to_uppercase());
            assert!(newest["date"].as_str().unwrap().ends_with('Z') && !newest["date"].as_str().unwrap().contains('.'));
        });
        close(id);
        crate::writer::flush();
        // 重新打开还在；认不出 kind 的跳过
        let text = std::fs::read_to_string(directory.join("journal.json")).unwrap();
        let mut items: Vec<Value> = serde_json::from_str(&text).unwrap();
        items.push(json!({"id": "X", "kind": "future", "title": "", "detail": ""}));
        std::fs::write(directory.join("journal.json"), Value::Array(items).to_string()).unwrap();
        let id = open(Some(&directory));
        assert_eq!(with(id, |journal| journal.entries(None).len()), Some(LIMIT));
        with(id, Journal::clear);
        close(id);
        crate::writer::flush();
        assert_eq!(Journal::load(Some(directory.join("journal.json"))).entries(None).len(), 0);
        // 只在内存里
        let memory = open(None);
        with(memory, |journal| journal.record("mcp", "a", "b"));
        assert_eq!(with(memory, |journal| journal.entries(None).len()), Some(1));
        close(memory);
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
