//! 和 Swift `JSONEncoder`（`.prettyPrinted, .sortedKeys, .withoutEscapingSlashes`）/ Qt `Json::stringify` 逐字一致的 JSON 文本：
//! 键排序、两空格缩进、`"键" : 值`、空对象 / 空数组中间一个空行、不转义「/」、控制字符写成 \uXXXX。
//! 存档文件两端（和降级回去的旧版）都要能读，写出来的样子和以前一样，diff 也不会整份变。

use serde_json::Value;

/// pretty 文本（存档用）
pub fn pretty(value: &Value) -> String {
    let mut out = String::new();
    write(&mut out, value, true, 0);
    out
}

/// 紧凑文本（键排序、不转义「/」）
pub fn compact(value: &Value) -> String {
    let mut out = String::new();
    write(&mut out, value, false, 0);
    out
}

fn newline(out: &mut String, pretty: bool, level: usize) {
    if pretty {
        out.push('\n');
        out.push_str(&"  ".repeat(level));
    }
}

fn write(out: &mut String, value: &Value, pretty: bool, indent: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&number_text(number)),
        Value::String(text) => escaped(out, text),
        Value::Array(items) => {
            out.push('[');
            if items.is_empty() {
                if pretty {
                    out.push('\n');
                    newline(out, pretty, indent);
                }
            } else {
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    newline(out, pretty, indent + 1);
                    write(out, item, pretty, indent + 1);
                }
                newline(out, pretty, indent);
            }
            out.push(']');
        }
        Value::Object(object) => {
            out.push('{');
            if object.is_empty() {
                if pretty {
                    out.push('\n');
                    newline(out, pretty, indent);
                }
            } else {
                let mut keys: Vec<&String> = object.keys().collect();
                keys.sort();
                for (index, key) in keys.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    newline(out, pretty, indent + 1);
                    escaped(out, key);
                    out.push_str(if pretty { " : " } else { ":" });
                    write(out, &object[*key], pretty, indent + 1);
                }
                newline(out, pretty, indent);
            }
            out.push('}');
        }
    }
}

/// 整数写成整数，其余最短表示（== Qt Json 的 formatNumber）
fn number_text(number: &serde_json::Number) -> String {
    if number.is_i64() || number.is_u64() {
        return number.to_string();
    }
    let value = number.as_f64().unwrap_or(0.0);
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

fn escaped(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn swift_style() {
        let value = json!({"b": {"y": [1, 2.5, "a/b"], "x": {}}, "a": [], "c": "引号\"\n\u{1}"});
        assert_eq!(
            pretty(&value),
            "{\n  \"a\" : [\n\n  ],\n  \"b\" : {\n    \"x\" : {\n\n    },\n    \"y\" : [\n      1,\n      2.5,\n      \"a/b\"\n    ]\n  },\n  \"c\" : \"引号\\\"\\n\\u0001\"\n}"
        );
        assert_eq!(compact(&value), r#"{"a":[],"b":{"x":{},"y":[1,2.5,"a/b"]},"c":"引号\"\n\u0001"}"#);
        assert_eq!(pretty(&json!({})), "{\n\n}");
        assert_eq!(compact(&json!(3.0)), "3");
    }
}
