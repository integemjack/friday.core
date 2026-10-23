//! 工具定义（== Swift `Agent/ToolTypes.swift` 的 ToolDescriptor.makeSpec / schema、`AgentTools.all`，Qt `agent/ToolTypes.cpp`、`AgentTools::all`）：
//! OpenAI function 格式的 spec，工具说明和参数说明取提示词里的 tool.<名字>、tool.<名字>.<参数>（数组项、嵌套对象的再往下一层）；
//! 内置工具的名字、标题、图标、权限、参数；调用前查必填参数有没有给。
//! 各工具注册表（脚本工具、MCP、能力、子智能体……）和调用前的其他检查要看两端的状态，留在两端。

use serde_json::{Map, Value, json};

use crate::prompts::{self, Args};

/// 一个参数：名字、类型、说明（提示词里有的用提示词的）、必填
pub struct Param<'a> {
    pub name: &'a str,
    pub kind: &'a str,
    pub description: &'a str,
    pub required: bool,
}

/// 参数的 JSON Schema（type object、必填的按名字排）
pub fn schema(params: &[Param]) -> Value {
    let properties: Map<String, Value> =
        params.iter().map(|param| (param.name.to_string(), json!({ "type": param.kind, "description": param.description }))).collect();
    let mut required: Vec<&str> = params.iter().filter(|param| param.required).map(|param| param.name).collect();
    required.sort_unstable();
    required.dedup();
    json!({ "type": "object", "properties": properties, "required": required })
}

/// 参数说明换成提示词里的（tool.<名字>.<参数>；数组项、嵌套对象的再往下一层）
fn described_schema(schema: &Value, key: &str, args: &Args, override_id: Option<u64>) -> Value {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else { return schema.clone() };
    let mut result = schema.clone();
    let mut described = Map::new();
    for (name, value) in properties {
        if !value.is_object() {
            described.insert(name.clone(), value.clone());
            continue;
        }
        let path = format!("{key}.{name}");
        let mut property = described_schema(value, &path, args, override_id);
        if prompts::contains(&path, override_id) {
            property["description"] = json!(prompts::text(&path, args, override_id));
        }
        if let Some(items) = property.get("items").filter(|items| items.is_object()).cloned() {
            property["items"] = described_schema(&items, &path, args, override_id);
        }
        described.insert(name.clone(), property);
    }
    result["properties"] = Value::Object(described);
    result
}

/// 一个工具的 spec：{type: function, function: {name, description, parameters}}，说明有提示词的用提示词的
pub fn make_spec(name: &str, description: &str, parameters: &Value, args: &Args, override_id: Option<u64>) -> Value {
    let key = format!("tool.{name}");
    let description = if prompts::contains(&key, override_id) { prompts::text(&key, args, override_id) } else { description.to_string() };
    json!({
        "type": "function",
        "function": { "name": name, "description": description, "parameters": described_schema(parameters, &key, args, override_id) },
    })
}

/// 调用前查必填参数：没给、给了 null 或空白文字的（spec 的 required 有一项不是文字就不查）
pub fn missing_required(spec: &Value, arguments: &Map<String, Value>) -> Vec<String> {
    let Some(required) = spec.pointer("/function/parameters/required").and_then(Value::as_array) else { return Vec::new() };
    let Some(names) = required.iter().map(Value::as_str).collect::<Option<Vec<&str>>>() else { return Vec::new() };
    names
        .into_iter()
        .filter(|name| match arguments.get(*name) {
            None | Some(Value::Null) => true,
            Some(Value::String(text)) => text.trim().is_empty(),
            Some(_) => false,
        })
        .map(str::to_string)
        .collect()
}

/// 内置工具
pub struct Builtin {
    pub name: &'static str,
    pub title: &'static str,
    /// 图标（SF Symbols 的名字）
    pub symbol: &'static str,
    /// 图标库里没有 symbol 时用它（Qt 版的图标是一套 SVG，缺几个）
    pub fallback_symbol: Option<&'static str>,
    /// readOnly / network / writeFiles / execute
    pub permission: &'static str,
    pub spec: Value,
}

fn builtin(
    name: &'static str,
    title: &'static str,
    symbol: &'static str,
    permission: &'static str,
    params: &[(&str, &str, bool)],
    args: &Args,
    override_id: Option<u64>,
) -> Builtin {
    let params: Vec<Param> = params.iter().map(|&(name, kind, required)| Param { name, kind, description: "", required }).collect();
    Builtin { name, title, symbol, fallback_symbol: None, permission, spec: make_spec(name, "", &schema(&params), args, override_id) }
}

/// edit_file：一处用 old_text / new_text，几处用 edits 数组（参数里有对象数组，schema 手写；必填只有 path）
fn edit_file(args: &Args, override_id: Option<u64>) -> Builtin {
    let property = |kind: &str| json!({ "type": kind, "description": "" });
    let parameters = json!({
        "type": "object",
        "properties": {
            "path": property("string"),
            "old_text": property("string"),
            "new_text": property("string"),
            "replace_all": property("boolean"),
            "edits": {
                "type": "array",
                "description": "",
                "items": {
                    "type": "object",
                    "properties": { "old_text": property("string"), "new_text": property("string"), "replace_all": property("boolean") },
                    "required": ["new_text", "old_text"],
                },
            },
        },
        "required": ["path"],
    });
    Builtin {
        name: "edit_file",
        title: "修改文件",
        symbol: "pencil",
        fallback_symbol: None,
        permission: "writeFiles",
        spec: make_spec("edit_file", "", &parameters, args, override_id),
    }
}

/// 内置工具的清单（顺序就是给模型的顺序）；args：工具说明里的参数（两端都不传，平台参数在提示词里）
pub fn builtins(args: &Args, override_id: Option<u64>) -> Vec<Builtin> {
    let make = |name, title, symbol, permission, params: &[(&str, &str, bool)]| {
        builtin(name, title, symbol, permission, params, args, override_id)
    };
    let with_fallback = |mut tool: Builtin, fallback: &'static str| {
        tool.fallback_symbol = Some(fallback);
        tool
    };
    vec![
        make("get_current_time", "获取时间", "clock", "readOnly", &[]),
        make("list_directory", "列出目录", "folder", "readOnly", &[("path", "string", true)]),
        make(
            "read_file",
            "读取文件",
            "doc.text",
            "readOnly",
            &[("path", "string", true), ("offset", "integer", false), ("limit", "integer", false)],
        ),
        make("find_files", "查找文件", "doc.text.magnifyingglass", "readOnly", &[("pattern", "string", true), ("path", "string", false)]),
        make(
            "search_files",
            "搜索文件内容",
            "text.magnifyingglass",
            "readOnly",
            &[("pattern", "string", true), ("path", "string", false), ("glob", "string", false), ("ignore_case", "boolean", false)],
        ),
        // v2.8 按关键词搜：search_files 是正则，按字面找要转义、几个词只能写成一行里的 a.*b；这里按字面、按文件找、排好序
        make(
            "search_keywords",
            "按关键词搜索",
            "text.magnifyingglass",
            "readOnly",
            &[
                ("keywords", "string", true),
                ("path", "string", false),
                ("glob", "string", false),
                ("match_any", "boolean", false),
                ("match_case", "boolean", false),
            ],
        ),
        make("write_file", "写入文件", "square.and.pencil", "writeFiles", &[("path", "string", true), ("content", "string", true)]),
        edit_file(args, override_id),
        make("run_shell", "执行命令", "terminal", "execute", &[("command", "string", true), ("timeout_seconds", "integer", false)]),
        make(
            "web_search",
            "搜索网页",
            "magnifyingglass",
            "network",
            &[("query", "string", true), ("time_range", "string", false), ("language", "string", false), ("count", "integer", false)],
        ),
        make("fetch_url", "抓取网页", "globe", "network", &[("url", "string", true), ("offset", "integer", false)]),
        // v2.2 内置终端：用户界面上这段对话的终端卡片
        make(
            "read_terminal",
            "读取终端",
            "terminal",
            "readOnly",
            &[
                ("terminal", "string", false),
                ("lines", "integer", false),
                ("wait_for", "string", false),
                ("wait_seconds", "integer", false),
            ],
        ),
        make("run_in_terminal", "在终端里运行", "terminal", "execute", &[("command", "string", true)]),
        // v2.49 关掉 run_in_terminal 开的终端（只关智能体自己开的，停的也是它自己起的命令）
        make("close_terminal", "关闭终端", "terminal", "readOnly", &[("terminal", "string", true)]),
        // v2.12 内置浏览器：用户界面上这段对话的浏览器卡片
        make(
            "browser_navigate",
            "打开网页",
            "globe",
            "network",
            &[
                ("url", "string", false),
                ("new_tab", "boolean", false),
                ("keep", "boolean", false),
                ("action", "string", false),
                ("tab", "string", false),
            ],
        ),
        make("browser_read", "读网页", "globe", "readOnly", &[("mode", "string", false), ("tab", "string", false)]),
        with_fallback(
            make(
                "browser_act",
                "操作网页",
                "cursorarrow.click",
                "network",
                &[
                    ("action", "string", true),
                    ("ref", "string", false),
                    ("x", "number", false),
                    ("y", "number", false),
                    ("text", "string", false),
                    ("submit", "boolean", false),
                    ("append", "boolean", false),
                    ("key", "string", false),
                    ("direction", "string", false),
                    ("amount", "integer", false),
                    ("accept", "boolean", false),
                    ("tab", "string", false),
                ],
            ),
            "globe",
        ),
        with_fallback(make("browser_screenshot", "网页截图", "camera.viewfinder", "readOnly", &[("tab", "string", false)]), "photo"),
        make(
            "browser_resize",
            "调整网页视口",
            "arrow.up.left.and.arrow.down.right",
            "network",
            &[("width", "integer", false), ("height", "integer", false), ("fit", "boolean", false), ("tab", "string", false)],
        ),
        make("browser_tabs", "浏览器标签", "globe", "network", &[("action", "string", false), ("tab", "string", false)]),
        with_fallback(
            make(
                "browser_hand_over",
                "请你在浏览器里操作",
                "hand.raised",
                "readOnly",
                &[("reason", "string", true), ("tab", "string", false)],
            ),
            "hand.raised.fill",
        ),
        make("browser_run_js", "在网页里运行脚本", "curlybraces", "execute", &[("script", "string", true), ("tab", "string", false)]),
    ]
}

impl Builtin {
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "title": self.title,
            "symbol": self.symbol,
            "fallbackSymbol": self.fallback_symbol,
            "permission": self.permission,
            "spec": self.spec,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs() {
        let _lock = prompts::TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let text = "@@ tool.demo\n演示 {{x}}\n\n@@ tool.demo.path\n路径\n\n@@ tool.demo.items\n几项\n\n@@ tool.demo.items.name\n名字\n";
        prompts::setup(std::iter::once(("prompts-tools.md".to_string(), text.to_string())).collect(), None, Vec::new());
        let parameters = json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "" },
                "other": { "type": "string", "description": "原来的" },
                "items": { "type": "array", "items": { "type": "object", "properties": { "name": { "type": "string" } } } },
            },
        });
        let args = Args::from([("x".to_string(), "一下".to_string())]);
        let spec = make_spec("demo", "没用上", &parameters, &args, None);
        assert_eq!(spec["function"]["description"], "演示 一下");
        assert_eq!(spec["function"]["parameters"]["properties"]["path"]["description"], "路径");
        assert_eq!(spec["function"]["parameters"]["properties"]["other"]["description"], "原来的");
        assert_eq!(spec["function"]["parameters"]["properties"]["items"]["description"], "几项");
        assert_eq!(spec["function"]["parameters"]["properties"]["items"]["items"]["properties"]["name"]["description"], "名字");
        assert_eq!(make_spec("none", "自带的", &json!({}), &Args::new(), None)["function"]["description"], "自带的");
        let tools = builtins(&Args::new(), None);
        assert_eq!(tools.len(), 22);
        assert_eq!(tools[7].name, "edit_file");
        assert_eq!(tools[2].spec["function"]["parameters"]["required"], json!(["path"]));
        assert_eq!(tools.iter().filter(|tool| tool.fallback_symbol.is_some()).count(), 3);
    }

    #[test]
    fn required() {
        let spec = json!({ "function": { "parameters": { "required": ["path", "content"] } } });
        let args = |value: Value| value.as_object().unwrap().clone();
        assert_eq!(missing_required(&spec, &args(json!({ "path": "a", "content": "x" }))), Vec::<String>::new());
        assert_eq!(missing_required(&spec, &args(json!({ "path": "  ", "content": null }))), ["path", "content"]);
        assert_eq!(missing_required(&spec, &args(json!({ "path": 1, "content": false }))), Vec::<String>::new());
        assert_eq!(missing_required(&json!({ "function": { "parameters": { "required": ["a", 1] } } }), &Map::new()), Vec::<String>::new());
        assert_eq!(
            schema(&[
                Param { name: "b", kind: "string", description: "", required: true },
                Param { name: "a", kind: "integer", description: "d", required: true }
            ])["required"],
            json!(["a", "b"])
        );
    }
}
