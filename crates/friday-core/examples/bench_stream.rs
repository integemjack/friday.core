//! 流式解析过一次接口要多久：`cargo run --release --example bench_stream`
//! （每个 SSE chunk 原样拼进 {"chunk":…,"stream":N} 交给 stream.feed，和两端的调法一样）

use std::time::Instant;

use friday_core::api;

fn main() {
    let opened: serde_json::Value = serde_json::from_str(&api::call("stream.open", r#"{"showReasoning":true,"quota":true}"#)).unwrap();
    let id = opened["result"]["stream"].as_u64().unwrap();
    let count: usize = std::env::args().nth(1).and_then(|n| n.parse().ok()).unwrap_or(3_000);
    let started = Instant::now();
    for index in 0..count {
        let text = format!("第 {index} 段：模型正在写的一小段中文和 some English words，");
        let payload = serde_json::json!({
            "id": "chatcmpl-1", "object": "chat.completion.chunk", "model": "glm-5",
            "choices": [{ "index": 0, "delta": { "content": text } }],
        })
        .to_string();
        let out = api::call("stream.feed", &format!(r#"{{"chunk":{payload},"stream":{id}}}"#));
        assert!(out.starts_with(r#"{"result":{"chunks""#), "{out}");
    }
    let elapsed = started.elapsed();
    let result = api::call("stream.finish", &format!(r#"{{"stream":{id}}}"#));
    println!("{count} 个 chunk：{:.1} µs / 个，正文 {} 字节", elapsed.as_secs_f64() * 1e6 / count as f64, result.len());
}
