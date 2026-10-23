//! v1.20：模型常照别家（Claude Code、Cline、Cursor……）的叫法调工具：read_file 的路径写成 file / file_path，调 search_content、grep、
//! glob、bash、str_replace 这些 Friday 没有的名字，每次都白白失败一轮。派发前把这些叫法换成 Friday 的：
//! - 工具名：注册表里没有这个名字（不是脚本 / MCP 工具）时，按别名表换成内置工具（不分大小写：Claude Code 的 Grep、Bash、WebFetch、MultiEdit 也认）。
//!   注册表在两端，「有没有这个工具」由包装层判断：没有才调 [`canonical_name`]，换出来的名字也在注册表里才用
//! - 参数名：内置工具缺了自己的参数、却有别名时改过来（file_path → path、old_string → old_text……）；Claude Code 的 timeout 是毫秒，
//!   换成 timeout_seconds 时大于 600 的按毫秒折算；include / type 只写了扩展名（.qml、cpp）时补成 *.qml
//!
//! 原来是 Swift `Agent/ToolAliases.swift` 和 Qt `agent/ToolAliases.cpp` 各写一份，别名表逐条照搬。

use serde_json::{Map, Value};

/// 内置工具名 → 别家的叫法
const NAMES: &[(&str, &[&str])] = &[
    ("read_file", &["read", "cat", "view", "view_file", "open_file", "readfile", "read_text_file"]),
    ("write_file", &["write", "create_file", "write_to_file", "writefile"]),
    ("edit_file", &["edit", "str_replace", "replace_in_file", "apply_edit", "edit_text", "multi_edit", "multiedit"]),
    ("find_files", &["glob", "find", "find_file", "search_file", "file_search", "list_files"]),
    ("search_files", &["grep", "search_content", "search_code", "code_search", "grep_search", "ripgrep", "rg", "search_in_files"]),
    ("search_keywords", &["keyword_search", "search_keyword", "keywords_search", "search_text", "full_text_search"]),
    ("list_directory", &["ls", "list_dir", "listdir", "list_folder"]),
    (
        "run_shell",
        &["bash", "shell", "sh", "exec", "execute", "execute_command", "run_command", "run_terminal_cmd", "terminal", "powershell", "cmd"],
    ),
    ("fetch_url", &["fetch", "web_fetch", "webfetch", "open_url", "browse"]),
    ("web_search", &["search_web", "websearch", "google_search", "internet_search"]),
    ("delegate_task", &["task"]),
    // v2.61 Claude Code 的 SlashCommand 工具
    ("use_command", &["slashcommand", "slash_command"]),
];

const FILE_PATH: &[&str] = &["file_path", "file", "filepath", "filename", "target_file"];
const DIRECTORY: &[&str] = &["directory", "dir", "folder", "root", "search_path", "target_directory"];
const DIRECTORY_OR_FILE: &[&str] = &["directory", "dir", "folder", "root", "search_path", "target_directory", "file"];
const OLD_TEXT: &[&str] = &["old_string", "old_str", "old", "search", "original_text", "find"];
const NEW_TEXT: &[&str] = &["new_string", "new_str", "new", "replace", "replacement", "updated_text"];
const GLOB: &[&str] = &["include", "file_pattern", "files", "fileTypes", "file_types", "type"];

type Aliases = &'static [(&'static str, &'static [&'static str])];

/// 内置工具的参数名 → 别家的叫法（自己的参数没给时才换）
fn parameters(tool: &str) -> Option<Aliases> {
    Some(match tool {
        "read_file" => &[("path", FILE_PATH), ("offset", &["start_line", "line_offset"]), ("limit", &["line_count", "max_lines"])],
        "write_file" => &[("path", FILE_PATH), ("content", &["text", "contents", "file_text", "data", "code"])],
        "edit_file" => &[("path", FILE_PATH), ("old_text", OLD_TEXT), ("new_text", NEW_TEXT), ("replace_all", &["replaceAll", "all"])],
        "find_files" => &[("pattern", &["glob", "name", "file_pattern", "query"]), ("path", DIRECTORY)],
        "search_files" => &[
            ("pattern", &["query", "regex", "search", "keyword", "text", "search_term"]),
            ("path", DIRECTORY_OR_FILE),
            ("glob", GLOB),
            ("ignore_case", &["case_insensitive", "ignoreCase", "insensitive", "i", "-i"]),
        ],
        "search_keywords" => &[
            ("keywords", &["keyword", "query", "words", "terms", "text", "search", "pattern"]),
            ("path", DIRECTORY_OR_FILE),
            ("glob", GLOB),
            ("match_any", &["any", "or"]),
            ("match_case", &["case_sensitive", "caseSensitive"]),
        ],
        "list_directory" => {
            &[("path", &["directory", "dir", "folder", "root", "search_path", "target_directory", "relative_workspace_path"])]
        }
        "run_shell" => &[("command", &["cmd", "script", "command_line", "commandLine"]), ("timeout_seconds", &["timeout", "timeout_secs"])],
        "fetch_url" => &[("url", &["uri", "link", "href", "page"])],
        "web_search" => &[("query", &["q", "search", "search_query", "keywords", "keyword", "term"])],
        "delegate_task" => &[("task", &["prompt", "instructions"]), ("name", &["description", "title"])],
        "use_command" => &[("command", &["name", "slash_command"]), ("arguments", &["args", "argument"])],
        _ => return None,
    })
}

/// 工具名的别名换成内置工具名；不是别名返回原样
pub fn canonical_name(name: &str) -> String {
    let lower = trim_spaces(name).to_lowercase();
    NAMES
        .iter()
        .find(|(target, aliases)| *target == lower || aliases.contains(&lower.as_str()))
        .map_or_else(|| name.to_string(), |(target, _)| target.to_string())
}

/// 去掉首尾的空格和制表符（== Swift 的 `.whitespaces`：Unicode Zs 类 + U+0009，不含换行）
fn trim_spaces(text: &str) -> &str {
    let space = |c: char| matches!(c, '\t' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}');
    text.trim_matches(space)
}

fn renamed(arguments: &Map<String, Value>, aliases: Aliases) -> Map<String, Value> {
    let mut result = arguments.clone();
    for (key, others) in aliases {
        if result.contains_key(*key) {
            continue;
        }
        if let Some(value) = others.iter().find_map(|other| result.remove(*other)) {
            result.insert(key.to_string(), value);
        }
    }
    result
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

/// 内置工具 tool（已经是换过的名字）的参数里，别家的叫法换成 Friday 的；没有别名表的工具原样返回
pub fn canonical_arguments(tool: &str, original: &Map<String, Value>) -> Map<String, Value> {
    let Some(aliases) = parameters(tool) else { return original.clone() };
    let mut arguments = renamed(original, aliases);
    if tool == "edit_file"
        && let Some(Value::Array(items)) = arguments.get_mut("edits")
    {
        // Claude Code 的 MultiEdit：edits 里也是 old_string / new_string
        for item in items.iter_mut() {
            if let Value::Object(object) = item {
                *object = renamed(object, aliases);
            }
        }
    }
    if tool == "run_shell"
        && !original.contains_key("timeout_seconds")
        && original.contains_key("timeout")
        && let Some(value) = number(arguments.get("timeout_seconds")).filter(|value| *value > 600.0)
    {
        // Claude Code 的 timeout 是毫秒：大于 600 的按毫秒折算成秒
        arguments.insert("timeout_seconds".to_string(), Value::from((value / 1000.0) as i64));
    }
    if (tool == "search_files" || tool == "search_keywords")
        && !original.contains_key("glob")
        && let Some(Value::String(raw)) = arguments.get("glob")
    {
        // include: ".qml" / type: "qml" 这类只写了扩展名的补成通配
        let glob = trim_spaces(raw);
        let fixed = if glob.starts_with('.') && !glob.contains('*') {
            Some(format!("*{glob}"))
        } else if !glob.is_empty() && glob.chars().all(|c| c.is_ascii_alphanumeric()) {
            Some(format!("*.{glob}"))
        } else {
            None
        };
        if let Some(fixed) = fixed {
            arguments.insert("glob".to_string(), Value::from(fixed));
        }
    }
    arguments
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    /// == Swift 自检和 Qt TestTools::toolAliases 的同一组（名字已经由包装层按注册表换好）
    #[test]
    fn aliases() {
        assert_eq!(
            canonical_arguments("read_file", &args(json!({"file": "a.txt", "limit": 20}))),
            args(json!({"path": "a.txt", "limit": 20}))
        );
        // 自己的参数已经给了就不动
        assert_eq!(
            canonical_arguments("read_file", &args(json!({"path": "a", "file_path": "b"}))),
            args(json!({"path": "a", "file_path": "b"}))
        );
        // 工具名不分大小写（Claude Code 的 Grep / Glob / Bash / MultiEdit）
        assert_eq!(canonical_name("Grep"), "search_files");
        assert_eq!(
            canonical_arguments("search_files", &args(json!({"query": "openSettings", "directory": "src", "include": ".qml", "-i": true}))),
            args(json!({"pattern": "openSettings", "path": "src", "glob": "*.qml", "ignore_case": true}))
        );
        assert_eq!(canonical_arguments("search_files", &args(json!({"pattern": "x", "type": "cpp"})))["glob"], json!("*.cpp"));
        assert_eq!(canonical_name("glob"), "find_files");
        // v2.8 search_keywords
        assert_eq!(canonical_name("keyword_search"), "search_keywords");
        assert_eq!(
            canonical_arguments("search_keywords", &args(json!({"query": "a b", "include": ".qml", "any": true}))),
            args(json!({"keywords": "a b", "glob": "*.qml", "match_any": true}))
        );
        // Claude Code 的 timeout 是毫秒
        assert_eq!(canonical_name("Bash"), "run_shell");
        assert_eq!(canonical_arguments("run_shell", &args(json!({"command": "ls", "timeout": 120000})))["timeout_seconds"], json!(120));
        assert_eq!(canonical_arguments("run_shell", &args(json!({"cmd": "ls", "timeout": 90})))["timeout_seconds"], json!(90));
        assert_eq!(canonical_arguments("run_shell", &args(json!({"cmd": "ls", "timeout": "1500"})))["timeout_seconds"], json!(1));
        // 自己写了 timeout_seconds 的不折算
        assert_eq!(
            canonical_arguments("run_shell", &args(json!({"cmd": "ls", "timeout_seconds": 900, "timeout": 5})))["timeout_seconds"],
            json!(900)
        );
        // MultiEdit：edits 里的 old_string / new_string 也换
        assert_eq!(canonical_name("MultiEdit"), "edit_file");
        let edit =
            canonical_arguments("edit_file", &args(json!({"file_path": "a.cpp", "edits": [{"old_string": "a", "new_string": "b"}, 3]})));
        assert_eq!(edit, args(json!({"path": "a.cpp", "edits": [{"old_text": "a", "new_text": "b"}, 3]})));
        // 不认识的名字原样；没有别名表的工具参数原样
        assert_eq!(canonical_name("no_such_tool"), "no_such_tool");
        assert_eq!(canonical_name("WebFetch"), "fetch_url");
        assert_eq!(canonical_name("SlashCommand"), "use_command");
        assert_eq!(canonical_name(" Read_File\t"), "read_file");
        assert_eq!(canonical_name("bash\n"), "bash\n");
        assert_eq!(canonical_arguments("my_grep", &args(json!({"query": "x"}))), args(json!({"query": "x"})));
        // 已经是通配的、带路径的不补
        assert_eq!(canonical_arguments("search_files", &args(json!({"include": "*.qml"})))["glob"], json!("*.qml"));
        assert_eq!(canonical_arguments("search_files", &args(json!({"include": "src/**/*.h"})))["glob"], json!("src/**/*.h"));
    }
}
