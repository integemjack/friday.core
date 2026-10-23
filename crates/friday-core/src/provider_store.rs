//! 脚本提供方的包（== Swift Capabilities/ScriptProviders 的 ProviderManifest / ProviderPackageStore；Qt capabilities/ScriptProviders）：
//! Providers/<id>/ 下的一个包
//!   provider.json  清单：提供哪些能力（内置能力的 id，或带新能力的定义）、设置项、权限、语言、版本…
//!   main.py|sh|js  实现（协议见两端的 ScriptProviderBackend）
//!   work/          脚本的工作目录
//!   .versions/vN/  历史版本
//!
//! 清单读的时候补默认值（createdBy 默认 user、权限默认 execute、语言认 python3 / sh / js 这类写法），说不通的给一句说明；
//! 存新版本时旧的归档到 .versions/；导入时找 provider.json（顶层或下一层）、换掉包里的文件（留着历史版本和工作目录）。
//! 解 zip、打 zip、移到废纸篓、能力定义和设置项的细节留在两端。

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::archive::now_iso8601;
use crate::script_tools::file_name;
use crate::{swift_json, tool_files};

pub fn is_valid_id(id: &str) -> bool {
    static PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\A[a-z][a-z0-9_-]{1,40}\z").unwrap());
    PATTERN.is_match(id)
}

/// 语言的宽松认法（python3 / py、sh / shell / zsh、javascript / js / nodejs）
pub fn loose_language(value: &str) -> Option<&'static str> {
    match value.to_lowercase().as_str() {
        "python" | "python3" | "py" => Some("python"),
        "bash" | "sh" | "shell" | "zsh" => Some("bash"),
        "node" | "javascript" | "js" | "nodejs" => Some("node"),
        _ => None,
    }
}

/// 清单补默认值；说不通时是给人看的说明
pub fn normalize(object: &Map<String, Value>) -> Result<Map<String, Value>, String> {
    let text = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_string);
    let id = text("id").ok_or("缺少 id")?;
    let language = loose_language(&text("language").unwrap_or_else(|| "python".into())).ok_or("language 只能是 python | bash | node")?;
    let Some(Value::Array(capabilities)) = object.get("capabilities") else {
        return Err("缺少 capabilities（能力 id 的数组）".into());
    };
    let mut definitions = Vec::new();
    for definition in object.get("definitions").and_then(Value::as_array).into_iter().flatten() {
        if definition.get("id").and_then(Value::as_str).is_none() {
            return Err("definitions 里的一项缺少 id".into());
        }
        definitions.push(definition.clone());
    }
    let now = now_iso8601();
    let mut manifest = Map::new();
    manifest.insert("id".into(), json!(id));
    manifest.insert("title".into(), json!(text("title").unwrap_or_else(|| id.clone())));
    manifest.insert("detail".into(), json!(text("detail").unwrap_or_default()));
    manifest.insert("symbol".into(), json!(text("symbol").unwrap_or_else(|| "puzzlepiece.extension".into())));
    manifest.insert("language".into(), json!(language));
    let permission =
        text("permission").filter(|value| crate::script_tools::PERMISSIONS.contains(&value.as_str())).unwrap_or_else(|| "execute".into());
    manifest.insert("permission".into(), json!(permission));
    manifest.insert("online".into(), json!(object.get("online").and_then(Value::as_bool).unwrap_or(false)));
    manifest.insert("capabilities".into(), Value::Array(capabilities.iter().filter(|item| item.is_string()).cloned().collect()));
    manifest.insert("definitions".into(), Value::Array(definitions));
    manifest.insert("options".into(), Value::Array(object.get("options").and_then(Value::as_array).cloned().unwrap_or_default()));
    manifest.insert(
        "secrets".into(),
        Value::Array(
            object.get("secrets").and_then(Value::as_array).into_iter().flatten().filter(|item| item.is_string()).cloned().collect(),
        ),
    );
    manifest.insert("check".into(), json!(object.get("check").and_then(Value::as_bool).unwrap_or(false)));
    if let Some(timeout) = object.get("timeout").and_then(Value::as_i64) {
        manifest.insert("timeout".into(), json!(timeout));
    }
    if let Some(Value::Object(test)) = object.get("test") {
        let capability = test.get("capability").and_then(Value::as_str).ok_or("test 缺少 capability")?;
        manifest
            .insert("test".into(), json!({ "capability": capability, "arguments": test.get("arguments").cloned().unwrap_or(Value::Null) }));
    }
    manifest.insert("version".into(), json!(object.get("version").and_then(Value::as_i64).unwrap_or(1)));
    manifest.insert("createdBy".into(), json!(text("createdBy").unwrap_or_else(|| "user".into())));
    if let Some(source) = text("source") {
        manifest.insert("source".into(), json!(source));
    }
    manifest.insert("createdAt".into(), json!(text("createdAt").unwrap_or_else(|| now.clone())));
    manifest.insert("updatedAt".into(), json!(text("updatedAt").unwrap_or(now)));
    if let Some(Value::Object(last)) = object.get("lastTest") {
        manifest.insert(
            "lastTest".into(),
            json!({ "passed": last.get("passed").and_then(Value::as_bool).unwrap_or(false), "output": last.get("output").and_then(Value::as_str).unwrap_or(""),
                "date": last.get("date").and_then(Value::as_str).map(str::to_string).unwrap_or_else(now_iso8601) }),
        );
    }
    Ok(manifest)
}

fn valid_capability_id(id: &str) -> bool {
    static PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\A[a-z][A-Za-z0-9_]{1,40}\z").unwrap());
    PATTERN.is_match(id)
}

fn valid_tool(name: &str) -> bool {
    static PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\A[a-z][a-z0-9_]{2,40}\z").unwrap());
    PATTERN.is_match(name)
}

/// 清单本身说不通的地方（id、能力、定义）；None 表示可以用。builtins：内置能力的 id；catalog：能力目录里已有的
pub fn problem(manifest: &Map<String, Value>, builtins: &[String], catalog: &[String]) -> Option<String> {
    let id = manifest.get("id").and_then(Value::as_str).unwrap_or("");
    if !is_valid_id(id) {
        return Some(format!("id {id} 不合法：小写字母开头，只含小写字母、数字、下划线、连字符，2–41 个字符"));
    }
    let capabilities: Vec<&str> =
        manifest.get("capabilities").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    if capabilities.is_empty() {
        return Some("capabilities 不能为空：写内置能力的 id，或 definitions 里新能力的 id".into());
    }
    let mut defined: Vec<&str> = Vec::new();
    for definition in manifest.get("definitions").and_then(Value::as_array).into_iter().flatten() {
        let definition_id = definition.get("id").and_then(Value::as_str).unwrap_or("");
        let tool = definition.get("tool").and_then(Value::as_str).unwrap_or("");
        if !valid_capability_id(definition_id) {
            return Some(format!("新能力的 id {definition_id} 不合法：小写字母开头，只含字母、数字、下划线"));
        }
        if !valid_tool(tool) {
            return Some(format!("新能力 {definition_id} 的工具名 {tool} 不合法：小写字母开头，只含小写字母、数字、下划线"));
        }
        if builtins.iter().any(|builtin| builtin == definition_id) {
            return Some(format!("{definition_id} 是内置能力，直接写进 capabilities 就行，不要重新定义"));
        }
        defined.push(definition_id);
    }
    let mut sorted: Vec<&String> = builtins.iter().collect();
    sorted.sort();
    let known = |capability: &str| {
        builtins.iter().any(|item| item == capability) || defined.contains(&capability) || catalog.iter().any(|item| item == capability)
    };
    capabilities.into_iter().find(|capability| !known(capability)).map(|unknown| {
        format!(
            "能力 {unknown} 既不是内置能力（{}），也没有在 definitions 里定义",
            sorted.iter().map(|item| item.as_str()).collect::<Vec<_>>().join("、")
        )
    })
}

fn encoded(manifest: &Map<String, Value>) -> Vec<u8> {
    swift_json::pretty(&Value::Object(manifest.clone())).into_bytes()
}

fn language_of(manifest: &Map<String, Value>) -> &str {
    manifest.get("language").and_then(Value::as_str).unwrap_or("python")
}

/// 读一个包：{manifest, code, directory}，读不出来是说明
pub fn read(directory: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(directory.join("provider.json")).map_err(|_| "没有 provider.json".to_string())?;
    let object = match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(object)) => object,
        Ok(_) => return Err("provider.json 读不出来：不是 JSON 对象".into()),
        Err(error) => return Err(format!("provider.json 读不出来：{error}")),
    };
    let manifest = normalize(&object).map_err(|problem| format!("provider.json 读不出来：{problem}"))?;
    let entry = file_name(language_of(&manifest));
    let code = std::fs::read_to_string(directory.join(entry)).map_err(|_| format!("缺少入口文件 {entry}"))?;
    Ok(json!({ "manifest": manifest, "code": code, "directory": tool_files::slash(directory) }))
}

/// 全部包（按名字排）和读不出来的原因（「Providers/<名字>：…」）
pub fn load_all(root: &Path) -> Value {
    let _ = std::fs::create_dir_all(root);
    let mut names: Vec<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    let (mut packages, mut problems) = (Vec::new(), Vec::new());
    for name in names {
        match read(&root.join(&name)) {
            Ok(package) => packages.push(package),
            Err(problem) => problems.push(format!("Providers/{name}：{problem}")),
        }
    }
    json!({ "packages": packages, "problems": problems })
}

fn archive(package: &Value) -> Result<(), String> {
    let manifest = package["manifest"].as_object().cloned().unwrap_or_default();
    let directory = PathBuf::from(package["directory"].as_str().unwrap_or(""));
    let version = manifest.get("version").and_then(Value::as_i64).unwrap_or(1);
    let target = directory.join(".versions").join(format!("v{version}"));
    let _ = std::fs::remove_dir_all(&target);
    tool_files::create_directory(&target)?;
    tool_files::write_in_place(&target.join("provider.json"), &encoded(&manifest))?;
    tool_files::write_atomically(&target.join(file_name(language_of(&manifest))), package["code"].as_str().unwrap_or("").as_bytes())
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// 写入新版本：已存在的旧版本先归档（版本号 +1、创建时间沿用）
pub fn save(root: &Path, manifest: &Map<String, Value>, code: &str) -> Result<Value, String> {
    let mut manifest = normalize(manifest)?;
    let directory = root.join(manifest["id"].as_str().unwrap_or(""));
    if let Ok(existing) = read(&directory) {
        archive(&existing)?;
        let old = existing["manifest"].as_object().cloned().unwrap_or_default();
        if language_of(&old) != language_of(&manifest) {
            let _ = std::fs::remove_file(directory.join(file_name(language_of(&old))));
        }
        manifest.insert("version".into(), json!(old.get("version").and_then(Value::as_i64).unwrap_or(1) + 1));
        if let Some(created) = old.get("createdAt") {
            manifest.insert("createdAt".into(), created.clone());
        }
    }
    manifest.insert("updatedAt".into(), json!(now_iso8601()));
    tool_files::create_directory(&directory.join("work"))?;
    let entry = directory.join(file_name(language_of(&manifest)));
    tool_files::write_atomically(&entry, code.as_bytes())?;
    make_executable(&entry);
    tool_files::write_in_place(&directory.join("provider.json"), &encoded(&manifest))?;
    Ok(json!({ "manifest": manifest, "code": code, "directory": tool_files::slash(&directory) }))
}

/// 只更新清单（自测结果），不产生新版本
pub fn update_manifest(root: &Path, manifest: &Map<String, Value>) -> Result<(), String> {
    let manifest = normalize(manifest)?;
    tool_files::write_in_place(&root.join(manifest["id"].as_str().unwrap_or("")).join("provider.json"), &encoded(&manifest))
}

/// 解开的包里 provider.json 在哪一层：顶层，或者唯一的一个子目录里（.、__MACOSX 开头的不算）
pub fn locate(folder: &Path, source_name: &str) -> Result<PathBuf, String> {
    if folder.join("provider.json").exists() {
        return Ok(folder.to_path_buf());
    }
    let children: Vec<PathBuf> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            !name.starts_with('.') && !name.starts_with("__MACOSX")
        })
        .map(|entry| entry.path())
        .collect();
    match children.as_slice() {
        [only] if only.join("provider.json").exists() => Ok(only.clone()),
        _ => Err(format!("{source_name} 里没有 provider.json")),
    }
}

fn skipped(name: &str) -> bool {
    name == ".versions" || name == "work" || name == "__MACOSX" || name.starts_with('.')
}

fn copy_recursively(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_recursively(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

/// 导入解开的包（folder 是 locate 找到的那层）：清单说不通就不导；同 id 的已存在时当作更新（旧版本归档、版本号 +1）。
/// 换成包里的文件，保留历史版本和工作目录。返回 {package, replaced}
pub fn install(root: &Path, folder: &Path, source_name: &str, builtins: &[String], catalog: &[String]) -> Result<Value, String> {
    let package = read(folder)?;
    let mut manifest = package["manifest"].as_object().cloned().unwrap_or_default();
    if let Some(problem) = problem(&manifest, builtins, catalog) {
        return Err(problem);
    }
    let directory = root.join(manifest["id"].as_str().unwrap_or(""));
    let existing = read(&directory).ok();
    if let Some(existing) = &existing {
        archive(existing)?;
    }
    tool_files::create_directory(&directory)?;
    for entry in std::fs::read_dir(&directory).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".versions" || name == "work" {
            continue;
        }
        let path = entry.path();
        let _ = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
    }
    for entry in std::fs::read_dir(folder).map_err(|error| error.to_string())?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if skipped(&name) {
            continue;
        }
        copy_recursively(&entry.path(), &directory.join(&name)).map_err(|error| format!("未能复制“{name}”：{error}"))?;
    }
    tool_files::create_directory(&directory.join("work"))?;
    make_executable(&directory.join(file_name(language_of(&manifest))));
    let previous = existing.as_ref().and_then(|existing| existing["manifest"].as_object().cloned());
    manifest.insert("createdBy".into(), json!("user"));
    manifest.insert("source".into(), json!(source_name));
    manifest.insert("version".into(), json!(previous.as_ref().and_then(|old| old.get("version")).and_then(Value::as_i64).unwrap_or(0) + 1));
    manifest.insert(
        "createdAt".into(),
        previous.as_ref().and_then(|old| old.get("createdAt")).cloned().unwrap_or_else(|| json!(now_iso8601())),
    );
    manifest.insert("updatedAt".into(), json!(now_iso8601()));
    manifest.remove("lastTest");
    tool_files::write_in_place(&directory.join("provider.json"), &encoded(&manifest))?;
    Ok(
        json!({ "package": { "manifest": manifest, "code": package["code"], "directory": tool_files::slash(&directory) }, "replaced": existing.is_some() }),
    )
}

/// 导出用的清单（自测结果不带出去）
pub fn export_manifest(manifest: &Map<String, Value>) -> String {
    let mut manifest = manifest.clone();
    manifest.remove("lastTest");
    swift_json::pretty(&Value::Object(manifest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Map<String, Value> {
        json!({ "id": "my-tts", "language": "python3", "capabilities": ["speech"] }).as_object().unwrap().clone()
    }

    #[test]
    fn normalizes_and_checks() {
        let normalized = normalize(&manifest()).unwrap();
        assert_eq!(
            (normalized["language"].as_str(), normalized["permission"].as_str(), normalized["createdBy"].as_str()),
            (Some("python"), Some("execute"), Some("user"))
        );
        assert_eq!(normalized["title"], "my-tts");
        let mut bad = manifest();
        bad.insert("language".into(), json!("ruby"));
        assert_eq!(normalize(&bad).unwrap_err(), "language 只能是 python | bash | node");
        bad = manifest();
        bad.remove("capabilities");
        assert_eq!(normalize(&bad).unwrap_err(), "缺少 capabilities（能力 id 的数组）");
        bad = manifest();
        bad.insert("test".into(), json!({}));
        assert_eq!(normalize(&bad).unwrap_err(), "test 缺少 capability");
        let builtins = vec!["image".to_string(), "speech".to_string()];
        assert!(problem(&normalized, &builtins, &[]).is_none());
        let mut unknown = normalized.clone();
        unknown.insert("capabilities".into(), json!(["music"]));
        assert_eq!(problem(&unknown, &builtins, &[]).unwrap(), "能力 music 既不是内置能力（image、speech），也没有在 definitions 里定义");
        assert!(problem(&unknown, &builtins, &["music".to_string()]).is_none());
        unknown.insert("definitions".into(), json!([{ "id": "music", "tool": "make_music" }]));
        assert!(problem(&unknown, &builtins, &[]).is_none());
        unknown.insert("definitions".into(), json!([{ "id": "speech", "tool": "say_it" }]));
        assert_eq!(problem(&unknown, &builtins, &[]).unwrap(), "speech 是内置能力，直接写进 capabilities 就行，不要重新定义");
        let mut id = normalized.clone();
        id.insert("id".into(), json!("X"));
        assert!(problem(&id, &builtins, &[]).unwrap().starts_with("id X 不合法"));
    }

    #[test]
    fn store_and_import() {
        let base = std::env::temp_dir().join(format!("friday-providers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("Providers");
        let saved = save(&root, &manifest(), "print(1)").unwrap();
        assert_eq!(saved["manifest"]["version"], 1);
        assert_eq!(save(&root, &manifest(), "print(2)").unwrap()["manifest"]["version"], 2);
        assert!(root.join("my-tts/.versions/v1/main.py").exists());
        std::fs::create_dir_all(root.join("broken")).unwrap();
        let all = load_all(&root);
        assert_eq!(all["packages"].as_array().unwrap().len(), 1);
        assert_eq!(all["problems"], json!(["Providers/broken：没有 provider.json"]));
        // 导入：provider.json 在下一层
        let unpacked = base.join("unpacked");
        std::fs::create_dir_all(unpacked.join("pkg/assets")).unwrap();
        std::fs::create_dir_all(unpacked.join("__MACOSX")).unwrap();
        std::fs::write(
            unpacked.join("pkg/provider.json"),
            r#"{"id":"my-tts","language":"bash","capabilities":["speech"],"lastTest":{"passed":true}}"#,
        )
        .unwrap();
        std::fs::write(unpacked.join("pkg/main.sh"), "echo hi").unwrap();
        std::fs::write(unpacked.join("pkg/assets/a.txt"), "a").unwrap();
        let folder = locate(&unpacked, "x.fridayprovider").unwrap();
        assert_eq!(folder, unpacked.join("pkg"));
        assert_eq!(locate(&base.join("Providers/broken"), "y.zip").unwrap_err(), "y.zip 里没有 provider.json");
        let installed = install(&root, &folder, "x.fridayprovider", &["speech".to_string()], &[]).unwrap();
        assert_eq!(installed["replaced"], true);
        assert_eq!(installed["package"]["manifest"]["version"], 3);
        assert_eq!(installed["package"]["manifest"]["source"], "x.fridayprovider");
        assert!(installed["package"]["manifest"].get("lastTest").is_none());
        let dir = root.join("my-tts");
        assert!(
            dir.join("main.sh").exists()
                && !dir.join("main.py").exists()
                && dir.join("assets/a.txt").exists()
                && dir.join(".versions/v2").exists()
        );
        assert!(!export_manifest(installed["package"]["manifest"].as_object().unwrap()).contains("lastTest"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
