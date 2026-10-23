//! 内置的文件工具（== Swift `Agent/AgentTools.swift` 的 list_directory / read_file / write_file / edit_file、Qt `agent/AgentTools.cpp`
//! 和 `ToolFiles`）：在核心里直接读写文件，结果和报错文案照 Swift 版 Foundation 报的（NSCocoaErrorDomain 的 zh-Hans 说明，Qt 版逐字复刻过）。
//! 路径由两端解析好（~、相对工作目录）再传进来；读媒体文件（交给界面当附件）、会话改动的原文、改过的文件登记留在两端。
//! 还有两端都用的输出整理：太长的截断（留头尾）、分页、去掉 HTML 标签。

use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use unicode_segmentation::UnicodeSegmentation;

use crate::file_edit::{self, Edit};
use crate::file_search;
use crate::line_diff;

/// 工具输出最多多少字（按用户看到的字算）
pub const MAX_OUTPUT: usize = 8_000;

/// 工具失败：Invalid 是参数不对（== AgentToolError.invalid），Failed 是执行出错（== AgentToolError.failed / FridayError）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolError {
    Invalid(String),
    Failed(String),
}

impl ToolError {
    pub fn message(&self) -> &str {
        match self {
            ToolError::Invalid(message) | ToolError::Failed(message) => message,
        }
    }
}

/// 一次工具调用的结果
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Output {
    pub text: String,
    /// 改文件的增删行数（写了文件、而且原来是文字的才有）
    pub line_diff: Option<line_diff::Counts>,
}

impl Output {
    fn text(text: String) -> Self {
        Output { text, line_diff: None }
    }
}

fn graphemes(text: &str) -> Vec<&str> {
    text.graphemes(true).collect()
}

/// 太长的输出：留开头和结尾（报错、退出码、测试结果常在结尾），中间写明省略了多少
pub fn truncate(text: &str) -> String {
    // 字节数不超过上限时字数一定也不超过
    if text.len() <= MAX_OUTPUT {
        return text.to_string();
    }
    let parts = graphemes(text);
    if parts.len() <= MAX_OUTPUT {
        return text.to_string();
    }
    let head = MAX_OUTPUT * 3 / 4;
    let tail = MAX_OUTPUT - head;
    format!(
        "{}\n…（已截断：共 {} 字符，中间省略 {} 字符；要看中间就分段读或缩小输出范围）…\n{}",
        parts[..head].concat(),
        parts.len(),
        parts.len() - head - tail,
        parts[parts.len() - tail..].concat()
    )
}

/// fetch_url 分页：从第 offset 个字（0 起）起取 MAX_OUTPUT 字，还有就写明共多少字、接着从哪读；不给 offset 且不超过 MAX_OUTPUT 字时原样返回
pub fn page(text: &str, offset: Option<i64>) -> String {
    let parts = graphemes(text);
    let count = parts.len();
    if offset.is_none() && count <= MAX_OUTPUT {
        return text.to_string();
    }
    let requested = offset.unwrap_or(0).max(0) as usize;
    if requested >= count {
        return format!("（正文共 {count} 字，第 {requested} 字起没有内容）");
    }
    let end = count.min(requested + MAX_OUTPUT);
    let mut result = parts[requested..end].concat();
    if end < count {
        result += &format!("\n…（正文共 {count} 字，这里是第 {}–{end} 字；接着读用 offset={end}）", requested + 1);
    } else if requested > 0 {
        result += &format!("\n（正文共 {count} 字，已读到末尾）");
    }
    result
}

/// 网页去掉脚本、样式、注释和标签，留下文字
pub fn strip_html(html: &str) -> String {
    static TAGS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        [r"(?is)<script.*?</script>", r"(?is)<style.*?</style>", r"(?is)<!--.*?-->", r"<[^>]+>"]
            .iter()
            .map(|pattern| Regex::new(pattern).unwrap())
            .collect()
    });
    static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]+").unwrap());
    static NEWLINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\n\s*").unwrap());
    let mut text = html.to_string();
    for pattern in TAGS.iter() {
        text = pattern.replace_all(&text, " ").into_owned();
    }
    // &amp; 放最后，免得「&amp;lt;」被解两次
    for (entity, value) in [("&nbsp;", " "), ("&lt;", "<"), ("&gt;", ">"), ("&quot;", "\""), ("&#39;", "'"), ("&amp;", "&")] {
        text = text.replace(entity, value);
    }
    text = SPACES.replace_all(&text, " ").into_owned();
    text = NEWLINES.replace_all(&text, "\n").into_owned();
    text.trim().to_string()
}

// MARK: - 路径

/// 路径的最后一段（== URL.lastPathComponent；根目录是「/」）
pub fn last_component(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return if path.is_empty() { String::new() } else { "/".into() };
    }
    Path::new(trimmed).file_name().map_or_else(|| trimmed.to_string(), |name| name.to_string_lossy().into_owned())
}

/// 按字面整理路径（== URL.standardizedFileURL：去掉「.」、按「..」往上，不解析符号链接）
pub fn standardized(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn parent_of(path: &Path) -> PathBuf {
    path.parent().map_or_else(|| path.to_path_buf(), Path::to_path_buf)
}

fn display(path: &Path) -> String {
    slash(path)
}

/// 交给两端的路径：Windows 上也用「/」（== Qt 的 QDir / cleanPath；Path::join 在 Windows 上拼的是「\\」）
pub fn slash(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) { text.replace('\\', "/") } else { text.into_owned() }
}

/// 给人看的完整路径：Windows 上用「\\」（== QDir::toNativeSeparators）
fn native(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) { text.replace('/', "\\") } else { text.into_owned() }
}

fn is_permission(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
}

/// 路径本身拿不到（stat 失败）是因为上级目录没有权限，而不是不存在
fn stat_denied(path: &Path) -> bool {
    fs::metadata(path).err().is_some_and(|error| is_permission(&error))
}

fn readable(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(info) if info.is_dir() => fs::read_dir(path).is_ok(),
        Ok(_) => fs::File::open(path).is_ok(),
        Err(_) => false,
    }
}

/// 读文件 / 列目录失败的说明：「未能打开文件“x”，因为它不存在。」「未能打开文件“x”，因为你没有查看它的权限。」「未能打开文件“x”。」
pub fn read_failure(path: &str) -> String {
    let target = Path::new(path);
    let name = last_component(path);
    let exists = target.exists();
    if (exists && !readable(target)) || (!exists && stat_denied(target)) {
        return format!("未能打开文件“{name}”，因为你没有查看它的权限。");
    }
    if !exists {
        return format!("未能打开文件“{name}”，因为它不存在。");
    }
    format!("未能打开文件“{name}”。")
}

/// 内容不是合法 UTF-8：「未能打开该文件，因为它的格式不正确。」
pub const INVALID_TEXT_ENCODING: &str = "未能打开该文件，因为它的格式不正确。";

/// 按 Finder 的顺序比名字（== localizedStandardCompare：不分大小写，数字按大小）
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn chunks(text: &str) -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = Vec::new();
        for c in text.chars() {
            let digit = c.is_ascii_digit();
            match out.last_mut() {
                Some((kind, chunk)) if *kind == digit => chunk.push(c),
                _ => out.push((digit, c.to_string())),
            }
        }
        out
    }
    let (x, y) = (chunks(a), chunks(b));
    for (left, right) in x.iter().zip(y.iter()) {
        let order = match (left, right) {
            ((true, l), (true, r)) => {
                let (l, r) = (l.trim_start_matches('0'), r.trim_start_matches('0'));
                l.len().cmp(&r.len()).then_with(|| l.cmp(r))
            }
            ((_, l), (_, r)) => l.to_lowercase().cmp(&r.to_lowercase()),
        };
        if order != std::cmp::Ordering::Equal {
            return order;
        }
    }
    x.len().cmp(&y.len()).then_with(|| a.cmp(b))
}

fn visible_entries(directory: &Path) -> Vec<(String, bool, bool)> {
    let Ok(entries) = fs::read_dir(directory) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            let kind = entry.file_type().ok();
            let link = kind.is_some_and(|kind| kind.is_symlink());
            let directory = if link { entry.path().is_dir() } else { kind.is_some_and(|kind| kind.is_dir()) };
            Some((name, directory, link))
        })
        .collect()
}

/// 路径不对时接在报错后面的提示：不存在 → 最近一层在的上级目录里有什么、工作目录下同名的在哪；要文件却是目录 → 用 list_directory；
/// 要目录却是文件 → 用 read_file。没什么可提示的为空；有的话以换行开头
pub fn path_hint(path: &str, working_directory: &str, want_directory: bool) -> String {
    let target = Path::new(path);
    let exists = target.exists();
    // 没有权限（本身不可读、上级目录进不去）：路径是对的，不提示
    if (exists && !readable(target)) || (!exists && stat_denied(target)) {
        return String::new();
    }
    if exists {
        let directory = target.is_dir();
        if directory && !want_directory {
            return "\n（这是一个目录：用 list_directory 看里面有什么）".into();
        }
        if !directory && want_directory {
            return "\n（这是一个文件：用 read_file 读）".into();
        }
        return String::new();
    }
    let mut hints = Vec::new();
    // 最近一层在的上级目录里有什么
    let mut ancestor = parent_of(&standardized(target));
    while !ancestor.is_dir() {
        let up = parent_of(&ancestor);
        if up == ancestor {
            break;
        }
        ancestor = up;
    }
    if ancestor.is_dir() {
        let mut items = visible_entries(&ancestor);
        items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| natural_cmp(&a.0, &b.0)));
        let mut names: Vec<String> =
            items.iter().take(15).map(|(name, directory, _)| format!("{name}{}", if *directory { "/" } else { "" })).collect();
        if items.len() > 15 {
            names.push(format!("…（共 {} 项）", items.len()));
        }
        hints.push(format!(
            "最近一层在的上级目录 {} 里有：{}",
            native(&ancestor),
            if names.is_empty() { "（空的）".to_string() } else { names.join("、") }
        ));
    }
    // 工作目录下同名的（跳过隐藏目录和依赖、构建目录；最多看 5 层、5000 项）
    let name = last_component(path);
    let base = if Path::new(working_directory).is_dir() { PathBuf::from(working_directory) } else { ancestor.clone() };
    if !name.is_empty() && base.is_dir() {
        const SKIPPED: [&str; 6] = ["node_modules", ".git", "target", "dist", "__pycache__", ".venv"];
        let mut found = Vec::new();
        let mut queue = std::collections::VecDeque::from([(base, 0)]);
        let mut visited = 0;
        while let Some((directory, depth)) = queue.pop_front() {
            if found.len() >= 5 || visited >= 5_000 {
                break;
            }
            let mut items = visible_entries(&directory);
            items.sort_by(|a, b| a.0.cmp(&b.0));
            for (item, is_directory, link) in items {
                visited += 1;
                if visited > 5_000 || found.len() >= 5 {
                    break;
                }
                let child = directory.join(&item);
                if item.to_lowercase() == name.to_lowercase() && is_directory == want_directory {
                    found.push(native(&child));
                }
                if is_directory && !link && depth < 5 && !SKIPPED.contains(&item.as_str()) && !item.starts_with("build") {
                    queue.push_back((child, depth + 1));
                }
            }
        }
        if !found.is_empty() {
            hints.push(format!("同名的在：{}", found.join("、")));
        }
    }
    if hints.is_empty() { String::new() } else { format!("\n（{}）", hints.join("；")) }
}

fn failure_with_hint(path: &str, working_directory: &str, want_directory: bool) -> ToolError {
    ToolError::Failed(read_failure(path) + &path_hint(path, working_directory, want_directory))
}

// MARK: - 写

fn write_failure(path: &Path, denied: bool) -> String {
    let name = last_component(&display(path));
    let folder = last_component(&display(&parent_of(path)));
    if denied {
        format!("你没有将文件“{name}”保存到文件夹“{folder}”中的权限。")
    } else {
        format!("未能将文件“{name}”保存到文件夹“{folder}”中。")
    }
}

/// == FileManager.createDirectory(at:withIntermediateDirectories: true)：已存在就什么也不做
pub fn create_directory(path: &Path) -> Result<(), String> {
    let target = standardized(path);
    if target.is_dir() {
        return Ok(());
    }
    // 往上找到已经存在的那一级
    let mut existing = target.clone();
    while !existing.exists() {
        let up = parent_of(&existing);
        if up == existing {
            break;
        }
        existing = up;
    }
    if existing.exists() && !existing.is_dir() {
        let name = last_component(&display(&existing));
        let folder = last_component(&display(&parent_of(&existing)));
        // 要建的目录本身是个文件：NSFileWriteFileExistsError；路径中间有文件：NSFileWriteUnknownError（报挡路的那个文件）
        return Err(if existing == target {
            format!("未能将文件“{name}”保存在文件夹“{folder}”中，因为已存在同名文件。")
        } else {
            format!("未能将文件“{name}”保存到文件夹“{folder}”中。")
        });
    }
    fs::create_dir_all(&target).map_err(|error| write_failure(&target, is_permission(&error)))
}

/// == Data.write(to:)（原地写，不建目录）：目录没了报「文件“X”不存在。」，不留下只有这个文件的空壳
pub fn write_in_place(path: &Path, data: &[u8]) -> Result<(), String> {
    if !parent_of(path).is_dir() || path.is_dir() {
        return Err(format!("文件“{}”不存在。", last_component(&display(path))));
    }
    fs::write(path, data).map_err(|error| write_failure(path, is_permission(&error)))
}

/// == String.write(to:atomically: true)：先写同目录里的临时文件再替换（目标只读也能写，权限保留）
pub fn write_atomically(path: &Path, data: &[u8]) -> Result<(), String> {
    let parent = parent_of(path);
    if !parent.is_dir() {
        return Err(format!("文件夹“{}”不存在。", last_component(&display(path))));
    }
    if path.is_dir() {
        return Err(format!("文件“{}”不存在。", last_component(&display(path))));
    }
    let permissions = fs::metadata(path).ok().map(|info| info.permissions());
    let temporary = parent.join(format!(".{}.friday-{}", last_component(&display(path)), std::process::id()));
    let written = (|| -> io::Result<()> {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        // 只读的目标：替换前先去掉只读（Windows 上只读文件替换不了），换完照原来的权限设回去
        if let Some(permissions) = permissions.as_ref().filter(|permissions| permissions.readonly()) {
            let mut writable = permissions.clone();
            #[allow(clippy::permissions_set_readonly_false)]
            writable.set_readonly(false);
            let _ = fs::set_permissions(path, writable);
        }
        fs::rename(&temporary, path)?;
        if let Some(permissions) = permissions {
            fs::set_permissions(path, permissions)?;
        }
        Ok(())
    })();
    written.map_err(|error| {
        let _ = fs::remove_file(&temporary);
        write_failure(path, is_permission(&error))
    })
}

/// 读 UTF-8 文字（读不了、不是 UTF-8 的为 None）
fn read_text(path: &Path) -> Option<String> {
    fs::read(path).ok().and_then(|data| String::from_utf8(data).ok())
}

// MARK: - 工具

/// == ByteCountFormatter(.file)（zh-Hans）：0 →「0 KB」；不到 1000 字节 →「N 字节」；KB 取整、MB 一位小数、GB 起两位小数，去掉末尾的 0
pub fn byte_count(bytes: i64) -> String {
    let sign = if bytes < 0 { "-" } else { "" };
    let magnitude = (bytes as f64).abs();
    if magnitude == 0.0 {
        return "0 KB".into();
    }
    if magnitude < 1000.0 {
        return format!("{sign}{} 字节", magnitude as i64);
    }
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    const DECIMALS: [i32; 5] = [0, 1, 2, 2, 2];
    let mut value = magnitude / 1000.0;
    let mut unit = 0;
    let rounded = |value: f64, unit: usize| {
        let factor = 10f64.powi(DECIMALS[unit]);
        (value * factor).round() / factor
    };
    while unit < 4 && rounded(value, unit) >= 1000.0 {
        value /= 1000.0;
        unit += 1;
    }
    let mut text = format!("{:.*}", DECIMALS[unit] as usize, rounded(value, unit));
    if text.contains('.') {
        text = text.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    format!("{sign}{text} {}", UNITS[unit])
}

/// list_directory：按名字排的前 200 项（跳过隐藏的；目录带 /，文件带大小）
pub fn list_directory(path: &str, working_directory: &str) -> Result<Output, ToolError> {
    let target = Path::new(path);
    let entries = match fs::read_dir(target) {
        Ok(entries) if target.is_dir() => entries,
        _ => return Err(failure_with_hint(path, working_directory, true)),
    };
    let mut items: Vec<(String, fs::DirEntry)> = entries
        .flatten()
        .map(|entry| (entry.file_name().to_string_lossy().into_owned(), entry))
        .filter(|(name, _)| !name.starts_with('.'))
        .collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    let shown = display_directory(path);
    let lines: Vec<String> = items
        .iter()
        .take(200)
        .map(|(name, entry)| {
            // 不跟随符号链接（链接的大小是目标路径的字节数）
            let info = fs::symlink_metadata(entry.path()).ok();
            if info.as_ref().is_some_and(|info| info.is_dir()) {
                format!("📁 {name}/")
            } else {
                format!("📄 {name}  ({})", byte_count(info.map_or(0, |info| info.len() as i64)))
            }
        })
        .collect();
    Ok(Output::text(if lines.is_empty() { format!("（空目录）{shown}") } else { format!("{shown}\n{}", lines.join("\n")) }))
}

/// 给模型看的目录路径（== URL.path：去掉结尾的 /）
fn display_directory(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() && !path.is_empty() { "/".into() } else { trimmed.to_string() }
}

/// read_file（文字）：给了 offset / limit 或文件太长就按行读（带行号、写明接着从哪行读）
pub fn read_file(path: &str, offset: Option<i64>, limit: Option<i64>, working_directory: &str) -> Result<Output, ToolError> {
    let target = Path::new(path);
    if target.is_dir() {
        return Err(failure_with_hint(path, working_directory, false));
    }
    let data = fs::read(target).map_err(|_| failure_with_hint(path, working_directory, false))?;
    let text = String::from_utf8(data).map_err(|_| ToolError::Failed(INVALID_TEXT_ENCODING.into()))?;
    if offset.is_none() && limit.is_none() && text.encode_utf16().count() <= MAX_OUTPUT {
        return Ok(Output::text(truncate(&text)));
    }
    Ok(Output::text(file_search::read_lines(&text, offset, limit, MAX_OUTPUT)))
}

/// write_file：建好上级目录、整份写入；原来是文字（或没有这个文件）的给出增删行数
pub fn write_file(path: &str, content: &str) -> Result<Output, ToolError> {
    let target = Path::new(path);
    let before = if target.exists() { read_text(target) } else { Some(String::new()) };
    create_directory(&parent_of(target)).map_err(ToolError::Failed)?;
    write_atomically(target, content.as_bytes()).map_err(ToolError::Failed)?;
    Ok(Output {
        text: format!("已写入 {} 字节到 {path}", content.len()),
        line_diff: before.map(|before| line_diff::count_text(&before, content)),
    })
}

/// edit_file 的改法：一处（old_text / new_text / replace_all）或几处（edits）
pub enum Change {
    One { old_text: String, new_text: String, replace_all: bool },
    Several(Vec<Edit>),
}

/// edit_file：读出原文、改好写回，给出改完的几行和增删行数
pub fn edit_file(path: &str, change: &Change, working_directory: &str) -> Result<Output, ToolError> {
    let target = Path::new(path);
    if target.is_dir() {
        return Err(failure_with_hint(path, working_directory, false));
    }
    let data = fs::read(target).map_err(|_| failure_with_hint(path, working_directory, false))?;
    let text = String::from_utf8(data).map_err(|_| ToolError::Failed(INVALID_TEXT_ENCODING.into()))?;
    let (edited, summary) = match change {
        Change::Several(edits) => {
            let result = file_edit::apply_all(&text, edits).map_err(ToolError::Invalid)?;
            let summary = file_edit::summary_all(path, &result);
            (result.text, summary)
        }
        Change::One { old_text, new_text, replace_all } => {
            let result = file_edit::apply(&text, old_text, new_text, *replace_all).map_err(ToolError::Invalid)?;
            let summary = file_edit::summary(path, &result);
            (result.text, summary)
        }
    };
    write_atomically(target, edited.as_bytes()).map_err(ToolError::Failed)?;
    Ok(Output { text: truncate(&summary), line_diff: Some(line_diff::count_text(&text, &edited)) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("friday-core-tools-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn formatting() {
        assert_eq!(byte_count(0), "0 KB");
        assert_eq!(byte_count(999), "999 字节");
        assert_eq!(byte_count(1_499), "1 KB");
        assert_eq!(byte_count(999_500), "1 MB");
        assert_eq!(byte_count(1_250_000), "1.3 MB");
        assert_eq!(byte_count(2_000_000), "2 MB");
        assert_eq!(byte_count(1_234_567_890), "1.23 GB");
        let long = "字".repeat(9_000);
        let cut = truncate(&long);
        assert!(cut.contains("（已截断：共 9000 字符，中间省略 1000 字符"));
        assert_eq!(truncate("短"), "短");
        assert_eq!(page("abc", None), "abc");
        assert_eq!(page("abc", Some(1)), "bc\n（正文共 3 字，已读到末尾）");
        assert_eq!(page("abc", Some(5)), "（正文共 3 字，第 5 字起没有内容）");
        assert!(page(&"x".repeat(9_000), None).ends_with("这里是第 1–8000 字；接着读用 offset=8000）"));
        assert_eq!(strip_html("<html><style>b{}</style><script>x()</script><p>a &amp; b</p>\n\n  <!-- c -->  <b>d</b></html>"), "a & b\nd");
        assert_eq!(last_component("/a/b/"), "b");
        assert_eq!(last_component("/"), "/");
        assert_eq!(standardized(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        assert_eq!(natural_cmp("file10", "File9"), std::cmp::Ordering::Greater);
    }

    #[test]
    fn files() {
        let dir = scratch("files");
        let wd = display(&dir);
        let file = dir.join("sub/a.txt");
        let path = display(&file);
        let written = write_file(&path, "one\ntwo\n").unwrap();
        assert_eq!(written.text, format!("已写入 8 字节到 {path}"));
        assert_eq!(written.line_diff.map(|counts| (counts.added, counts.removed)), Some((2, 0)));
        let read = read_file(&path, None, None, &wd).unwrap();
        assert_eq!(read.text, "one\ntwo\n");
        let lines = read_file(&path, Some(2), None, &wd).unwrap();
        assert_eq!(lines.text, "2\ttwo\n（文件共 2 行，已读到末尾）");
        let edited = edit_file(&path, &Change::One { old_text: "two".into(), new_text: "2".into(), replace_all: false }, &wd).unwrap();
        assert!(edited.text.starts_with(&format!("已修改 {path}：替换了 1 处")));
        assert_eq!(fs::read_to_string(&file).unwrap(), "one\n2\n");
        let bad = edit_file(&path, &Change::One { old_text: "nope".into(), new_text: "x".into(), replace_all: false }, &wd);
        assert!(matches!(bad, Err(ToolError::Invalid(_))));
        let listing = list_directory(&display(&dir.join("sub")), &wd).unwrap();
        assert_eq!(listing.text, format!("{}\n📄 a.txt  (6 字节)", display(&dir.join("sub"))));
        fs::create_dir_all(dir.join("empty")).unwrap();
        assert!(list_directory(&display(&dir.join("empty")), &wd).unwrap().text.starts_with("（空目录）"));
        // 不存在：报错 + 上级目录里有什么、工作目录下同名的在哪
        let missing = read_file(&display(&dir.join("a.txt")), None, None, &wd).unwrap_err();
        assert!(missing.message().starts_with("未能打开文件“a.txt”，因为它不存在。"), "{missing:?}");
        assert!(missing.message().contains("里有：empty/、sub/") && missing.message().contains("同名的在："), "{missing:?}");
        // 把目录当文件读、把文件当目录列
        assert!(
            read_file(&display(&dir.join("sub")), None, None, &wd)
                .unwrap_err()
                .message()
                .ends_with("（这是一个目录：用 list_directory 看里面有什么）")
        );
        assert!(list_directory(&path, &wd).unwrap_err().message().ends_with("（这是一个文件：用 read_file 读）"));
        fs::write(dir.join("bin.dat"), [0xff, 0xfe, 0x00]).unwrap();
        assert_eq!(
            read_file(&display(&dir.join("bin.dat")), None, None, &wd).unwrap_err(),
            ToolError::Failed(INVALID_TEXT_ENCODING.into())
        );
        // 写进不是目录的地方
        assert!(
            write_file(&display(&file.join("x.txt")), "x")
                .unwrap_err()
                .message()
                .starts_with("未能将文件“a.txt”保存在文件夹“sub”中，因为已存在同名文件。")
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
