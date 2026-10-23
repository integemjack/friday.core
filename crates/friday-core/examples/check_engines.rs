//! 拿真实的 engines.json 看规整会改什么（只读，不写回）：cargo run --release --example check_engines <engines.json>
use serde_json::Value;

fn main() {
    for path in std::env::args().skip(1) {
        let Ok(Value::Array(original)) = serde_json::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()) else {
            panic!("不是数组")
        };
        let written = serde_json::to_value(friday_core::engines::load(std::path::Path::new(&path))).unwrap();
        let written = written.as_array().unwrap();
        println!("{path}：{} 个引擎，读出 {}", original.len(), written.len());
        for (before, after) in original.iter().zip(written) {
            let (before, after) = (before.as_object().unwrap(), after.as_object().unwrap());
            for key in before.keys().chain(after.keys()).collect::<std::collections::BTreeSet<_>>() {
                let (a, b) = (before.get(key), after.get(key));
                let same = match (a, b) {
                    (Some(Value::Number(x)), Some(Value::Number(y))) => x.as_f64() == y.as_f64(),
                    _ => a == b,
                };
                if !same {
                    println!("  {}：{key} {:?} → {:?}", before["id"], a, b);
                }
            }
        }
    }
}
