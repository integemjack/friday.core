//! 拿真实的 conversations.json 检查数据模型（只读，不写回）：cargo run --release --example check_archive <conversations.json>...
//! 每段会话读进来再写出去，和原文按 JSON 值比（数字按数值比：12 和 12.0 算一样）；规整改过的单独数出来
use std::time::Instant;

use serde_json::Value;

fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(key, value)| y.get(key).is_some_and(|other| same(value, other)))
        }
        _ => a == b,
    }
}

fn first_difference(path: &str, a: &Value, b: &Value) -> Option<String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for key in x.keys().chain(y.keys()) {
                match (x.get(key), y.get(key)) {
                    (Some(p), Some(q)) => {
                        if let Some(found) = first_difference(&format!("{path}.{key}"), p, q) {
                            return Some(found);
                        }
                    }
                    _ => return Some(format!("{path}.{key}：只在一边")),
                }
            }
            None
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            x.iter().zip(y).enumerate().find_map(|(index, (p, q))| first_difference(&format!("{path}[{index}]"), p, q))
        }
        _ if same(a, b) => None,
        _ => Some(format!("{path}：{} ≠ {}", truncate(a), truncate(b))),
    }
}

fn truncate(value: &Value) -> String {
    let text = value.to_string();
    if text.chars().count() > 80 { text.chars().take(80).collect::<String>() + "…" } else { text }
}

fn main() {
    let team = ["delegate_task", "run_parallel_tasks", "rework_agent", "run_workflow"].map(str::to_string);
    for path in std::env::args().skip(1) {
        let text = std::fs::read_to_string(&path).unwrap();
        let Value::Array(original) = serde_json::from_str::<Value>(&text).unwrap() else { panic!("不是数组") };
        let start = Instant::now();
        let loaded = friday_core::conversations::load(std::path::Path::new(&path), &team);
        let elapsed = start.elapsed();
        let mut identical = 0;
        let mut normalized = 0;
        let mut different = Vec::new();
        for conversation in &loaded {
            let written = serde_json::to_value(conversation).unwrap();
            let source = original.iter().find(|item| item["id"].as_str() == Some(conversation.id.as_str())).unwrap();
            if same(&written, source) {
                identical += 1;
            } else if let Some(difference) = first_difference("", &written, source) {
                // 规整会改的：中断的调用、清掉的统计、补的块、旧存档的 messages
                if difference.contains("status")
                    || difference.contains("result")
                    || difference.contains("stats")
                    || difference.contains("blocks")
                    || difference.contains("messages")
                    || difference.contains("queued")
                    || difference.contains("nodes")
                {
                    normalized += 1;
                }
                different.push(format!("{}：{difference}", conversation.id));
            }
        }
        println!(
            "{path}：{} KB，存档 {} 段，读出 {} 段（{:?}）；原样 {identical}，规整过 {normalized}，其他不同 {}",
            text.len() / 1000,
            original.len(),
            loaded.len(),
            elapsed,
            different.len() - normalized
        );
        for line in different.iter().take(8) {
            println!("  {line}");
        }
    }
}
