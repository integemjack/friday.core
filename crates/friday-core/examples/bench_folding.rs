//! 量 fold.* 经统一入口（含 JSON 解析和写回）的开销：cargo run --release --example bench_folding
use std::time::Instant;

use serde_json::json;

fn main() {
    let entries: Vec<_> = (0..400)
        .map(|i| json!({"role": if i % 2 == 0 { "assistant" } else { "tool" }, "name": "run_shell", "content": format!("第 {i} 步 ") + &"内容".repeat(1_200)}))
        .collect();
    let history = json!(entries);
    let size = history.to_string().len();
    let params = json!({"entries": history}).to_string();
    let start = Instant::now();
    for _ in 0..10 {
        friday_core::api::call("fold.estimate", &params);
    }
    println!("历史 {} KB：fold.estimate 每次 {:?}", size / 1000, start.elapsed() / 10);
    let params =
        json!({"history": entries, "folded": 0, "budget": 30_000, "taskStateTool": "update_task_state", "wholeTools": ["delegate_task"]})
            .to_string();
    let start = Instant::now();
    let response = friday_core::api::call("fold.session", &params);
    println!("fold.session（折叠并写回 {} KB）{:?}", response.len() / 1000, start.elapsed());
}
