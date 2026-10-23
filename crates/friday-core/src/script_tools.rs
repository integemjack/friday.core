//! 自定义脚本工具的存储（== Swift Agent/ScriptTools 的 ScriptToolStore；Qt agent/ScriptTools）：每个工具是 Tools/<名称>/ 下的一个目录
//!   tool.json      清单（描述、参数 schema、权限、语言、版本…）
//!   main.py|sh|js  实现：从 stdin 读取 JSON 参数，把结果写到 stdout
//!   work/          脚本可写的工作目录
//!   .versions/vN/  历史版本，用于回滚
//!
//! 清单读的时候缺的键用默认值（插件里手写的 tool.json 常常不带 symbol、版本、时间）；写法同 Swift 的 JSONEncoder
//! （prettyPrinted + sortedKeys + withoutEscapingSlashes，日期 .iso8601）。存新版本时旧的归档到 .versions/。
//! 起进程跑脚本（沙盒、超时）、移到废纸篓留在两端；运行结果的摘要、脚本声明的产出文件在这里。

use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::{Map, Value, json};
use std::sync::LazyLock;

use crate::archive::now_iso8601;
use crate::{swift_json, tool_files};

pub const PERMISSIONS: [&str; 4] = ["readOnly", "network", "writeFiles", "execute"];
pub const LANGUAGES: [&str; 3] = ["python", "bash", "node"];

/// 入口文件名
pub fn file_name(language: &str) -> &'static str {
    match language {
        "bash" => "main.sh",
        "node" => "main.js",
        _ => "main.py",
    }
}

pub fn is_valid_name(name: &str) -> bool {
    static PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\A[a-z][a-z0-9_]{2,40}\z").unwrap());
    PATTERN.is_match(name)
}

/// 清单补全默认值；必填的（name、title、description、permission、language、parameters）缺了或不对是 None
pub fn normalize_manifest(object: &Map<String, Value>) -> Option<Map<String, Value>> {
    let text = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_string);
    let (name, title, description) = (text("name")?, text("title")?, text("description")?);
    let permission = text("permission").filter(|value| PERMISSIONS.contains(&value.as_str()))?;
    let language = text("language").filter(|value| LANGUAGES.contains(&value.as_str()))?;
    let parameters = object.get("parameters")?.clone();
    let now = now_iso8601();
    let mut manifest = Map::new();
    manifest.insert("name".into(), json!(name));
    manifest.insert("title".into(), json!(title));
    manifest.insert("description".into(), json!(description));
    manifest.insert("symbol".into(), json!(text("symbol").unwrap_or_else(|| "puzzlepiece.extension".into())));
    manifest.insert("permission".into(), json!(permission));
    manifest.insert("language".into(), json!(language));
    manifest.insert("parameters".into(), parameters);
    // decodeIfPresent：null 与缺失都是 nil
    if let Some(arguments) = object.get("testArguments").filter(|value| !value.is_null()) {
        manifest.insert("testArguments".into(), arguments.clone());
    }
    manifest.insert("version".into(), json!(object.get("version").and_then(Value::as_i64).unwrap_or(1)));
    manifest.insert("enabled".into(), json!(object.get("enabled").and_then(Value::as_bool).unwrap_or(true)));
    manifest.insert("createdBy".into(), json!(text("createdBy").unwrap_or_else(|| "agent".into())));
    manifest.insert("createdAt".into(), json!(text("createdAt").unwrap_or_else(|| now.clone())));
    manifest.insert("updatedAt".into(), json!(text("updatedAt").unwrap_or(now)));
    if let Some(Value::Object(test)) = object.get("lastTest") {
        let mut last = Map::new();
        last.insert("passed".into(), json!(test.get("passed").and_then(Value::as_bool).unwrap_or(false)));
        last.insert("output".into(), json!(test.get("output").and_then(Value::as_str).unwrap_or("")));
        last.insert("date".into(), json!(test.get("date").and_then(Value::as_str).map(str::to_string).unwrap_or_else(now_iso8601)));
        manifest.insert("lastTest".into(), Value::Object(last));
    }
    if let Some(Value::Array(secrets)) = object.get("secrets") {
        manifest.insert("secrets".into(), Value::Array(secrets.iter().filter(|item| item.is_string()).cloned().collect()));
    }
    Some(manifest)
}

fn encoded(manifest: &Map<String, Value>) -> Vec<u8> {
    swift_json::pretty(&Value::Object(manifest.clone())).into_bytes()
}

fn read_manifest(directory: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(directory.join("tool.json")).ok()?;
    match serde_json::from_str::<Value>(&text).ok()? {
        Value::Object(object) => normalize_manifest(&object),
        _ => None,
    }
}

fn language_of(manifest: &Map<String, Value>) -> &str {
    manifest.get("language").and_then(Value::as_str).unwrap_or("python")
}

/// 读一个工具文件夹：{manifest, code, directory}；读不出来是 None
pub fn load(directory: &Path) -> Option<Value> {
    let manifest = read_manifest(directory)?;
    let code = std::fs::read_to_string(directory.join(file_name(language_of(&manifest)))).ok()?;
    Some(json!({ "manifest": manifest, "code": code, "directory": tool_files::slash(directory) }))
}

/// 全部工具（按名字排；以「.」开头的目录跳过）
pub fn load_all(root: &Path) -> Vec<Value> {
    let _ = std::fs::create_dir_all(root);
    let mut tools: Vec<Value> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|entry| load(&entry.path()))
        .collect();
    tools.sort_by(|a, b| a["manifest"]["name"].as_str().cmp(&b["manifest"]["name"].as_str()));
    tools
}

/// 写入新版本：已存在的旧版本先归档到 .versions/v<旧版本号>/（版本号 +1、创建时间沿用）；换了语言时删掉旧的入口文件
pub fn save(root: &Path, manifest: &Map<String, Value>, code: &str) -> Result<Value, String> {
    let mut manifest = normalize_manifest(manifest).ok_or("清单不完整：要有 name、title、description、permission、language、parameters")?;
    let name = manifest["name"].as_str().unwrap_or("").to_string();
    let directory = root.join(&name);
    if let Some(existing) = load(&directory) {
        let old = existing["manifest"].as_object().cloned().unwrap_or_default();
        let version = old.get("version").and_then(Value::as_i64).unwrap_or(1);
        let archive = directory.join(".versions").join(format!("v{version}"));
        let _ = std::fs::remove_dir_all(&archive);
        tool_files::create_directory(&archive)?;
        tool_files::write_in_place(&archive.join("tool.json"), &encoded(&old))?;
        tool_files::write_atomically(&archive.join(file_name(language_of(&old))), existing["code"].as_str().unwrap_or("").as_bytes())?;
        if language_of(&old) != language_of(&manifest) {
            let _ = std::fs::remove_file(directory.join(file_name(language_of(&old))));
        }
        manifest.insert("version".into(), json!(version + 1));
        if let Some(created) = old.get("createdAt") {
            manifest.insert("createdAt".into(), created.clone());
        }
    }
    manifest.insert("updatedAt".into(), json!(now_iso8601()));
    tool_files::create_directory(&directory.join("work"))?;
    let entry = directory.join(file_name(language_of(&manifest)));
    tool_files::write_atomically(&entry, code.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o755)).map_err(|_| {
            format!(
                "你没有将文件“{}”保存到文件夹“{}”中的权限。",
                tool_files::last_component(&entry.to_string_lossy()),
                tool_files::last_component(&directory.to_string_lossy())
            )
        })?;
    }
    tool_files::write_in_place(&directory.join("tool.json"), &encoded(&manifest))?;
    Ok(json!({ "manifest": manifest, "code": code, "directory": tool_files::slash(&directory) }))
}

/// 只更新清单（启用状态、自测结果），不产生新版本
pub fn update_manifest(root: &Path, manifest: &Map<String, Value>) -> Result<(), String> {
    let manifest = normalize_manifest(manifest).ok_or("清单不完整")?;
    let name = manifest["name"].as_str().unwrap_or("");
    tool_files::write_in_place(&root.join(name).join("tool.json"), &encoded(&manifest))
}

/// 历史版本（新的在前）：[{manifest, code}]
pub fn versions(root: &Path, name: &str) -> Vec<Value> {
    let directory = root.join(name).join(".versions");
    let mut result: Vec<Value> = std::fs::read_dir(&directory)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let manifest = read_manifest(&entry.path())?;
            let code = std::fs::read_to_string(entry.path().join(file_name(language_of(&manifest)))).ok()?;
            Some(json!({ "manifest": manifest, "code": code }))
        })
        .collect();
    result.sort_by_key(|item| std::cmp::Reverse(item["manifest"]["version"].as_i64().unwrap_or(0)));
    result
}

/// 交给模型 / 界面看的运行摘要
pub fn run_summary(exit_code: i64, stdout: &str, stderr: &str, timed_out: bool, timeout: f64) -> String {
    if exit_code == 0 && !timed_out {
        return if stdout.is_empty() { "（无输出）".to_string() } else { tool_files::truncate(stdout) };
    }
    let mut text =
        if timed_out { format!("脚本超时（{} 秒）被终止。", timeout as i64) } else { format!("脚本退出码 {exit_code}。") };
    if !stderr.is_empty() {
        text += &format!("\nstderr:\n{}", tool_files::truncate(stderr));
    }
    if !stdout.is_empty() {
        text += &format!("\nstdout:\n{}", tool_files::truncate(stdout));
    }
    text
}

/// 脚本用 JSON 输出里的 media / files / images / image / video / audio / file 键声明产出的文件（绝对路径、~ 开头或相对 work/ 的路径），
/// 只留存在的
pub fn media_paths(stdout: &str, work_directory: &Path, home: &Path) -> Vec<String> {
    let Ok(Value::Object(object)) = serde_json::from_str::<Value>(stdout) else { return Vec::new() };
    let mut paths: Vec<String> = Vec::new();
    for key in ["media", "files", "images", "image", "video", "audio", "file"] {
        match object.get(key) {
            Some(Value::String(one)) => paths.push(one.clone()),
            Some(Value::Array(many)) if many.iter().all(Value::is_string) => {
                paths.extend(many.iter().filter_map(Value::as_str).map(str::to_string))
            }
            _ => {}
        }
    }
    paths
        .into_iter()
        .map(|path| {
            let expanded: PathBuf = if path == "~" {
                home.to_path_buf()
            } else if let Some(rest) = path.strip_prefix("~/") {
                home.join(rest)
            } else {
                PathBuf::from(&path)
            };
            if expanded.is_absolute() { expanded } else { work_directory.join(expanded) }
        })
        .filter(|path| path.exists())
        // Windows 上统一写「/」（同 Qt 的 QDir::fromNativeSeparators）
        .map(|path| tool_files::slash(&path))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(version: Option<i64>) -> Map<String, Value> {
        let mut value = json!({ "name": "echo_tool", "title": "回声", "description": "原样返回", "permission": "readOnly", "language": "python",
            "parameters": { "type": "object", "properties": {} } });
        if let Some(version) = version {
            value["version"] = json!(version);
        }
        value.as_object().unwrap().clone()
    }

    #[test]
    fn manifests_and_names() {
        let normalized = normalize_manifest(&manifest(None)).unwrap();
        assert_eq!(normalized["symbol"], "puzzlepiece.extension");
        assert_eq!(normalized["version"], 1);
        assert_eq!(normalized["enabled"], true);
        assert!(normalized["createdAt"].as_str().unwrap().ends_with('Z'));
        let mut broken = manifest(None);
        broken.insert("language".into(), json!("ruby"));
        assert!(normalize_manifest(&broken).is_none());
        broken = manifest(None);
        broken.remove("parameters");
        assert!(normalize_manifest(&broken).is_none());
        assert!(is_valid_name("abc_1") && !is_valid_name("ab") && !is_valid_name("Abc"));
    }

    #[test]
    fn saves_versions() {
        let root = std::env::temp_dir().join(format!("friday-script-tools-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let first = save(&root, &manifest(None), "print(1)").unwrap();
        assert_eq!(first["manifest"]["version"], 1);
        let created = first["manifest"]["createdAt"].clone();
        let mut bash = manifest(None);
        bash.insert("language".into(), json!("bash"));
        let second = save(&root, &bash, "echo 2").unwrap();
        assert_eq!(second["manifest"]["version"], 2);
        assert_eq!(second["manifest"]["createdAt"], created);
        let directory = root.join("echo_tool");
        assert!(directory.join("main.sh").exists() && !directory.join("main.py").exists() && directory.join("work").is_dir());
        assert!(std::fs::read_to_string(directory.join("tool.json")).unwrap().contains("\"name\" : \"echo_tool\""));
        let history = versions(&root, "echo_tool");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0]["code"], "print(1)");
        assert_eq!(load_all(&root).len(), 1);
        let mut disabled = second["manifest"].as_object().unwrap().clone();
        disabled.insert("enabled".into(), json!(false));
        update_manifest(&root, &disabled).unwrap();
        assert_eq!(load(&directory).unwrap()["manifest"]["enabled"], false);
        let mut gone = manifest(None);
        gone.insert("name".into(), json!("missing_tool"));
        assert_eq!(update_manifest(&root, &gone).unwrap_err(), "文件“tool.json”不存在。");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn run_texts() {
        assert_eq!(run_summary(0, "", "", false, 60.0), "（无输出）");
        assert_eq!(run_summary(1, "o", "e", false, 60.0), "脚本退出码 1。\nstderr:\ne\nstdout:\no");
        assert_eq!(run_summary(0, "", "", true, 60.0), "脚本超时（60 秒）被终止。");
        let work = std::env::temp_dir().join(format!("friday-script-media-{}", std::process::id()));
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("a.png"), b"x").unwrap();
        let paths = media_paths("{\"images\": [\"a.png\", \"b.png\"], \"file\": 3}", &work, Path::new("/nonexistent"));
        assert_eq!(paths, [work.join("a.png").to_string_lossy().into_owned()]);
        assert!(media_paths("不是 JSON", &work, Path::new("/")).is_empty());
        let _ = std::fs::remove_dir_all(&work);
    }
}
