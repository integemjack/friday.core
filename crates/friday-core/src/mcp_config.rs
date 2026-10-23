//! MCP 服务的配置（== Swift `MCP/MCPManager.swift` 的 MCPConfigFile / MCPServerConfig / qualifiedName、Qt `mcp/MCPManager.cpp`）：
//! mcp.json 的类型检查（照 Swift JSONDecoder 失败时的 zh-Hans 说明）、读别处复制来的配置（Claude Desktop / Cursor / VS Code / Windsurf
//! 的各种写法）、server 名合不合法、工具的全名（<server>__<tool>）、读写 mcp.json（Swift JSONEncoder pretty + 键排序 + 不转义斜杠）。
//! 起 server 进程、连接、调工具留在两端（第 4 阶段后面再搬）。

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};
use unicode_segmentation::UnicodeSegmentation;

use crate::{swift_json, writer};

/// JSONDecoder 失败时的 localizedDescription（NSCocoaErrorDomain 4864 / 4865 的 zh-Hans 文案）
pub const CORRUPTED: &str = "未能读取数据，因为它的格式不正确。";
pub const MISSING: &str = "未能读取数据，因为数据丢失。";

/// 没有 mcp.json 时编辑框里的样子
pub const EMPTY_CONFIG: &str = "{\n  \"mcpServers\": {\n  }\n}\n";

fn absent(value: Option<&Value>) -> bool {
    matches!(value, None | Some(Value::Null))
}

/// 一组字符串（数组项或对象的值）：null 是「数据丢失」，别的类型是「格式不正确」
fn strings<'a>(values: impl Iterator<Item = &'a Value>) -> Option<&'static str> {
    for value in values {
        match value {
            Value::Null => return Some(MISSING),
            Value::String(_) => {}
            _ => return Some(CORRUPTED),
        }
    }
    None
}

/// 一个 server 的类型检查（== JSONDecoder.decode(MCPServerConfig.self)）：失败原因，没问题为 None
fn server_problem(config: &Map<String, Value>) -> Option<&'static str> {
    for key in ["command", "url"] {
        if !absent(config.get(key)) && !config[key].is_string() {
            return Some(CORRUPTED);
        }
    }
    match config.get("args") {
        None | Some(Value::Null) => {}
        Some(Value::Array(items)) => {
            if let Some(problem) = strings(items.iter()) {
                return Some(problem);
            }
        }
        Some(_) => return Some(CORRUPTED),
    }
    for key in ["env", "headers"] {
        match config.get(key) {
            None | Some(Value::Null) => {}
            Some(Value::Object(map)) => {
                if let Some(problem) = strings(map.values()) {
                    return Some(problem);
                }
            }
            Some(_) => return Some(CORRUPTED),
        }
    }
    for key in ["disabled", "autoApprove"] {
        if !absent(config.get(key)) && !config[key].is_boolean() {
            return Some(CORRUPTED);
        }
    }
    None
}

/// 整个 mcp.json 的类型检查（== JSONDecoder.decode(MCPConfigFile.self)）：失败原因，没问题为 None
pub fn decode_problem(root: &Map<String, Value>) -> Option<&'static str> {
    // mcpServers 是非可选属性（带默认值的 var 解码时仍是必填）
    let servers = match root.get("mcpServers") {
        None | Some(Value::Null) => return Some(MISSING),
        Some(Value::Object(servers)) => servers,
        Some(_) => return Some(CORRUPTED),
    };
    for value in servers.values() {
        match value {
            Value::Null => return Some(MISSING),
            Value::Object(config) => {
                if let Some(problem) = server_problem(config) {
                    return Some(problem);
                }
            }
            _ => return Some(CORRUPTED),
        }
    }
    None
}

/// 只留认得的键（== MCPServerConfig 编码回去的样子：没有的不写）
fn normalized(config: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for key in ["command", "args", "env", "url", "headers", "disabled", "autoApprove"] {
        if let Some(value) = config.get(key).filter(|value| !value.is_null()) {
            out.insert(key.to_string(), value.clone());
        }
    }
    out
}

/// server 名：字母、数字、下划线、连字符，1–40 个字符
pub fn is_valid_name(name: &str) -> bool {
    static NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\A[A-Za-z0-9_-]{1,40}\z").unwrap());
    NAME.is_match(name)
}

/// 工具的全名：<server>__<tool>，字母、数字、_、- 之外的每个字换成一个 _
pub fn qualified_name(server: &str, tool: &str) -> String {
    let clean = |text: &str| -> String {
        text.graphemes(true)
            .map(|cluster| {
                let first = cluster.chars().next().unwrap_or(' ');
                if first.is_alphanumeric() || first == '_' || first == '-' { cluster.to_string() } else { "_".to_string() }
            })
            .collect()
    };
    format!("{}__{}", clean(server), clean(tool))
}

/// 读别处复制来的配置：Claude Desktop / Cursor 的 {"mcpServers": {…}}、VS Code 的 {"servers": {…}}、直接是 {"名字": {command…}}，
/// 或者单个 server 的 {command…}（这时用 fallback_name）。结果：名字 → 配置；不对时 Err((是不是参数问题, 说明))
pub fn parse_servers(text: &str, fallback_name: Option<&str>) -> Result<Map<String, Value>, (bool, String)> {
    let object = |source: &str| -> Option<Map<String, Value>> {
        let mut trimmed = source.trim().to_string();
        // 从文档里复制的片段常常是 "name": {…}，没有外层大括号
        if trimmed.starts_with('"') {
            trimmed = format!("{{{trimmed}}}");
        }
        serde_json::from_str::<Value>(&trimmed).ok().and_then(|value| value.as_object().cloned())
    };
    // 解析不了再试一次把弯引号换成直引号：手打的或经过富文本的引号会变成弯引号
    let root = object(text)
        .or_else(|| object(&text.replace(['\u{201C}', '\u{201D}', '\u{201E}'], "\"")))
        .ok_or_else(|| (true, "不是有效的 JSON 对象".to_string()))?;
    // 一个 server：要有 command 或 url（Windsurf 写 serverUrl）；类型不对时失败（== JSONDecoder.decode）
    let server = |value: &Value| -> Result<Option<Map<String, Value>>, (bool, String)> {
        let Some(dict) = value.as_object() else { return Ok(None) };
        if !dict.contains_key("command") && !dict.contains_key("url") && !dict.contains_key("serverUrl") {
            return Ok(None);
        }
        let mut dict = dict.clone();
        if !dict.contains_key("url")
            && let Some(url) = dict.get("serverUrl").cloned()
        {
            dict.insert("url".into(), url);
        }
        if let Some(problem) = server_problem(&dict) {
            return Err((false, problem.to_string()));
        }
        Ok(Some(normalized(&dict)))
    };
    let servers = |map: &Map<String, Value>| -> Result<Map<String, Value>, (bool, String)> {
        let mut result = Map::new();
        for (name, value) in map {
            if let Some(config) = server(value)? {
                result.insert(name.clone(), Value::Object(config));
            }
        }
        Ok(result)
    };
    let nested = root.get("mcpServers").or_else(|| root.get("servers")).and_then(Value::as_object);
    let result = if let Some(map) = nested {
        servers(map)?
    } else if let Some(single) = server(&Value::Object(root.clone()))? {
        let Some(name) = fallback_name.filter(|name| is_valid_name(name)) else {
            return Err((true, "这段配置只有一个 server，没有名字：外面套上 {\"名字\": …} 再粘贴".into()));
        };
        Map::from_iter([(name.to_string(), Value::Object(single))])
    } else {
        servers(&root)?
    };
    if result.is_empty() {
        return Err((true, "没有找到 server 配置（每个 server 需要 command 或 url）".into()));
    }
    let mut names: Vec<&String> = result.keys().collect();
    names.sort();
    if let Some(bad) = names.into_iter().find(|name| !is_valid_name(name)) {
        return Err((true, format!("server 名「{bad}」只能包含字母、数字、下划线、连字符")));
    }
    Ok(result)
}

/// 读 mcp.json：没有、读不出、格式不对都当空的（== try? decode，失败给空配置）
pub fn load(path: &Path) -> Map<String, Value> {
    let empty = || Map::from_iter([("mcpServers".to_string(), json!({}))]);
    let Some(root) =
        std::fs::read(path).ok().and_then(|data| serde_json::from_slice::<Value>(&data).ok()).and_then(|value| value.as_object().cloned())
    else {
        return empty();
    };
    if decode_problem(&root).is_some() {
        return empty();
    }
    let servers: Map<String, Value> = root["mcpServers"]
        .as_object()
        .map(|servers| {
            servers.iter().map(|(name, config)| (name.clone(), Value::Object(normalized(config.as_object().unwrap())))).collect()
        })
        .unwrap_or_default();
    Map::from_iter([("mcpServers".to_string(), Value::Object(servers))])
}

/// 编辑框里的原文：读得出 UTF-8 就原样，否则给一份空的
pub fn config_text(path: &Path) -> String {
    std::fs::read(path).ok().and_then(|data| String::from_utf8(data).ok()).unwrap_or_else(|| EMPTY_CONFIG.to_string())
}

fn write_failure(path: &Path) -> String {
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let folder = path.parent().and_then(Path::file_name).map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    format!("未能将文件“{name}”保存到文件夹“{folder}”中。")
}

/// 用户在编辑框里改的原文：先查格式（不对给「JSON 格式不正确：…」），对了原样写进去
pub fn save_text(path: &Path, text: &str) -> Result<(), String> {
    let root = serde_json::from_str::<Value>(text).ok().and_then(|value| value.as_object().cloned());
    let Some(root) = root else { return Err(format!("JSON 格式不正确：{CORRUPTED}")) };
    if let Some(problem) = decode_problem(&root) {
        return Err(format!("JSON 格式不正确：{problem}"));
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    writer::write_now(path, text.as_bytes()).map_err(|_| write_failure(path))
}

/// 程序改的配置（停用、信任、加、删）：按 Swift JSONEncoder 的写法写回
pub fn save(path: &Path, servers: &Map<String, Value>) -> Result<(), String> {
    let normalized: Map<String, Value> = servers
        .iter()
        .map(|(name, config)| (name.clone(), Value::Object(config.as_object().map(normalized).unwrap_or_default())))
        .collect();
    let text = swift_json::pretty(&json!({ "mcpServers": normalized }));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    writer::write_now(path, text.as_bytes()).map_err(|_| write_failure(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pasted_configs() {
        let claude = r#"{"mcpServers": {"fs": {"command": "npx", "args": ["-y", "x"], "extra": 1}}}"#;
        assert_eq!(parse_servers(claude, None).unwrap()["fs"], json!({ "command": "npx", "args": ["-y", "x"] }));
        let vscode = r#"{"servers": {"web": {"url": "https://x/mcp", "headers": {"A": "b"}}}}"#;
        assert_eq!(parse_servers(vscode, None).unwrap()["web"]["headers"], json!({ "A": "b" }));
        let windsurf = r#""wind": {"serverUrl": "https://w/sse"}"#;
        assert_eq!(parse_servers(windsurf, None).unwrap()["wind"]["url"], "https://w/sse");
        let curly = "{“a”: {“command”: “uvx”}}";
        assert_eq!(parse_servers(curly, None).unwrap()["a"]["command"], "uvx");
        assert_eq!(parse_servers(r#"{"command": "x"}"#, Some("solo")).unwrap()["solo"]["command"], "x");
        assert!(parse_servers(r#"{"command": "x"}"#, None).unwrap_err().1.starts_with("这段配置只有一个 server"));
        assert_eq!(parse_servers("not json", None).unwrap_err(), (true, "不是有效的 JSON 对象".into()));
        assert!(parse_servers(r#"{"a": {"x": 1}}"#, None).unwrap_err().1.starts_with("没有找到 server 配置"));
        assert_eq!(
            parse_servers(r#"{"bad name": {"command": "x"}}"#, None).unwrap_err().1,
            "server 名「bad name」只能包含字母、数字、下划线、连字符"
        );
        assert_eq!(parse_servers(r#"{"a": {"command": 1}}"#, None).unwrap_err(), (false, CORRUPTED.into()));
        assert_eq!(parse_servers(r#"{"a": {"command": "x", "args": [null]}}"#, None).unwrap_err(), (false, MISSING.into()));
    }

    #[test]
    fn names_and_files() {
        assert!(is_valid_name("a-b_1") && !is_valid_name("") && !is_valid_name(&"a".repeat(41)) && !is_valid_name("中"));
        assert_eq!(qualified_name("my server", "read.file"), "my_server__read_file");
        assert_eq!(qualified_name("文件", "读"), "文件__读");
        let dir = std::env::temp_dir().join(format!("friday-core-mcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("mcp.json");
        assert_eq!(load(&path), Map::from_iter([("mcpServers".to_string(), json!({}))]));
        assert_eq!(config_text(&path), EMPTY_CONFIG);
        let servers = Map::from_iter([("fs".to_string(), json!({ "command": "/usr/bin/x", "disabled": true }))]);
        save(&path, &servers).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"mcpServers\" : {\n    \"fs\" : {\n      \"command\" : \"/usr/bin/x\",\n      \"disabled\" : true\n    }\n  }\n}"
        );
        assert_eq!(load(&path)["mcpServers"]["fs"]["command"], "/usr/bin/x");
        assert_eq!(save_text(&path, r#"{"mcpServers": 1}"#).unwrap_err(), format!("JSON 格式不正确：{CORRUPTED}"));
        assert_eq!(save_text(&path, "{}").unwrap_err(), format!("JSON 格式不正确：{MISSING}"));
        save_text(&path, "{\"mcpServers\": {}}\n").unwrap();
        assert_eq!(config_text(&path), "{\"mcpServers\": {}}\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
