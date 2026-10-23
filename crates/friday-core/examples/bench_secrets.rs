use std::collections::{HashMap, HashSet};
use std::time::Instant;

use friday_core::secret_guard::Guard;

fn main() {
    let path = std::env::args().nth(1);
    let text = match path {
        Some(path) => std::fs::read_to_string(path).unwrap(),
        None => {
            let chunk = "let tokensPerSecond = 12.5; // 统计 password: required\nfunc load() { api_key = os.environ.get(\"X\") }\n{\"role\":\"user\",\"content\":\"看下 https://github.com/a/b 的 README，commit 3f2a9c1d8e7b6a5f4e3d2c1b0a9f8e7d6c5b4a39\"}\n";
            chunk.repeat(20_000_000 / chunk.len())
        }
    };
    let guard = Guard::default();
    let values: HashMap<String, String> = (0..20).map(|i| (format!("KEY_{i}"), format!("AbCdEf{i:04}GhIjKlMnOpQr"))).collect();
    guard.update(&values, HashSet::new(), &HashMap::new());
    println!("文本 {} MB", text.len() / 1_000_000);
    let start = Instant::now();
    let found = guard.detect(&text, true);
    println!("detect(strongOnly) {:?}，认出 {}", start.elapsed(), found.len());
    let start = Instant::now();
    let found = guard.detect(&text, false);
    println!("detect(全部) {:?}，认出 {}", start.elapsed(), found.len());
    let start = Instant::now();
    let masked = guard.mask_stored(&text);
    println!("mask_stored {:?}，{} MB", start.elapsed(), masked.len() / 1_000_000);
    let body = serde_json::json!({"messages": (0..2000).map(|i| serde_json::json!({"role": "user", "content": format!("{} {i}", &text[..500])})).collect::<Vec<_>>()});
    let json = body.to_string();
    let start = Instant::now();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let masked = guard.mask_json(value);
    let out = masked.to_string();
    println!("请求体 {} KB：解析 + 遮 + 写回 {:?}", out.len() / 1000, start.elapsed());
}
