//! v2.11 提示词引擎。提示词集中放在 Friday 仓库的 Friday/Friday/Prompts/prompts-*.md（Swift 打进 App 资源，Qt 编进 qrc），
//! 两端第一次用到时把这些文件的内容交给核心（[`setup`]）。文件格式见 Friday 的 qt/docs/PROMPTS.md：
//! - 「@@ 名字」开一段，到下一个「@@」为止；段首段尾的空行去掉，中间原样；
//! - 「@//」开头的行是注释；
//! - {{参数}} 由调用方给（没给的写成「【缺参数：名字】」）；整行只有一个参数、给的是空串时整行去掉；
//! - {{>名字}} 原样引用另一段（引用的段也按同样的参数展开）；
//! - 平台参数（trash、shell、shellHint、speakResult、toolCodeCwd、scriptCwd、promptsDir）自动带上，调用方给了同名的以调用方为准；
//!   取值两端不同（setup 时给：固定的值，或「取哪一段」）。
//!
//! 找不到的段返回「【缺提示词：名字】」，导出和测试里一眼能看出来。
//!
//! 用的是数据目录里那一份（<数据目录>/Prompts/prompts-*.md），谁都能改（用户、Agent），改了就重新读（两端的 PromptStore 调 reload）：
//! - 第一次用到时没有就把自带的写进去；装了新版本（自带的和 .upstream/ 里上次的不一样）就按段三方合并：
//!   本地没改过的段换成新的，本地改过、新版本没改的留本地的，两边都改了的留本地的、记进 keptLocal（设置里提示）；
//! - 读的时候逐段校验：本地的段用了自带那段没有的参数、引用了不存在的段，就用自带的那段，原因记进 problems；
//!   缺的段、缺的文件也用自带的。数据目录用不了（没有、写不进）时整份用自带的。
//!
//! 评测某个提示词版本时临时换一套段（[`override_sections`]，句柄；两端放在当前线程 / 任务上，调用时带上）。
//! 版本历史、回滚、git 还在两端的 PromptStore 里。每个进程一份，任意线程都能调。
//!
//! 原来是 Swift `Prompts/Prompts.swift` 和 Qt `util/Prompts.cpp` 各写一份。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, RwLock};

use regex::Regex;

pub type Args = HashMap<String, String>;
pub type Sections = HashMap<String, String>;

pub const PLATFORM_KEYS: &[&str] = &["trash", "shell", "shellHint", "speakResult", "toolCodeCwd", "scriptCwd", "promptsDir"];

/// 一个平台参数的取值
#[derive(Debug, Clone, PartialEq)]
pub enum Platform {
    /// 固定的值
    Value(String),
    /// 取这一段展开的结果（跟着段变）
    Section(String),
}

/// 上次合并新版本的结果（第一次写入、合并时才有；没合并过全空）
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MergeReport {
    pub installed: bool,
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub kept_local: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct Snapshot {
    /// 实际用的（数据目录里校验过的段 + 其余自带的）
    sections: Sections,
    problems: Vec<String>,
    platform: Args,
}

#[derive(Debug, Default)]
struct State {
    /// 自带的：文件名 → 内容，段名 → 正文
    bundled_files: BTreeMap<String, String>,
    bundled: Sections,
    dir: Option<PathBuf>,
    platform: Vec<(String, Platform)>,
    merge: MergeReport,
    snapshot: Snapshot,
}

static STATE: LazyLock<RwLock<State>> = LazyLock::new(|| RwLock::new(State::default()));
static OVERRIDES: LazyLock<Mutex<HashMap<u64, Snapshot>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT: AtomicU64 = AtomicU64::new(1);

static LONELY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\{\{\s*([^{}>\s]+)\s*\}\}$").unwrap());
static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{\{\s*(>?)\s*([^{}\s]+)\s*\}\}").unwrap());

/// Swift 的 `.whitespaces`：Unicode Zs 类 + 制表符（不含换行）
fn is_space(c: char) -> bool {
    matches!(c, '\t' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}')
}

fn blank(line: &str) -> bool {
    line.chars().all(is_space)
}

fn normalized(content: &str) -> String {
    content.replace("\r\n", "\n")
}

// MARK: - 格式

/// 解析一份文件的内容：段名 → 正文
pub fn parse(content: &str) -> Sections {
    let mut sections = Sections::new();
    let mut key: Option<String> = None;
    let mut lines: Vec<&str> = Vec::new();
    let mut flush = |key: &Option<String>, lines: &mut Vec<&str>| {
        let Some(key) = key else { return };
        // 段首段尾的空行去掉，中间原样
        let start = lines.iter().position(|line| !blank(line)).unwrap_or(lines.len());
        let end = lines.iter().rposition(|line| !blank(line)).map_or(start, |index| index + 1);
        sections.insert(key.clone(), lines[start..end.max(start)].join("\n"));
    };
    for raw in content.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.starts_with("@//") {
            continue;
        }
        if let Some(name) = line.strip_prefix("@@ ") {
            flush(&key, &mut lines);
            key = Some(name.trim_matches(is_space).to_string());
            lines = Vec::new();
            continue;
        }
        if key.is_some() {
            lines.push(line);
        }
    }
    flush(&key, &mut lines);
    sections
}

/// 按给定的段展开
pub fn render(sections: &Sections, key: &str, args: &Args) -> String {
    render_at(sections, key, args, 0)
}

fn render_at(sections: &Sections, key: &str, args: &Args, depth: usize) -> String {
    let Some(section) = sections.get(key).filter(|_| depth <= 8) else { return format!("【缺提示词：{key}】") };
    // 整行只有一个参数、给的是空串：整行去掉（可有可无的一条）
    let text = section
        .split('\n')
        .filter(|line| match LONELY.captures(line) {
            Some(captures) => args.get(&captures[1]).is_none_or(|value| !value.is_empty()),
            None => true,
        })
        .collect::<Vec<_>>()
        .join("\n");
    // 一遍替换：参数的值里出现 {{…}} 也不会再被展开
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for captures in PLACEHOLDER.captures_iter(&text) {
        let whole = captures.get(0).unwrap();
        out.push_str(&text[at..whole.start()]);
        let name = &captures[2];
        if captures[1].is_empty() {
            match args.get(name) {
                Some(value) => out.push_str(value),
                None => out.push_str(&format!("【缺参数：{name}】")),
            }
        } else {
            out.push_str(&render_at(sections, name, args, depth + 1));
        }
        at = whole.end();
    }
    out.push_str(&text[at..]);
    out
}

/// 文字里用到的参数名（includes = false）或引用的段名（includes = true）
fn placeholders(text: &str, includes: bool) -> Vec<String> {
    PLACEHOLDER.captures_iter(text).filter(|captures| captures[1].is_empty() != includes).map(|captures| captures[2].to_string()).collect()
}

fn platform_args(template: &[(String, Platform)], sections: &Sections) -> Args {
    template
        .iter()
        .map(|(name, value)| {
            let text = match value {
                Platform::Value(text) => text.clone(),
                Platform::Section(key) => render(sections, key, &Args::new()),
            };
            (name.clone(), text)
        })
        .collect()
}

/// 自带的段 + 校验过的本地段（校验不过的用自带的，原因记进 problems）
fn validated(bundled: &Sections, local: &Sections, template: &[(String, Platform)]) -> Snapshot {
    let mut snapshot = Snapshot { sections: bundled.clone(), problems: Vec::new(), platform: Args::new() };
    let known: BTreeSet<&String> = bundled.keys().chain(local.keys()).collect();
    let mut keys: Vec<&String> = local.keys().collect();
    keys.sort();
    for key in keys {
        let text = &local[key];
        let mut reasons: Vec<String> = Vec::new();
        for name in placeholders(text, true) {
            if !known.contains(&name) {
                reasons.push(format!("引用了不存在的段 {{{{>{name}}}}}"));
            }
        }
        if let Some(source) = bundled.get(key) {
            // 代码只给自带那段用到的参数（和平台参数）：本地多用的展开不了
            let allowed: BTreeSet<String> =
                placeholders(source, false).into_iter().chain(PLATFORM_KEYS.iter().map(|key| key.to_string())).collect();
            for name in placeholders(text, false) {
                if !allowed.contains(&name) {
                    reasons.push(format!("用了没有的参数 {{{{{name}}}}}"));
                }
            }
        }
        if reasons.is_empty() {
            snapshot.sections.insert(key.clone(), text.clone());
        } else {
            let mut unique: Vec<String> = Vec::new();
            for reason in reasons {
                if !unique.contains(&reason) {
                    unique.push(reason);
                }
            }
            snapshot.problems.push(format!("{key}：{}，先用自带的", unique.join("、")));
        }
    }
    snapshot.platform = platform_args(template, &snapshot.sections);
    snapshot
}

/// 读数据目录里的文件，逐段校验，校验不过的用自带的
fn build(state: &State) -> Snapshot {
    let mut local = Sections::new();
    if let Some(dir) = &state.dir {
        for name in prompt_files(dir) {
            if let Some(content) = read_file(&dir.join(name)) {
                local.extend(parse(&content));
            }
        }
    }
    validated(&state.bundled, &local, &state.platform)
}

// MARK: - 文件里的段（原文，带注释）

/// 一段的原文：「@@ 名字」那行到下一个「@@」之前（含注释和空行）
#[derive(Debug, Clone)]
struct Block {
    key: String,
    raw: String,
}

#[derive(Debug, Clone, Default)]
struct Blocks {
    /// 第一个「@@」之前的（文件说明）
    head: String,
    blocks: Vec<Block>,
}

fn split_blocks(content: &str) -> Blocks {
    let mut result = Blocks::default();
    let mut current: Vec<String> = Vec::new();
    let mut key = String::new();
    let flush = |result: &mut Blocks, current: &mut Vec<String>, key: &str| {
        let raw = if current.is_empty() { String::new() } else { current.join("\n") + "\n" };
        if key.is_empty() {
            result.head = raw;
        } else {
            result.blocks.push(Block { key: key.to_string(), raw });
        }
        current.clear();
    };
    let content = normalized(content);
    let mut lines: Vec<&str> = content.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    for line in lines {
        if let Some(name) = line.strip_prefix("@@ ") {
            flush(&mut result, &mut current, &key);
            key = name.trim_matches(is_space).to_string();
        }
        current.push(line.to_string());
    }
    flush(&mut result, &mut current, &key);
    result
}

fn join_blocks(blocks: &Blocks) -> String {
    blocks.head.clone() + &blocks.blocks.iter().map(|block| block.raw.as_str()).collect::<String>()
}

/// 一段的正文（去掉注释、首尾空行）：比较两边改没改按它
fn block_text(block: &Block) -> String {
    parse(&block.raw).remove(&block.key).unwrap_or_default()
}

/// 按段三方合并：base 是上次自带的，incoming 是这次自带的，local 是数据目录里的。
/// 顺序按 incoming；本地没改过的换成新的，本地改过、新版本没改的留本地的，两边都改了留本地的（记进 keptLocal）；
/// 只有本地有的段：本地加的留着，新版本删掉的（本地也没改）跟着删
fn merge3(base: Option<&Blocks>, incoming: &Blocks, local: &Blocks, report: &mut MergeReport) -> String {
    let mut result = Blocks { head: incoming.head.clone(), blocks: Vec::new() };
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let find = |blocks: &Blocks, key: &str| blocks.blocks.iter().find(|block| block.key == key).cloned();
    for next in &incoming.blocks {
        seen.insert(&next.key);
        let Some(mine) = find(local, &next.key) else {
            if base.is_none_or(|base| find(base, &next.key).is_none()) {
                report.added.push(next.key.clone());
            }
            result.blocks.push(next.clone());
            continue;
        };
        let (mine_text, next_text) = (block_text(&mine), block_text(next));
        let Some(base) = base else {
            // 不知道上次自带的是什么（.upstream 丢了）：不一样的一律当本地改过的
            result.blocks.push(if mine_text == next_text { next.clone() } else { mine });
            continue;
        };
        let Some(old) = find(base, &next.key) else {
            // 新版本加的段，本地也有同名的（本地自己加的）
            if mine_text != next_text {
                report.kept_local.push(next.key.clone());
            }
            result.blocks.push(if mine_text == next_text { next.clone() } else { mine });
            continue;
        };
        let old_text = block_text(&old);
        if mine_text == old_text {
            if next_text != old_text {
                report.updated.push(next.key.clone());
            }
            result.blocks.push(next.clone());
        } else if next_text == old_text || mine_text == next_text {
            result.blocks.push(mine);
        } else {
            report.kept_local.push(next.key.clone());
            result.blocks.push(mine);
        }
    }
    for mine in &local.blocks {
        if seen.contains(mine.key.as_str()) {
            continue;
        }
        // 新版本删掉了、本地也没改：跟着删
        if let Some(old) = base.and_then(|base| find(base, &mine.key))
            && block_text(&old) == block_text(mine)
        {
            continue;
        }
        result.blocks.push(mine.clone());
    }
    join_blocks(&result)
}

/// 第一次写入，或按段合并新版本自带的
fn install_or_merge(state: &mut State) {
    let Some(dir) = state.dir.clone() else { return };
    let upstream = dir.join(".upstream");
    if std::fs::create_dir_all(&upstream).is_err() {
        state.dir = None;
        return;
    }
    let first_time = prompt_files(&dir).is_empty();
    state.merge.installed = first_time;
    for (name, incoming) in state.bundled_files.clone() {
        let local_path = dir.join(&name);
        let base_path = upstream.join(&name);
        let base = read_file(&base_path);
        let Some(local) = read_file(&local_path) else {
            write_file(&local_path, &incoming);
            write_file(&base_path, &incoming);
            if !first_time {
                state.merge.added.extend(split_blocks(&incoming).blocks.into_iter().map(|block| block.key));
            }
            continue;
        };
        if base.as_deref() == Some(incoming.as_str()) {
            continue; // 自带的没变
        }
        if local != incoming {
            let base_blocks = base.as_deref().map(split_blocks);
            let merged = merge3(base_blocks.as_ref(), &split_blocks(&incoming), &split_blocks(&local), &mut state.merge);
            if merged != local {
                write_file(&local_path, &merged);
            }
        }
        write_file(&base_path, &incoming);
    }
    // 版本历史只记 prompts-*.md：基准不进 git
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        write_file(&ignore, ".upstream/\n");
    }
}

/// 文件夹里的 prompts-*.md（按名字排）
pub fn prompt_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| entries.filter_map(|entry| entry.ok()?.file_name().into_string().ok()).collect())
        .unwrap_or_default();
    names.retain(|name| name.starts_with("prompts-") && name.ends_with(".md"));
    names.sort();
    names
}

fn read_file(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|data| normalized(&String::from_utf8_lossy(&data)))
}

fn write_file(path: &Path, content: &str) -> bool {
    crate::writer::write_now(path, content.as_bytes()).is_ok()
}

// MARK: - 对外

/// 准备好（两端第一次用到时调；测试换文件夹时再调一次）：自带的文件（文件名 → 内容）、数据目录里的提示词文件夹（None = 用不了，全用自带的）、
/// 平台参数的取值。写入 / 合并数据目录里的、读一遍
pub fn setup(bundled_files: BTreeMap<String, String>, dir: Option<PathBuf>, platform: Vec<(String, Platform)>) {
    let mut bundled = Sections::new();
    for content in bundled_files.values() {
        bundled.extend(parse(content));
    }
    let mut state = State { bundled_files, bundled, dir, platform, merge: MergeReport::default(), snapshot: Snapshot::default() };
    install_or_merge(&mut state);
    state.snapshot = build(&state);
    *STATE.write().unwrap() = state;
}

fn snapshot(override_id: Option<u64>) -> Snapshot {
    if let Some(id) = override_id
        && let Some(snapshot) = OVERRIDES.lock().unwrap().get(&id)
    {
        return snapshot.clone();
    }
    STATE.read().unwrap().snapshot.clone()
}

fn with_snapshot<T>(override_id: Option<u64>, body: impl FnOnce(&Snapshot) -> T) -> T {
    if let Some(id) = override_id
        && let Some(snapshot) = OVERRIDES.lock().unwrap().get(&id)
    {
        return body(snapshot);
    }
    body(&STATE.read().unwrap().snapshot)
}

/// 按名字取一段，展开引用和参数（平台参数自动带上，调用方给了同名的以调用方为准）
pub fn text(key: &str, args: &Args, override_id: Option<u64>) -> String {
    with_snapshot(override_id, |snapshot| {
        let mut all = snapshot.platform.clone();
        all.extend(args.iter().map(|(name, value)| (name.clone(), value.clone())));
        render(&snapshot.sections, key, &all)
    })
}

pub fn contains(key: &str, override_id: Option<u64>) -> bool {
    with_snapshot(override_id, |snapshot| snapshot.sections.contains_key(key))
}

/// 现在实际用的全部段（段名 → 原文）
pub fn sections(override_id: Option<u64>) -> Sections {
    snapshot(override_id).sections
}

/// 自带的那一段（不看数据目录；「恢复默认」、设置页对比用）
pub fn bundled_text(key: &str, args: &Args) -> String {
    let state = STATE.read().unwrap();
    let mut all = platform_args(&state.platform, &state.bundled);
    all.extend(args.iter().map(|(name, value)| (name.clone(), value.clone())));
    render(&state.bundled, key, &all)
}

/// 自带的那一段的原文（参数没展开）
pub fn bundled_source(key: &str) -> String {
    STATE.read().unwrap().bundled.get(key).cloned().unwrap_or_default()
}

pub fn directory() -> Option<PathBuf> {
    STATE.read().unwrap().dir.clone()
}

pub fn file_names() -> Vec<String> {
    STATE.read().unwrap().bundled_files.keys().cloned().collect()
}

pub fn last_merge() -> MergeReport {
    STATE.read().unwrap().merge.clone()
}

pub fn problems() -> Vec<String> {
    STATE.read().unwrap().snapshot.problems.clone()
}

/// 自带的这份文件里有哪些段（按文件里的顺序）
pub fn bundled_keys(file_name: &str) -> Vec<String> {
    let content = STATE.read().unwrap().bundled_files.get(file_name).cloned().unwrap_or_default();
    split_blocks(&content).blocks.into_iter().map(|block| block.key).filter(|key| !key.is_empty()).collect()
}

/// 把 key 这段换成 text 行不行（同读数据目录时的校验）：行返回空，不行返回原因
pub fn check_section(key: &str, text: &str) -> String {
    if text.trim().is_empty() {
        return "是空的".to_string();
    }
    let now = sections(None);
    let bundled = bundled_source(key);
    if !now.contains_key(key) && bundled.is_empty() {
        return "没有这一段".to_string();
    }
    let mut reasons: Vec<String> = Vec::new();
    for name in placeholders(text, true) {
        if !now.contains_key(&name) {
            reasons.push(format!("引用了不存在的段 {{{{>{name}}}}}"));
        }
    }
    let source = if bundled.is_empty() { now.get(key).cloned().unwrap_or_default() } else { bundled };
    let allowed: BTreeSet<String> =
        placeholders(&source, false).into_iter().chain(PLATFORM_KEYS.iter().map(|key| key.to_string())).collect();
    for name in placeholders(text, false) {
        if !allowed.contains(&name) {
            reasons.push(format!("用了没有的参数 {{{{{name}}}}}"));
        }
    }
    if text.contains("\n@@") || text.starts_with("@@") {
        reasons.push("里面有「@@」开头的行".to_string());
    }
    let mut unique: Vec<String> = Vec::new();
    for reason in reasons {
        if !unique.contains(&reason) {
            unique.push(reason);
        }
    }
    unique.join("、")
}

/// 重新读数据目录里的文件（文件改了时 PromptStore 调）；返回内容变了的段名
pub fn reload() -> Vec<String> {
    let mut state = STATE.write().unwrap();
    let before = std::mem::take(&mut state.snapshot.sections);
    let after = build(&state);
    let mut changed: Vec<String> =
        before.keys().chain(after.sections.keys()).filter(|key| before.get(*key) != after.sections.get(*key)).cloned().collect();
    changed.sort();
    changed.dedup();
    state.snapshot = after;
    changed
}

/// 把某一段的内容换成 text（写回它所在的文件，保留那段的注释），然后重新读；设置里改系统提示词用
pub fn set_section(key: &str, text: &str) -> bool {
    let (dir, bundled_files) = {
        let state = STATE.read().unwrap();
        (state.dir.clone(), state.bundled_files.clone())
    };
    let Some(dir) = dir else { return false };
    // 在哪个文件：本地文件里有就是它，否则按自带的
    let has = |content: &str| split_blocks(content).blocks.iter().any(|block| block.key == key);
    let target = prompt_files(&dir)
        .into_iter()
        .rev()
        .find(|name| read_file(&dir.join(name)).is_some_and(|content| has(&content)))
        .or_else(|| bundled_files.iter().find(|(_, content)| has(content)).map(|(name, _)| name.clone()));
    let Some(target) = target else { return false };
    let path = dir.join(&target);
    let mut blocks = split_blocks(&read_file(&path).or_else(|| bundled_files.get(&target).cloned()).unwrap_or_default());
    // 留着这段的注释，正文换掉；段后空一行
    let index = blocks.blocks.iter().position(|block| block.key == key);
    let comments: Vec<String> = index
        .map(|index| blocks.blocks[index].raw.split('\n').filter(|line| line.starts_with("@//")).map(str::to_string).collect())
        .unwrap_or_default();
    let mut raw = format!("@@ {key}\n");
    for line in comments {
        raw.push_str(&line);
        raw.push('\n');
    }
    raw.push_str(normalized(text).trim());
    raw.push_str("\n\n");
    match index {
        Some(index) => blocks.blocks[index].raw = raw,
        None => blocks.blocks.push(Block { key: key.to_string(), raw }),
    }
    if !write_file(&path, &join_blocks(&blocks)) {
        return false;
    }
    reload();
    true
}

/// 把数据目录里的文件全换成自带的（「恢复成自带的」），然后重新读
pub fn restore_bundled() -> bool {
    let (dir, bundled_files) = {
        let state = STATE.read().unwrap();
        (state.dir.clone(), state.bundled_files.clone())
    };
    let Some(dir) = dir else { return false };
    let mut ok = true;
    for (name, content) in &bundled_files {
        ok = write_file(&dir.join(name), content) && ok;
        ok = write_file(&dir.join(".upstream").join(name), content) && ok;
    }
    reload();
    ok
}

/// 评测某个版本：自带的段 + 这几份文件里校验过的段（同 text 用的那套规则）
pub fn compose(files: &[String]) -> Sections {
    let state = STATE.read().unwrap();
    let mut local = Sections::new();
    for content in files {
        local.extend(parse(content));
    }
    validated(&state.bundled, &local, &state.platform).sections
}

/// 临时换成这一套段（text、sections、contains 带上返回的句柄就用它；平台参数按它重算）。用完 [`release`]
pub fn override_sections(sections: Sections) -> u64 {
    let platform = platform_args(&STATE.read().unwrap().platform, &sections);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    OVERRIDES.lock().unwrap().insert(id, Snapshot { sections, problems: Vec::new(), platform });
    id
}

pub fn release(id: u64) {
    OVERRIDES.lock().unwrap().remove(&id);
}

/// 测试里改了全局状态的拿着它轮流跑
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn args(pairs: &[(&str, &str)]) -> Args {
        pairs.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect()
    }

    #[test]
    fn format() {
        let sections =
            parse("文件说明\n@// 注释\n@@ a\n\n第一行 {{x}}\r\n  \n{{y}}\n中间\n\n@@ b\n引用 {{>a}}，缺 {{>nope}}\n@// 段里的注释\n");
        assert_eq!(sections["a"], "第一行 {{x}}\n  \n{{y}}\n中间");
        assert_eq!(sections["b"], "引用 {{>a}}，缺 {{>nope}}");
        // 整行只有一个参数、给的是空串：整行去掉；没给的写缺参数；一遍替换
        assert_eq!(render(&sections, "a", &args(&[("x", "{{y}}"), ("y", "")])), "第一行 {{y}}\n  \n中间");
        assert_eq!(render(&sections, "a", &Args::new()), "第一行 【缺参数：x】\n  \n【缺参数：y】\n中间");
        assert_eq!(render(&sections, "b", &args(&[("x", "1"), ("y", "2")])), "引用 第一行 1\n  \n2\n中间，缺 【缺提示词：nope】");
        assert_eq!(render(&sections, "zzz", &Args::new()), "【缺提示词：zzz】");
        // 引用自己：深度到头就停
        let looping = parse("@@ s\n再来 {{>s}}");
        assert!(render(&looping, "s", &Args::new()).ends_with("【缺提示词：s】"));
    }

    fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect()
    }

    /// 数据目录里的：第一次写进去、校验、改一段、按段三方合并、恢复成自带的、临时换一套段
    #[test]
    fn data_directory() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("friday-core-prompts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let platform = vec![
            ("shell".to_string(), Platform::Value("macOS 的 zsh".to_string())),
            ("trash".to_string(), Platform::Section("platform.trash".to_string())),
        ];
        let v1 = "说明\n@@ a\nA {{x}}，用 {{shell}}，删到{{trash}}\n\n@@ b\nB\n\n@@ gone\n要删的\n\n@@ platform.trash\n废纸篓\n";
        setup(files(&[("prompts-main.md", v1)]), Some(dir.clone()), platform.clone());
        assert!(last_merge().installed);
        assert_eq!(text("a", &args(&[("x", "1")]), None), "A 1，用 macOS 的 zsh，删到废纸篓");
        assert_eq!(text("a", &args(&[("x", "1"), ("shell", "bash")]), None), "A 1，用 bash，删到废纸篓");
        assert!(dir.join(".upstream/prompts-main.md").exists() && dir.join(".gitignore").exists());
        assert_eq!(bundled_keys("prompts-main.md"), ["a", "b", "gone", "platform.trash"]);

        // 本地改：用了没有的参数、引用不存在的段 → 用自带的，记 problems
        let local = std::fs::read_to_string(dir.join("prompts-main.md")).unwrap().replace("B\n", "B 改过 {{nope}} {{>missing}}\n");
        std::fs::write(dir.join("prompts-main.md"), local).unwrap();
        assert_eq!(reload(), Vec::<String>::new(), "校验不过：实际用的没变");
        assert_eq!(problems(), ["b：引用了不存在的段 {{>missing}}、用了没有的参数 {{nope}}，先用自带的"]);
        assert_eq!(check_section("b", "B {{nope}}"), "用了没有的参数 {{nope}}");
        assert_eq!(check_section("b", "  "), "是空的");
        assert_eq!(check_section("zz", "x"), "没有这一段");
        assert_eq!(check_section("b", "@@ c"), "里面有「@@」开头的行");

        // 改一段（保留注释）
        assert!(set_section("b", "B 新的\n"));
        assert_eq!(text("b", &Args::new(), None), "B 新的");
        assert!(problems().is_empty());

        // 新版本：a 改了（本地没改 → 换成新的），b 改了（本地也改了 → 留本地的），gone 删了（本地没改 → 跟着删），加了 c
        let v2 = "说明\n@@ a\nA2 {{x}}\n\n@@ b\nB 版本二\n\n@@ c\nC\n\n@@ platform.trash\n回收站\n";
        setup(files(&[("prompts-main.md", v2)]), Some(dir.clone()), platform.clone());
        let merge = last_merge();
        assert!(!merge.installed);
        assert_eq!(
            (merge.added, merge.updated, merge.kept_local),
            (vec!["c".to_string()], vec!["a".to_string(), "platform.trash".to_string()], vec!["b".to_string()])
        );
        assert_eq!(text("a", &args(&[("x", "1")]), None), "A2 1");
        assert_eq!(text("b", &Args::new(), None), "B 新的");
        assert!(!contains("gone", None) && contains("c", None));

        // 临时换一套段：平台参数按它算
        let mut other = sections(None);
        other.insert("a".to_string(), "换过的 {{trash}}".to_string());
        other.insert("platform.trash".to_string(), "别处".to_string());
        let id = override_sections(other);
        assert_eq!(text("a", &Args::new(), Some(id)), "换过的 别处");
        assert_eq!(text("a", &args(&[("x", "1")]), None), "A2 1");
        release(id);
        assert_eq!(text("a", &args(&[("x", "1")]), Some(id)), "A2 1", "放掉之后按现在的");

        // 恢复成自带的；评测某个版本
        assert!(restore_bundled());
        assert_eq!(text("b", &Args::new(), None), "B 版本二");
        assert_eq!(compose(&["@@ b\n评测的".to_string()])["b"], "评测的");
        assert_eq!(bundled_text("b", &Args::new()), "B 版本二");

        // 数据目录用不了：全用自带的
        setup(files(&[("prompts-main.md", v1)]), None, platform);
        assert_eq!(text("b", &Args::new(), None), "B");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
