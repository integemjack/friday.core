//! find_files（按文件名找）、search_files（按正则搜内容）、search_keywords（按关键词搜内容）和 read_file 按行读（offset / limit）
//! （== Swift `Agent/FileSearch.swift`、Qt `agent/FileSearch.cpp`）。
//! 进程内遍历：跳过隐藏目录和依赖 / 构建目录（直接把 path 指过去就会搜），不跟随符号链接；二进制文件和超过 2 MB 的文件不搜内容；
//! 遍历有文件数和时间上限，超了就交回已找到的并说明不全。路径在工作目录下时写相对路径（read_file 能直接用），否则写绝对路径。
//! 字数按用户看到的字（Swift 的 Character）算，按行读的长度按 UTF-16 算。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use regex::{Regex, RegexBuilder};
use unicode_segmentation::UnicodeSegmentation;

use crate::tool_files::{self, ToolError};

/// 最多列几条（find_files 的文件、search_files 的匹配行）
pub const MAX_RESULTS: usize = 200;
/// 遍历的文件数上限
pub const MAX_FILES: usize = 50_000;
/// 遍历的时间上限（秒）
pub const MAX_SECONDS: u64 = 30;
/// 超过这个大小的文件不搜内容
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// 搜索结果里每行最多显示几个字
pub const MAX_LINE_LENGTH: usize = 200;
/// search_keywords 最多几个关键词
pub const MAX_KEYWORDS: usize = 10;
/// search_keywords 每个文件最多列几行
pub const KEYWORD_LINES_PER_FILE: usize = 5;
/// search_keywords 结果最多多少字（UTF-16；再多就不列后面的文件，免得被 8000 字的输出上限从中间截掉）
pub const MAX_KEYWORD_OUTPUT: usize = 7_000;

const SKIPPED_NOTE: &str = "隐藏目录和 node_modules、build、dist、third_party 这类依赖 / 构建目录没有进去，要找就把 path 直接指过去";

fn graphemes(text: &str) -> Vec<&str> {
    text.graphemes(true).collect()
}

/// 遍历时跳过的目录：隐藏目录（. 开头）、依赖和构建产物
pub fn skips_directory(name: &str) -> bool {
    name.starts_with('.')
        || ["node_modules", "DerivedData", "Pods", "__pycache__", "third_party", "dist", "build"].contains(&name)
        || name.starts_with("build-")
        || name.starts_with("build_")
}

/// 文件名通配转成正则（不区分大小写）：* 匹配除 / 外任意字符，? 匹配一个字符，** 跨目录，{a,b} 任选其一。
/// 通配里有 / 时按相对搜索目录的路径匹配，否则只按文件名匹配（见 matches）
pub fn glob_expression(glob: &str) -> Option<Regex> {
    let chars: Vec<char> = glob.chars().collect();
    let mut pattern = String::new();
    let mut braces = 0;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' if chars.get(i + 1) == Some(&'*') => {
                // **/ 跨任意层目录（含零层），单独的 ** 匹配任意字符
                if chars.get(i + 2) == Some(&'/') {
                    pattern += "(?:.*/)?";
                    i += 2;
                } else {
                    pattern += ".*";
                    i += 1;
                }
            }
            '*' => pattern += "[^/]*",
            '?' => pattern += "[^/]",
            '{' => {
                braces += 1;
                pattern += "(?:";
            }
            '}' if braces > 0 => {
                braces -= 1;
                pattern += ")";
            }
            ',' if braces > 0 => pattern += "|",
            other => pattern += &regex::escape(&other.to_string()),
        }
        i += 1;
    }
    pattern += &")".repeat(braces);
    RegexBuilder::new(&format!("^{pattern}$")).case_insensitive(true).build().ok()
}

/// relative_path 用 / 分隔
pub fn matches(expression: &Regex, glob: &str, relative_path: &str) -> bool {
    let subject = if glob.contains('/') { relative_path } else { relative_path.rsplit('/').next().unwrap_or("") };
    expression.is_match(subject)
}

/// 给模型看的路径：在工作目录下写相对路径，否则写绝对路径
pub fn display_path(path: &str, working_directory: &str) -> String {
    let full = tool_files::standardized(Path::new(path)).to_string_lossy().into_owned();
    if working_directory.is_empty() {
        return full;
    }
    let base = tool_files::standardized(Path::new(working_directory)).to_string_lossy().into_owned();
    let (full_lower, base_lower) = (full.to_lowercase(), base.to_lowercase());
    if full_lower == base_lower {
        return ".".into();
    }
    let trimmed = base_lower.trim_end_matches(['/', '\\']);
    for separator in ['/', '\\'] {
        let prefix = format!("{trimmed}{separator}");
        if full_lower.starts_with(&prefix) && full.is_char_boundary(prefix.len()) {
            return full[prefix.len()..].to_string();
        }
    }
    full
}

/// 按名字排序（先文件后目录），跳过隐藏的、不跟随符号链接；超出文件数或时间上限时 incomplete = true 并停下
struct Walker {
    visited: usize,
    incomplete: bool,
    started: Instant,
}

impl Walker {
    fn new() -> Self {
        Walker { visited: 0, incomplete: false, started: Instant::now() }
    }

    fn exhausted(&mut self) -> bool {
        if self.visited >= MAX_FILES || self.started.elapsed() >= Duration::from_secs(MAX_SECONDS) {
            self.incomplete = true;
        }
        self.incomplete
    }

    fn run(&mut self, root: &Path, visit: &mut dyn FnMut(&Path, &str)) {
        self.walk(root, "", visit);
    }

    fn walk(&mut self, directory: &Path, relative: &str, visit: &mut dyn FnMut(&Path, &str)) {
        let mut entries: Vec<(String, PathBuf, Option<fs::FileType>)> = fs::read_dir(directory)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| (entry.file_name().to_string_lossy().into_owned(), entry.path(), entry.file_type().ok()))
                    .collect()
            })
            .unwrap_or_default();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let join = |name: &str| if relative.is_empty() { name.to_string() } else { format!("{relative}/{name}") };
        let mut directories = Vec::new();
        for (name, path, kind) in entries {
            if self.exhausted() {
                return;
            }
            if name.starts_with('.') {
                continue;
            }
            let Some(kind) = kind else { continue };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                if !skips_directory(&name) {
                    directories.push((name, path));
                }
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            self.visited += 1;
            visit(&path, &join(&name));
        }
        for (name, path) in directories {
            if self.exhausted() {
                return;
            }
            self.walk(&path, &join(&name), visit);
        }
    }

    fn note(&self) -> String {
        format!("（文件太多，只看了 {} 个文件（上限 {MAX_FILES} 个、{MAX_SECONDS} 秒），结果不全；缩小 path 再找）", self.visited)
    }
}

fn extension_is_empty(path: &str) -> bool {
    Path::new(&tool_files::last_component(path)).extension().is_none_or(|extension| extension.is_empty())
}

fn existing(root: &str) -> Result<(PathBuf, bool), ToolError> {
    let path = Path::new(root);
    if !path.exists() {
        // v2.5 带上提示：最近一层在的上级目录里有什么、同名的在哪（有扩展名的当文件找）
        return Err(ToolError::Failed(format!(
            "找不到“{root}”：路径不存在。{}",
            tool_files::path_hint(root, "", extension_is_empty(root))
        )));
    }
    Ok((tool_files::standardized(path), path.is_dir()))
}

fn clip_line(line: &str) -> String {
    let trimmed = line.trim();
    let parts = graphemes(trimmed);
    if parts.len() > MAX_LINE_LENGTH { parts[..MAX_LINE_LENGTH].concat() + "…" } else { trimmed.to_string() }
}

/// 长行截取 at（在原行里的位置，按字）附近的一段，让关键词留在显示的部分里
fn clip_around(line: &str, at: usize) -> String {
    let trimmed = line.trim();
    let parts = graphemes(trimmed);
    if parts.len() <= MAX_LINE_LENGTH {
        return trimmed.to_string();
    }
    let leading = graphemes(line).iter().take_while(|part| part.chars().all(char::is_whitespace)).count();
    let start = (at as isize - leading as isize - (MAX_LINE_LENGTH / 4) as isize).max(0) as usize;
    let start = start.min(parts.len() - MAX_LINE_LENGTH);
    let piece = parts[start..start + MAX_LINE_LENGTH].concat();
    format!("{}{piece}{}", if start > 0 { "…" } else { "" }, if start + MAX_LINE_LENGTH < parts.len() { "…" } else { "" })
}

/// 读出要搜的文本：超过 2 MB 的记进 large、二进制的（开头一段里有 NUL）和读不了的不搜
fn searchable_text(file: &Path, large: &mut usize) -> Option<String> {
    let size = fs::metadata(file).map(|info| info.len()).unwrap_or(0);
    if size > MAX_FILE_BYTES {
        *large += 1;
        return None;
    }
    let data = fs::read(file).ok()?;
    if data.iter().take(8_000).any(|&byte| byte == 0) {
        return None;
    }
    Some(String::from_utf8_lossy(&data).into_owned())
}

/// glob 参数：空的不过滤，写错了报错
fn glob_filter(glob: Option<&str>) -> Result<Option<(String, Regex)>, ToolError> {
    let Some(glob) = glob.map(str::trim).filter(|glob| !glob.is_empty()) else { return Ok(None) };
    let expression = glob_expression(glob).ok_or_else(|| ToolError::Invalid(format!("glob 写法不对：{glob}")))?;
    Ok(Some((glob.to_string(), expression)))
}

/// 按行拆开：\n 和 \r\n 都算换行，末尾的换行不多算一行
pub fn split_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line)).collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// find_files：root 下（含子目录）名字匹配 pattern 的文件
pub fn find_files(pattern: &str, root: &str, working_directory: &str) -> Result<String, ToolError> {
    let (url, is_directory) = existing(root)?;
    if !is_directory {
        return Err(ToolError::Invalid(format!("path 要是目录：{root}")));
    }
    let expression = glob_expression(pattern).ok_or_else(|| ToolError::Invalid(format!("通配写法不对：{pattern}")))?;
    let mut found = Vec::new();
    let mut total = 0;
    let mut walker = Walker::new();
    walker.run(&url, &mut |path, relative| {
        if !matches(&expression, pattern, relative) {
            return;
        }
        total += 1;
        if found.len() < MAX_RESULTS {
            found.push(display_path(&path.to_string_lossy(), working_directory));
        }
    });
    let mut lines = Vec::new();
    if found.is_empty() {
        lines.push(format!(
            "没有找到名字匹配「{pattern}」的文件（在 {} 下找的；{SKIPPED_NOTE}）",
            display_path(&url.to_string_lossy(), working_directory)
        ));
    } else {
        let shown = found.len();
        lines.extend(found);
        if total > shown {
            lines.push(format!("…（共 {total} 个，只列出前 {shown} 个；缩小 path 或换更具体的 pattern 再找）"));
        }
    }
    if walker.incomplete {
        lines.push(walker.note());
    }
    Ok(lines.join("\n"))
}

/// search_files：root（目录或单个文件）下内容匹配正则 pattern 的行；glob 按文件名过滤。pattern 不是有效的正则时按字面搜（结果开头说明）
pub fn search_files(
    pattern: &str,
    root: &str,
    glob: Option<&str>,
    ignore_case: bool,
    working_directory: &str,
) -> Result<String, ToolError> {
    let (url, is_directory) = existing(root)?;
    let flags = if ignore_case { "(?i)" } else { "" };
    let mut literal_note = None;
    let regex = match fancy_regex::Regex::new(&format!("{flags}{pattern}")) {
        Ok(regex) => regex,
        Err(error) => {
            // 想按字面找却没转义（foo(、[x]）：直接按字面搜并说明
            literal_note = Some(format!("（「{pattern}」不是有效的正则（{error}），按字面搜了）"));
            fancy_regex::Regex::new(&format!("{flags}{}", regex::escape(pattern))).map_err(|error| ToolError::Invalid(error.to_string()))?
        }
    };
    let filter = glob_filter(glob)?;
    let (mut shown, mut match_count, mut files, mut searched, mut large) = (Vec::new(), 0, 0, 0, 0);
    let mut search = |file: &Path| {
        let Some(text) = searchable_text(file, &mut large) else { return };
        searched += 1;
        let mut hit = false;
        for (index, line) in split_lines(&text).into_iter().enumerate() {
            if !regex.is_match(line).unwrap_or(false) {
                continue;
            }
            hit = true;
            match_count += 1;
            if shown.len() < MAX_RESULTS {
                shown.push(format!("{}:{}: {}", display_path(&file.to_string_lossy(), working_directory), index + 1, clip_line(line)));
            }
        }
        if hit {
            files += 1;
        }
    };
    let mut walker = Walker::new();
    if is_directory {
        walker.run(&url, &mut |file, relative| {
            if let Some((glob, expression)) = &filter
                && !matches(expression, glob, relative)
            {
                return;
            }
            search(file);
        });
    } else {
        search(&url);
    }
    let mut lines: Vec<String> = literal_note.into_iter().collect();
    if shown.is_empty() {
        let place = display_path(&url.to_string_lossy(), working_directory);
        lines.push(if is_directory {
            format!("没有找到匹配「{pattern}」的内容（在 {place} 下搜了 {searched} 个文件；{SKIPPED_NOTE}）")
        } else {
            format!("没有找到匹配「{pattern}」的内容（{place}）")
        });
    } else {
        let count = shown.len();
        lines.extend(shown);
        let more = if match_count > count {
            format!("；只列出前 {count} 处，缩小 path / glob 或换更具体的 pattern 再搜")
        } else {
            String::new()
        };
        lines.push(format!("（共 {match_count} 处，在 {files} 个文件里{more}）"));
    }
    if large > 0 {
        lines.push(format!("（{large} 个超过 2 MB 的文件没有搜）"));
    }
    if walker.incomplete {
        lines.push(walker.note());
    }
    Ok(lines.join("\n"))
}

/// 把关键词串拆开：按空白分开，双引号（"…" 或 “…”）括起来的算一个词（可以带空格）；去掉重复的（不分大小写时不分大小写地比）
pub fn split_keywords(text: &str, match_case: bool) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut add = |word: &str| {
        let trimmed = word.trim();
        if trimmed.is_empty() {
            return;
        }
        let duplicate =
            words.iter().any(|known| if match_case { known == trimmed } else { known.to_lowercase() == trimmed.to_lowercase() });
        if !duplicate {
            words.push(trimmed.to_string());
        }
    };
    let mut current = String::new();
    let mut closing: Option<char> = None;
    for c in text.chars() {
        if let Some(end) = closing {
            if c == end || (end == '”' && c == '"') {
                add(&current);
                current.clear();
                closing = None;
            } else {
                current.push(c);
            }
        } else if c == '"' || c == '“' {
            add(&current);
            current.clear();
            closing = Some(if c == '“' { '”' } else { '"' });
        } else if c.is_whitespace() {
            add(&current);
            current.clear();
        } else {
            current.push(c);
        }
    }
    add(&current);
    words
}

struct KeywordLine<'a> {
    number: usize,
    words: usize,
    at: usize,
    text: &'a str,
}

struct Hit {
    path: String,
    present: Vec<bool>,
    words: usize,
    full_lines: usize,
    name_words: usize,
    lines: usize,
    shown: Vec<(usize, String)>,
}

/// search_keywords：按字面找几个词，默认要一个文件里都有（可以不在同一行；文件名里有也算），match_any 时有一个就算。
/// 文件按「有几个词 → 几行同时有全部的词 → 文件名里有几个词 → 几行有词」排序，每个文件列出词最多的几行；没找到时分开说每个词在几个文件里
pub fn search_keywords(
    keywords: &str,
    root: &str,
    glob: Option<&str>,
    match_case: bool,
    match_any: bool,
    working_directory: &str,
) -> Result<String, ToolError> {
    let (url, is_directory) = existing(root)?;
    let words = split_keywords(keywords, match_case);
    if words.is_empty() {
        return Err(ToolError::Invalid("keywords 是空的：写要找的几个词，空格分开".into()));
    }
    if words.len() > MAX_KEYWORDS {
        return Err(ToolError::Invalid(format!("关键词太多（{} 个，最多 {MAX_KEYWORDS} 个）：留下最能区分的几个", words.len())));
    }
    let filter = glob_filter(glob)?;
    let finders: Vec<Regex> =
        words.iter().map(|word| RegexBuilder::new(&regex::escape(word)).case_insensitive(!match_case).build().unwrap()).collect();
    let count = words.len();
    let mut hits: Vec<Hit> = Vec::new();
    let mut files_with = vec![0usize; count];
    let (mut searched, mut large) = (0, 0);
    let mut search = |file: &Path, name: &str| {
        let Some(text) = searchable_text(file, &mut large) else { return };
        searched += 1;
        let mut hit = Hit {
            path: file.to_string_lossy().into_owned(),
            present: vec![false; count],
            words: 0,
            full_lines: 0,
            name_words: 0,
            lines: 0,
            shown: Vec::new(),
        };
        let mut any = false;
        for (k, finder) in finders.iter().enumerate() {
            if finder.is_match(name) {
                hit.present[k] = true;
                hit.name_words += 1;
                any = true;
            } else if finder.is_match(&text) {
                any = true;
            }
        }
        if !any {
            return;
        }
        let mut matched: Vec<KeywordLine> = Vec::new();
        for (index, line) in split_lines(&text).into_iter().enumerate() {
            let mut in_line = 0;
            let mut first: Option<usize> = None;
            for (k, finder) in finders.iter().enumerate() {
                let Some(found) = finder.find(line) else { continue };
                in_line += 1;
                hit.present[k] = true;
                let at = line[..found.start()].graphemes(true).count();
                first = Some(first.map_or(at, |first| first.min(at)));
            }
            if in_line == 0 {
                continue;
            }
            hit.lines += 1;
            if in_line == count {
                hit.full_lines += 1;
            }
            matched.push(KeywordLine { number: index + 1, words: in_line, at: first.unwrap_or(0), text: line });
        }
        hit.words = hit.present.iter().filter(|&&present| present).count();
        for (files, &present) in files_with.iter_mut().zip(&hit.present) {
            if present {
                *files += 1;
            }
        }
        if if match_any { hit.words == 0 } else { hit.words < count } {
            return;
        }
        // 词最多的几行，再按行号排
        let mut order: Vec<usize> = (0..matched.len()).collect();
        order.sort_by(|&a, &b| matched[b].words.cmp(&matched[a].words).then(a.cmp(&b)));
        let mut chosen: Vec<&KeywordLine> = order.into_iter().take(KEYWORD_LINES_PER_FILE).map(|index| &matched[index]).collect();
        chosen.sort_by_key(|line| line.number);
        hit.shown = chosen.into_iter().map(|line| (line.number, clip_around(line.text, line.at))).collect();
        hits.push(hit);
    };
    let mut walker = Walker::new();
    if is_directory {
        walker.run(&url, &mut |file, relative| {
            if let Some((glob, expression)) = &filter
                && !matches(expression, glob, relative)
            {
                return;
            }
            search(file, relative.rsplit('/').next().unwrap_or(""));
        });
    } else {
        search(&url, &tool_files::last_component(&url.to_string_lossy()));
    }
    // 同样好的按遍历顺序（先文件后子目录、按名字）
    let mut ranked: Vec<usize> = (0..hits.len()).collect();
    ranked.sort_by(|&a, &b| {
        let (x, y) = (&hits[a], &hits[b]);
        y.words
            .cmp(&x.words)
            .then(y.full_lines.cmp(&x.full_lines))
            .then(y.name_words.cmp(&x.name_words))
            .then(y.lines.cmp(&x.lines))
            .then(a.cmp(&b))
    });
    let quoted = format!("「{}」", words.join("」「"));
    let place = display_path(&url.to_string_lossy(), working_directory);
    let mut out = Vec::new();
    if ranked.is_empty() {
        let location = if is_directory { format!("在 {place} 下搜了 {searched} 个文件；{SKIPPED_NOTE}") } else { place };
        if count == 1 || match_any {
            out.push(format!("没有找到含{quoted}的文件（{location}）"));
        } else {
            out.push(format!("没有文件同时含{quoted}（{location}）"));
            let each: Vec<String> = (0..count)
                .map(|k| {
                    if files_with[k] > 0 {
                        format!("「{}」在 {} 个文件里", words[k], files_with[k])
                    } else {
                        format!("「{}」哪里都没有", words[k])
                    }
                })
                .collect();
            out.push(format!("分开看：{}。少写一两个词再搜，或者 match_any 列出含其中任一个词的文件", each.join("，")));
        }
    } else {
        let (mut used, mut listed) = (0, 0);
        for &index in &ranked {
            let hit = &hits[index];
            let mut detail = if hit.lines == 0 {
                "文件名匹配，内容里没有".to_string()
            } else if count == 1 {
                format!("{} 行", hit.lines)
            } else if hit.full_lines > 0 {
                format!("{} 行有关键词，{} 行几个词都在", hit.lines, hit.full_lines)
            } else {
                format!("{} 行有关键词，没有一行几个词都在", hit.lines)
            };
            if hit.words < count {
                let missing: Vec<&str> = (0..count).filter(|&k| !hit.present[k]).map(|k| words[k].as_str()).collect();
                detail += &format!("；没有「{}」", missing.join("」「"));
            }
            let mut block = vec![format!("{}（{detail}）", display_path(&hit.path, working_directory))];
            block.extend(hit.shown.iter().map(|(number, text)| format!("  {number}: {text}")));
            if hit.lines > hit.shown.len() {
                block.push(format!("  …（还有 {} 行）", hit.lines - hit.shown.len()));
            }
            let text = block.join("\n");
            let length = text.encode_utf16().count();
            if listed > 0 && used + length > MAX_KEYWORD_OUTPUT {
                break;
            }
            out.push(text);
            used += length + 1;
            listed += 1;
        }
        let which = if count == 1 {
            format!("含{quoted}")
        } else if match_any {
            "含其中的词".to_string()
        } else {
            format!("都含{quoted}")
        };
        let more = if listed < ranked.len() {
            format!("；只列出前 {listed} 个，加关键词、缩小 path / glob 再搜")
        } else {
            String::new()
        };
        out.push(format!("（{} 个文件{which}，搜了 {searched} 个文件{more}）", ranked.len()));
    }
    if large > 0 {
        out.push(format!("（{large} 个超过 2 MB 的文件没有搜）"));
    }
    if walker.incomplete {
        out.push(walker.note());
    }
    Ok(out.join("\n"))
}

/// read_file 按行读：从第 offset 行（1 起）开始最多 limit 行，每行前面带行号和制表符，总长不超过 max_characters（按 UTF-16 计）；
/// 结尾写明共几行、接着从哪行读
pub fn read_lines(text: &str, offset: Option<i64>, limit: Option<i64>, max_characters: usize) -> String {
    if text.is_empty() {
        return "（空文件）".into();
    }
    let lines = split_lines(text);
    let total = lines.len();
    if total == 0 {
        return "（空文件）".into();
    }
    let start = offset.unwrap_or(1).max(1) as usize;
    if start > total {
        return format!("（文件共 {total} 行，第 {start} 行起没有内容）");
    }
    let last = limit.map_or(total, |limit| if limit > 0 { total.min(start + limit as usize - 1) } else { total });
    let mut out = Vec::new();
    let mut used = 0;
    let mut shown = start - 1;
    for number in start..=last {
        let line = format!("{number}\t{}", lines[number - 1]);
        let length = line.encode_utf16().count();
        if used + length + 1 > max_characters {
            // 第一行就放不下：截短交回，其余的下次接着读
            if number == start {
                let parts = graphemes(&line);
                out.push(format!(
                    "{}…（这一行太长，只显示了前 {max_characters} 个字符）",
                    parts[..max_characters.min(parts.len())].concat()
                ));
                shown = number;
            }
            break;
        }
        out.push(line);
        used += length + 1;
        shown = number;
    }
    if shown < total {
        out.push(format!("…（文件共 {total} 行，这里是第 {start}–{shown} 行；接着读用 offset={}）", shown + 1));
    } else if start > 1 {
        out.push(format!("（文件共 {total} 行，已读到末尾）"));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("friday-core-search-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for (path, content) in [
            ("src/main.rs", "fn main() {\n    println!(\"hello world\");\n}\n"),
            ("src/lib/util.rs", "pub fn helper() -> u32 { 42 }\n// hello there\n"),
            ("README.md", "Hello project\nworld map\n"),
            ("node_modules/x/index.js", "hello"),
            (".git/config", "hello"),
            ("build-debug/out.txt", "hello"),
        ] {
            let file = dir.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, content).unwrap();
        }
        fs::write(dir.join("bin.dat"), b"hello\0world").unwrap();
        dir
    }

    #[test]
    fn globs() {
        let star = glob_expression("*.rs").unwrap();
        assert!(matches(&star, "*.rs", "src/main.rs") && !matches(&star, "*.rs", "README.md"));
        let deep = glob_expression("src/**/*.RS").unwrap();
        assert!(matches(&deep, "src/**/*.RS", "src/main.rs") && matches(&deep, "src/**/*.RS", "src/lib/util.rs"));
        let braces = glob_expression("*.{md,txt}").unwrap();
        assert!(matches(&braces, "*.{md,txt}", "a/README.md") && !matches(&braces, "*.{md,txt}", "a.rs"));
        assert!(skips_directory("node_modules") && skips_directory("build_x") && !skips_directory("builder"));
        assert_eq!(display_path("/a/b/c.txt", "/a"), "b/c.txt");
        assert_eq!(display_path("/A/b", "/a/"), "b");
        assert_eq!(display_path("/a", "/a"), ".");
        assert_eq!(display_path("/x/y", "/a"), "/x/y");
    }

    #[test]
    fn finds_and_searches() {
        let dir = tree("find");
        let root = dir.to_string_lossy().into_owned();
        assert_eq!(find_files("*.rs", &root, &root).unwrap(), "src/main.rs\nsrc/lib/util.rs");
        assert!(find_files("*.zzz", &root, &root).unwrap().starts_with("没有找到名字匹配「*.zzz」的文件（在 . 下找的；"));
        assert!(matches!(find_files("*.rs", &format!("{root}/README.md"), &root), Err(ToolError::Invalid(_))));
        let missing = find_files("*", &format!("{root}/nope"), &root).unwrap_err();
        assert!(missing.message().starts_with(&format!("找不到“{root}/nope”：路径不存在。")));
        let found = search_files("hello", &root, None, true, &root).unwrap();
        assert_eq!(
            found,
            "README.md:1: Hello project\nsrc/main.rs:2: println!(\"hello world\");\nsrc/lib/util.rs:2: // hello there\n（共 3 处，在 3 个文件里）"
        );
        assert!(
            search_files("hello", &root, Some("*.md"), false, &root)
                .unwrap()
                .starts_with("没有找到匹配「hello」的内容（在 . 下搜了 1 个文件")
        );
        let literal = search_files("println!(", &root, None, false, &root).unwrap();
        assert!(literal.starts_with("（「println!(」不是有效的正则（") && literal.contains("src/main.rs:2:"), "{literal}");
        assert!(search_files("(?<=fn )main", &root, None, false, &root).unwrap().starts_with("src/main.rs:1: fn main() {"));
        assert!(search_files("x", &root, Some("{"), false, &root).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn keywords() {
        assert_eq!(split_keywords("a  \"b c\" “d e” A", false), ["a", "b c", "d e"]);
        assert_eq!(split_keywords("a A", true), ["a", "A"]);
        let dir = tree("keywords");
        let root = dir.to_string_lossy().into_owned();
        let both = search_keywords("hello world", &root, None, false, false, &root).unwrap();
        assert_eq!(
            both,
            "src/main.rs（1 行有关键词，1 行几个词都在）\n  2: println!(\"hello world\");\nREADME.md（2 行有关键词，没有一行几个词都在）\n  1: Hello project\n  2: world map\n（2 个文件都含「hello」「world」，搜了 3 个文件）"
        );
        let none = search_keywords("hello nothing", &root, None, false, false, &root).unwrap();
        assert!(none.starts_with("没有文件同时含「hello」「nothing」") && none.contains("「nothing」哪里都没有"), "{none}");
        assert!(
            search_keywords("util", &root, None, false, false, &root).unwrap().starts_with("src/lib/util.rs（文件名匹配，内容里没有）")
        );
        assert!(matches!(search_keywords("  ", &root, None, false, false, &root), Err(ToolError::Invalid(_))));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn lines() {
        assert_eq!(read_lines("", None, None, 100), "（空文件）");
        assert_eq!(read_lines("a\r\nb\nc\n", Some(2), Some(1), 100), "2\tb\n…（文件共 3 行，这里是第 2–2 行；接着读用 offset=3）");
        assert_eq!(read_lines("a\nb", Some(5), None, 100), "（文件共 2 行，第 5 行起没有内容）");
        assert_eq!(
            read_lines("abcdef\nx", None, None, 4),
            "1\tab…（这一行太长，只显示了前 4 个字符）\n…（文件共 2 行，这里是第 1–1 行；接着读用 offset=2）"
        );
        let long = "很长的一行".repeat(100);
        assert!(clip_around(&format!("    {long}关键词{long}"), 504).contains("关键词"));
    }
}
