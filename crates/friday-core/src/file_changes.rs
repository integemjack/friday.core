//! v2.29 命令（run_shell）改了、建了、删了哪些文本文件，加了几行、删了几行（== Swift `Agent/FileChanges.swift`、Qt `agent/FileChanges.cpp`）。
//! 执行前后各看一遍：工作目录（递归，跳过隐藏目录和 node_modules、build 这类生成的目录）+ 命令里写到的路径（工作目录外的文件、
//! 目录、还没有的文件也看）。按修改时间和大小找出变了的文件，执行前只留每行的指纹，不留全文；没变的文件指纹按路径缓存，下一条命令不用重读。
//! 不算的：二进制 / 不是 UTF-8 的、超过 1 MB 的；工作目录太大（超过 2 万个文件或执行前 1.5 秒没看完）、是家目录或根目录时
//! 只看命令里写到的路径；同一时间 write_file / edit_file 写的（并行的子智能体）记在它们自己的卡片上，这里跳过。
//! 路径一律用 / 分隔（Windows 上把 \ 换成 /、不分大小写，命令里的 \ 不当转义），和两端传进来的写法一致。

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::line_diff;

pub const MAX_FILES: usize = 20_000;
pub const MAX_FILE_BYTES: u64 = 1 << 20;
const SCAN_BUDGET: Duration = Duration::from_millis(1_500);
const TOO_BIG_INTERVAL: Duration = Duration::from_secs(60 * 60);
const MAX_MENTIONED: usize = 64;
const MAX_CACHED: usize = 200_000;
/// 不往里看的目录：隐藏的（.git 等）之外，常见的依赖和生成目录（编译、装依赖时整片地变，不算改动）
const SKIPPED_DIRECTORIES: [&str; 10] =
    ["node_modules", "DerivedData", "build", "target", "dist", "__pycache__", "Pods", "venv", "CMakeFiles", "_deps"];

/// 一个文件当时的样子：修改时间（毫秒）、大小、每行的指纹（二进制、太大、读不了的没有指纹，不算行数）
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    modified: i64,
    size: u64,
    lines: Option<Vec<u64>>,
}

#[derive(Debug, Default)]
struct Scope {
    /// 递归看的目录（空 = 只看 files）
    root: String,
    /// 目录没看完（太大、超时）就不算这个目录
    complete: bool,
    /// 单独看的文件（命令里写到的、在目录外面的）
    files: Vec<String>,
    entries: BTreeMap<String, Entry>,
}

/// 执行前看到的样子
#[derive(Debug, Default)]
pub struct Snapshot {
    scopes: Vec<Scope>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Created,
    Modified,
    Deleted,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Created => "created",
            Kind::Modified => "modified",
            Kind::Deleted => "deleted",
        }
    }
}

/// 命令动了的一个文件（会话改动记基线用）：before 是执行前每行的指纹（新建的、二进制的没有）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    pub path: String,
    pub kind: Kind,
    pub before: Option<Vec<u64>>,
}

#[derive(Default)]
struct Shared {
    /// 看过的文件（路径 → 当时的样子）：修改时间和大小没变的下次直接用这里的指纹
    cache: HashMap<String, Entry>,
    /// 太大的目录记 1 小时，这期间的命令不再整片地看它
    too_big: HashMap<String, Instant>,
    /// write_file / edit_file 写完时的修改时间
    written: HashMap<String, i64>,
}

static SHARED: LazyLock<Mutex<Shared>> = LazyLock::new(|| Mutex::new(Shared::default()));

fn case_insensitive() -> bool {
    cfg!(windows)
}

fn same(a: &str, b: &str) -> bool {
    if case_insensitive() { a.to_lowercase() == b.to_lowercase() } else { a == b }
}

/// 绝对路径：/ 开头，Windows 上还有 C:/ 和 //server
fn is_absolute(path: &str) -> bool {
    path.starts_with('/') || (cfg!(windows) && path.len() >= 2 && path.as_bytes()[1] == b':' && path.as_bytes()[0].is_ascii_alphabetic())
}

/// 按字面整理：去掉多余的 /、「.」，按「..」往上，去掉结尾的 /（根目录保留）
pub fn clean(path: &str) -> String {
    let (prefix, rest) = if cfg!(windows) && path.len() >= 2 && path.as_bytes()[1] == b':' {
        (path[..2].to_string(), &path[2..])
    } else {
        (String::new(), path)
    };
    let absolute = rest.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    match (absolute, joined.is_empty()) {
        (true, _) => format!("{prefix}/{joined}"),
        (false, true) => {
            if prefix.is_empty() {
                ".".into()
            } else {
                prefix
            }
        }
        (false, false) => format!("{prefix}{joined}"),
    }
}

/// ~ 展开，相对路径按 base，去掉 . 和 ..（不解析符号链接）
pub fn resolve(path: &str, base: &str, home: &str) -> String {
    let mut expanded = if path == "~" {
        home.to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("{}/{rest}", home.trim_end_matches('/'))
    } else {
        path.to_string()
    };
    if cfg!(windows) {
        expanded = expanded.replace('\\', "/");
    }
    if is_absolute(&expanded) { clean(&expanded) } else { clean(&format!("{}/{expanded}", base.replace('\\', "/"))) }
}

fn is_inside(path: &str, root: &str) -> bool {
    if root.is_empty() {
        return false;
    }
    if same(path, root) {
        return true;
    }
    let prefix = if root.ends_with('/') { root.to_string() } else { format!("{root}/") };
    if case_insensitive() { path.to_lowercase().starts_with(&prefix.to_lowercase()) } else { path.starts_with(&prefix) }
}

enum FileKind {
    File,
    Directory,
    Other,
}

fn millis(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i64,
        Err(error) => -(error.duration().as_millis() as i64),
    }
}

/// 不跟随符号链接
fn stat(path: &str) -> Option<(FileKind, Entry)> {
    let info = fs::symlink_metadata(path).ok()?;
    let kind = if info.is_file() {
        FileKind::File
    } else if info.is_dir() {
        FileKind::Directory
    } else {
        FileKind::Other
    };
    Some((kind, Entry { modified: info.modified().map(millis).unwrap_or(0), size: info.len(), lines: None }))
}

fn directory_exists(path: &str) -> bool {
    matches!(stat(path), Some((FileKind::Directory, _)))
}

/// 递归列出 root 下的文件（只取修改时间和大小，不跟符号链接）；文件超过上限或超时返回 false
fn walk(root: &str, out: &mut BTreeMap<String, Entry>, files: &mut usize, deadline: Option<Instant>) -> bool {
    let Ok(entries) = fs::read_dir(root) else { return true };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = if root.ends_with('/') { format!("{root}{name}") } else { format!("{root}/{name}") };
        let Some((kind, info)) = stat(&path) else { continue };
        match kind {
            FileKind::Directory => {
                if name.starts_with('.') || SKIPPED_DIRECTORIES.contains(&name.as_str()) {
                    continue;
                }
                if !walk(&path, out, files, deadline) {
                    return false;
                }
            }
            FileKind::File => {
                *files += 1;
                if *files > MAX_FILES {
                    return false;
                }
                out.insert(path, info);
            }
            FileKind::Other => {}
        }
        if deadline.is_some_and(|deadline| Instant::now() > deadline) {
            return false;
        }
    }
    true
}

/// 每行的指纹；二进制（有 NUL）、不是 UTF-8、太大、读不了的为 None
fn read_lines(path: &str, size: u64) -> Option<Vec<u64>> {
    if size > MAX_FILE_BYTES {
        return None;
    }
    let data = fs::read(path).ok()?;
    if data.len() as u64 > MAX_FILE_BYTES || data.contains(&0) {
        return None;
    }
    let text = String::from_utf8(data).ok()?;
    Some(line_diff::line_hashes(&text))
}

/// 填上指纹：缓存里修改时间和大小一样的直接用，否则读一遍（并记进缓存）
fn filled(path: &str, entry: &Entry) -> Entry {
    if let Some(cached) = SHARED.lock().unwrap().cache.get(path)
        && cached.modified == entry.modified
        && cached.size == entry.size
    {
        return cached.clone();
    }
    let result = Entry { lines: read_lines(path, entry.size), ..entry.clone() };
    let mut shared = SHARED.lock().unwrap();
    if shared.cache.len() >= MAX_CACHED {
        shared.cache.clear();
    }
    shared.cache.insert(path.to_string(), result.clone());
    result
}

fn written_by_tool(path: &str, modified: i64) -> bool {
    SHARED.lock().unwrap().written.get(path) == Some(&modified)
}

/// 家目录、根目录（盘符根）不整片地看
fn too_wide(root: &str, home: &str) -> bool {
    let is_root = root == "/" || (cfg!(windows) && root.len() == 3 && root.ends_with(":/"));
    is_root || (!home.is_empty() && same(root, &clean(&home.replace('\\', "/"))))
}

fn remembered_too_big(root: &str) -> bool {
    SHARED.lock().unwrap().too_big.get(root).is_some_and(|until| Instant::now() < *until)
}

fn mark_too_big(root: &str) {
    SHARED.lock().unwrap().too_big.insert(root.to_string(), Instant::now() + TOO_BIG_INTERVAL);
}

/// 命令里写到的路径（按空白和 ; | & < > ( ) 拆开，引号里的算一个；-x=路径 取等号后面；~/ 换成家目录；相对路径按 base 解析）
pub fn mentioned_paths(command: &str, base: &str, home: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;
    let mut chars = command.chars();
    while let Some(ch) = chars.next() {
        if let Some(open) = quote {
            if ch == open {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            in_token = true;
        } else if ch.is_whitespace() || ";|&<>()".contains(ch) {
            if in_token {
                tokens.push(std::mem::take(&mut current));
            }
            current.clear();
            in_token = false;
        } else if ch == '\\' && !cfg!(windows) {
            // shell 的转义（Windows 上 \ 是路径分隔符）
            if let Some(next) = chars.next() {
                current.push(next);
                in_token = true;
            }
        } else {
            current.push(ch);
            in_token = true;
        }
    }
    if in_token {
        tokens.push(current);
    }
    let mut paths: Vec<String> = Vec::new();
    for mut token in tokens {
        if token.starts_with('-') {
            let Some(equals) = token.find('=') else { continue };
            token = token[equals + 1..].to_string();
        }
        if token.is_empty() || token.contains("://") || token.contains(['*', '?', '$', '`', '\n']) {
            continue;
        }
        let path = resolve(&token, base, home);
        if !paths.contains(&path) {
            paths.push(path);
        }
        if paths.len() >= MAX_MENTIONED {
            break;
        }
    }
    paths
}

/// 执行前看一遍 command 会动到的文件；home：家目录（不整片地看），current：没有工作目录时相对路径按它解析
pub fn before(command: &str, working_directory: &str, home: &str, current: &str) -> Snapshot {
    let mut snapshot = Snapshot::default();
    let deadline = Instant::now() + SCAN_BUDGET;
    let mut files = 0;
    // 一个目录整片地看：列文件、填指纹，超出上限或超时就不算这个目录（记 1 小时）
    let mut scan_root = |root: &str, snapshot: &mut Snapshot| -> bool {
        let mut scope = Scope { root: root.to_string(), ..Scope::default() };
        if !(too_wide(root, home) || remembered_too_big(root)) {
            scope.complete = walk(root, &mut scope.entries, &mut files, Some(deadline));
            if scope.complete {
                for (path, entry) in scope.entries.iter_mut() {
                    *entry = filled(path, entry);
                    if Instant::now() > deadline {
                        scope.complete = false;
                        break;
                    }
                }
            }
            if !scope.complete {
                mark_too_big(root);
            }
        }
        if !scope.complete {
            scope.entries.clear();
        }
        let complete = scope.complete;
        snapshot.scopes.push(scope);
        complete
    };
    let cwd = if working_directory.is_empty() { String::new() } else { resolve(working_directory, "/", home) };
    let cwd_covered = !cwd.is_empty() && directory_exists(&cwd) && scan_root(&cwd, &mut snapshot);
    let mut singles = Scope { complete: true, ..Scope::default() };
    for path in mentioned_paths(command, if cwd.is_empty() { current } else { &cwd }, home) {
        if cwd_covered && is_inside(&path, &cwd) {
            continue;
        }
        if snapshot.scopes.iter().any(|scope| scope.complete && is_inside(&path, &scope.root)) {
            continue;
        }
        match stat(&path) {
            None => {
                // 还没有的：执行后有了算新建
                let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
                if directory_exists(if parent.is_empty() { "/" } else { parent }) {
                    singles.files.push(path);
                }
            }
            Some((FileKind::Directory, _)) => {
                scan_root(&path, &mut snapshot);
            }
            Some((FileKind::File, entry)) => {
                let entry = filled(&path, &entry);
                singles.entries.insert(path.clone(), entry);
                singles.files.push(path);
            }
            Some((FileKind::Other, _)) => {}
        }
    }
    if !singles.files.is_empty() {
        snapshot.scopes.push(singles);
    }
    snapshot
}

/// 执行后和执行前比：变了的文本文件按行比，新建的全算加、删掉的全算删；一个文本文件都没变的 counts 是 None。changed 记内容真变了的（含二进制）
pub fn after(snapshot: &Snapshot) -> (Option<line_diff::Counts>, Vec<Changed>) {
    let mut sum = line_diff::Counts { added: 0, removed: 0 };
    let mut any = false;
    let mut changed = Vec::new();
    for scope in snapshot.scopes.iter().filter(|scope| scope.complete) {
        let mut now = BTreeMap::new();
        if !scope.root.is_empty() {
            let mut files = 0;
            if !walk(&scope.root, &mut now, &mut files, None) {
                continue;
            }
        }
        for path in &scope.files {
            if let Some((FileKind::File, entry)) = stat(path) {
                now.insert(path.clone(), entry);
            }
        }
        for (path, old) in &scope.entries {
            let Some(current) = now.remove(path) else {
                // 删掉了
                SHARED.lock().unwrap().cache.remove(path);
                changed.push(Changed { path: path.clone(), kind: Kind::Deleted, before: old.lines.clone() });
                if let Some(lines) = &old.lines {
                    sum.removed += lines.len();
                    any = true;
                }
                continue;
            };
            if current.modified == old.modified && current.size == old.size {
                continue;
            }
            if written_by_tool(path, current.modified) {
                continue;
            }
            let fresh = filled(path, &current);
            if old.lines.is_none() || fresh.lines.is_none() || old.lines != fresh.lines {
                changed.push(Changed { path: path.clone(), kind: Kind::Modified, before: old.lines.clone() });
            }
            if let (Some(before), Some(after)) = (&old.lines, &fresh.lines) {
                let counts = line_diff::count(before, after);
                sum.added += counts.added;
                sum.removed += counts.removed;
                any = true;
            }
        }
        // 新建的
        for (path, entry) in now {
            if written_by_tool(&path, entry.modified) {
                continue;
            }
            let lines = filled(&path, &entry).lines;
            changed.push(Changed { path, kind: Kind::Created, before: None });
            if let Some(lines) = lines {
                sum.added += lines.len();
                any = true;
            }
        }
    }
    ((any && (sum.added > 0 || sum.removed > 0)).then_some(sum), changed)
}

/// write_file / edit_file 刚写完 path：同一时间跑着的命令不把它算进去（记的是写完时的修改时间，之后再被改照样算）
pub fn note_written(path: &str, home: &str) {
    let key = resolve(path, "/", home);
    let Some((_, entry)) = stat(&key) else { return };
    let mut shared = SHARED.lock().unwrap();
    if shared.written.len() >= 10_000 {
        shared.written.clear();
    }
    shared.written.insert(key, entry.modified);
}

// MARK: - 句柄（每条命令一个）

static SNAPSHOTS: LazyLock<Mutex<HashMap<u64, Snapshot>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT: AtomicU64 = AtomicU64::new(1);

pub fn open(snapshot: Snapshot) -> u64 {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    SNAPSHOTS.lock().unwrap().insert(id, snapshot);
    id
}

/// 取出（之后句柄就没了）
pub fn take(id: u64) -> Option<Snapshot> {
    SNAPSHOTS.lock().unwrap().remove(&id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("friday-core-changes-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        clean(&dir.to_string_lossy().replace('\\', "/"))
    }

    #[test]
    fn paths() {
        assert_eq!(clean("/a//b/./c/../d/"), "/a/b/d");
        assert_eq!(clean("/.."), "/");
        assert_eq!(clean("a/../.."), "..");
        assert_eq!(resolve("~/x", "/base", "/home/me"), "/home/me/x");
        assert_eq!(resolve("x/../y", "/base", "/home/me"), "/base/y");
        let base = "/w";
        // Windows 上 \ 是路径分隔符、不是 shell 的转义：c\ d.txt 是两个词（见 mentioned_paths）
        let expected: &[&str] = if cfg!(windows) {
            &["/w/cp", "/w/a b.txt", "/w/c", "/w/d.txt", "/w/out.txt", "/w/cat", "/w/a.txt", "/w/wc"]
        } else {
            &["/w/cp", "/w/a b.txt", "/w/c d.txt", "/w/out.txt", "/w/cat", "/w/a.txt", "/w/wc"]
        };
        assert_eq!(mentioned_paths(r#"cp "a b.txt" c\ d.txt -o=out.txt --flag http://x *.rs $HOME; cat ./a.txt|wc"#, base, "/h"), expected);
        assert!(is_inside("/a/b", "/a") && is_inside("/a", "/a") && !is_inside("/ab", "/a"));
        assert!(too_wide("/", "/h") && too_wide("/h", "/h/") && !too_wide("/h/x", "/h"));
    }

    #[test]
    fn counts_changes() {
        let dir = scratch("count");
        fs::write(format!("{dir}/a.txt"), "1\n2\n3\n").unwrap();
        fs::write(format!("{dir}/gone.txt"), "x\ny\n").unwrap();
        fs::create_dir_all(format!("{dir}/node_modules")).unwrap();
        fs::write(format!("{dir}/node_modules/skip.txt"), "z\n").unwrap();
        let snapshot = before("sh run", &dir, "/nonexistent-home", "/");
        std::thread::sleep(Duration::from_millis(20));
        fs::write(format!("{dir}/a.txt"), "1\nTWO\n3\n4\n").unwrap();
        fs::remove_file(format!("{dir}/gone.txt")).unwrap();
        fs::write(format!("{dir}/new.txt"), "n\n").unwrap();
        fs::write(format!("{dir}/node_modules/skip.txt"), "z\nz\n").unwrap();
        let (counts, changed) = after(&snapshot);
        let counts = counts.unwrap();
        assert_eq!((counts.added, counts.removed), (2 + 1, 1 + 2));
        let mut kinds: Vec<(String, &str)> =
            changed.iter().map(|change| (change.path.rsplit('/').next().unwrap().to_string(), change.kind.name())).collect();
        kinds.sort();
        assert_eq!(kinds, [("a.txt".to_string(), "modified"), ("gone.txt".to_string(), "deleted"), ("new.txt".to_string(), "created")]);
        // write_file 刚写的不算进命令里
        let snapshot = before("sh run", &dir, "/nonexistent-home", "/");
        std::thread::sleep(Duration::from_millis(20));
        fs::write(format!("{dir}/a.txt"), "only\n").unwrap();
        note_written(&format!("{dir}/a.txt"), "/nonexistent-home");
        assert_eq!(after(&snapshot), (None, vec![]));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mentioned_outside() {
        let dir = scratch("outside");
        let other = scratch("outside-other");
        fs::write(format!("{other}/o.txt"), "a\n").unwrap();
        let snapshot = before(&format!("echo b >> {other}/o.txt; touch {other}/fresh.txt"), &dir, "/nonexistent-home", "/");
        std::thread::sleep(Duration::from_millis(20));
        fs::write(format!("{other}/o.txt"), "a\nb\n").unwrap();
        fs::write(format!("{other}/fresh.txt"), "f\n").unwrap();
        let (counts, changed) = after(&snapshot);
        assert_eq!(counts.map(|counts| (counts.added, counts.removed)), Some((2, 0)));
        assert_eq!(changed.len(), 2);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&other);
    }
}
