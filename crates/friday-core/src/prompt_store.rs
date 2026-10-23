//! 数据目录里的提示词文件夹（== Swift `Prompts/PromptStore.swift`、Qt `models/PromptStore.cpp`）：谁都能改，改了就热重载；装了 git 就是一个
//! git 仓库，每次变化自动提交一次，设置里能看历史、回滚到任一版本、恢复成自带的。v2.79 第 4 阶段起整个在核心里：
//! - 每秒看一次提示词文件的修改时间和大小（编辑器、智能体怎么写都看得到），变了就重读（prompts::reload），停 1.5 秒再提交（连着改几次只记一次）；
//! - git 在核心的一个线程上一次跑一个（提交、回滚、恢复、读历史）；
//! - 状态变了推 `promptStore.state`（完整快照，带递增的 revision，两端只收比手上新的），实际用的段变了推 `promptStore.promptsChanged`，
//!   applySections 提交完推 `promptStore.committed`；
//! - 找 git 留在两端（macOS 没装命令行工具时 /usr/bin/git 一调就弹安装框，要先问 xcode-select）：start / refresh 时两端直接给，
//!   或者不给、让核心在 git 的队列里请两端找（`promptStore.findGit` 请求），这样先后顺序和原来一样。
//!
//! 自动回滚的判断要用记忆库，留在两端（判出来调 rollback）。每个进程一份。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::events;
use crate::prompt_history::{self, Version};
use crate::prompts;

/// 读完停多久再提交
const COMMIT_DELAY: Duration = Duration::from_millis(1_500);
/// 多久看一次文件
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// 请两端找 git 最多等多久
const FIND_GIT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Default)]
struct Store {
    /// start 一次加一：旧的看文件线程看到不是自己的就停
    generation: u64,
    directory: Option<PathBuf>,
    git: Option<String>,
    /// 跑 git 用的环境变量（两端的 shell 环境；空 = 照核心进程的）
    environment: Vec<(String, String)>,
    history: Vec<Version>,
    problems: Vec<String>,
    kept_local: Vec<String>,
    busy: bool,
    last_error: String,
    signature: BTreeMap<String, String>,
    files: BTreeMap<String, Vec<u8>>,
    /// 到这个时候提交（停 1.5 秒再提交）
    pending_commit: Option<Instant>,
    /// 排着、正在跑的 git 活
    queued: usize,
    /// 快照的版本：每次变化加一
    revision: u64,
}

static STORE: LazyLock<Mutex<Store>> = LazyLock::new(|| Mutex::new(Store::default()));
static IDLE: Condvar = Condvar::new();

type Job = Box<dyn FnOnce() + Send>;

static WORKER: LazyLock<Mutex<Sender<Job>>> = LazyLock::new(|| {
    let (sender, receiver) = mpsc::channel::<Job>();
    std::thread::Builder::new()
        .name("friday-prompt-git".into())
        .spawn(move || {
            for job in receiver {
                job();
                let mut store = STORE.lock().unwrap();
                store.queued = store.queued.saturating_sub(1);
                IDLE.notify_all();
            }
        })
        .expect("起不了 git 的线程");
    Mutex::new(sender)
});

/// 排一件 git 活（一次一个，排在上一个后面）
fn enqueue(job: impl FnOnce() + Send + 'static) {
    STORE.lock().unwrap().queued += 1;
    if WORKER.lock().unwrap().send(Box::new(job)).is_err() {
        STORE.lock().unwrap().queued -= 1;
    }
}

fn version_json(version: &Version) -> Value {
    json!({ "id": version.id, "shortID": version.short_id, "time": version.time, "message": version.message })
}

fn snapshot(store: &Store) -> Value {
    json!({
        "revision": store.revision,
        "directory": store.directory.as_ref().map(|directory| crate::tool_files::slash(directory)),
        "gitAvailable": store.git.is_some(),
        "git": store.git,
        "history": store.history.iter().map(version_json).collect::<Vec<_>>(),
        "problems": store.problems,
        "keptLocal": store.kept_local,
        "busy": store.busy,
        "lastError": store.last_error,
        "version": store.history.first().map(|version| version.short_id.clone()).unwrap_or_default(),
    })
}

/// 改一下状态、推一次快照
fn update(change: impl FnOnce(&mut Store)) {
    let state = {
        let mut store = STORE.lock().unwrap();
        change(&mut store);
        store.revision += 1;
        snapshot(&store)
    };
    events::emit("promptStore.state", state);
}

pub fn state() -> Value {
    snapshot(&STORE.lock().unwrap())
}

// MARK: - 文件

/// 每个提示词文件的修改时间和大小
fn signature(directory: &Path) -> BTreeMap<String, String> {
    prompts::prompt_files(directory)
        .into_iter()
        .map(|name| {
            let info = std::fs::metadata(directory.join(&name)).ok();
            let modified = info
                .as_ref()
                .and_then(|info| info.modified().ok())
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0.0, |duration| duration.as_secs_f64());
            let size = info.map_or(0, |info| info.len());
            (name, format!("{modified}-{size}"))
        })
        .collect()
}

fn read_files(directory: &Path) -> BTreeMap<String, Vec<u8>> {
    prompts::prompt_files(directory)
        .into_iter()
        .map(|name| (name.clone(), std::fs::read(directory.join(name)).unwrap_or_default()))
        .collect()
}

/// 文件变了（修改时间、大小）就重读
fn check_files() {
    let (directory, old) = {
        let store = STORE.lock().unwrap();
        (store.directory.clone(), store.signature.clone())
    };
    let Some(directory) = directory else { return };
    let now = signature(&directory);
    if now == old {
        return;
    }
    STORE.lock().unwrap().signature = now;
    reload_now(true);
}

/// 内容真变了就重读、推一次；commit_later：停 1.5 秒再提交
fn reload_now(commit_later: bool) {
    let Some(directory) = STORE.lock().unwrap().directory.clone() else { return };
    let signature = signature(&directory);
    let files = read_files(&directory);
    {
        let mut store = STORE.lock().unwrap();
        store.signature = signature;
        if files == store.files {
            return;
        }
        store.files = files;
    }
    let keys = prompts::reload();
    let problems = prompts::problems();
    update(|store| {
        store.problems = problems;
        if commit_later {
            store.pending_commit = Some(Instant::now() + COMMIT_DELAY);
        }
    });
    if !keys.is_empty() {
        events::emit("promptStore.promptsChanged", json!({ "keys": keys }));
    }
}

/// 每秒看一次文件；到了提交的时候排一次提交
fn poll(generation: u64) {
    loop {
        std::thread::sleep(POLL_INTERVAL);
        if STORE.lock().unwrap().generation != generation {
            return;
        }
        check_files();
        let due = {
            let mut store = STORE.lock().unwrap();
            let due = store.pending_commit.is_some_and(|at| Instant::now() >= at);
            if due {
                store.pending_commit = None;
            }
            due
        };
        if due {
            enqueue(|| commit(None));
        }
    }
}

// MARK: - git

/// 跑一次 git（带核心里的那组设置）：退出码（起不来、被信号结束是 -1）和输出（标准输出 + 标准错误）
fn run(arguments: &[&str]) -> (i32, String) {
    let (git, directory, environment) = {
        let store = STORE.lock().unwrap();
        (store.git.clone(), store.directory.clone(), store.environment.clone())
    };
    let (Some(git), Some(directory)) = (git, directory) else { return (-1, String::new()) };
    let mut command = Command::new(git);
    command.args(prompt_history::GIT_BASE).args(arguments).current_dir(directory).stdin(Stdio::null());
    if !environment.is_empty() {
        command.env_clear().envs(environment);
    }
    command.env("GIT_TERMINAL_PROMPT", "0").env("GIT_OPTIONAL_LOCKS", "0");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // 不弹控制台窗口（CREATE_NO_WINDOW）
        command.creation_flags(0x0800_0000);
    }
    match command.output() {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text += &String::from_utf8_lossy(&output.stderr);
            (output.status.code().unwrap_or(-1), text)
        }
        Err(error) => (-1, error.to_string()),
    }
}

fn has_repository() -> bool {
    STORE.lock().unwrap().directory.as_ref().is_some_and(|directory| directory.join(".git").exists())
}

fn ensure_repository() -> bool {
    if has_repository() {
        return true;
    }
    let (code, output) = run(&["init", "-q"]);
    if code != 0 {
        update(|store| store.last_error = format!("建 git 仓库没成功：{}", output.trim()));
    }
    code == 0
}

/// 有改动就提交（message None = 按改了哪几段自动写）
fn commit(message: Option<String>) {
    if STORE.lock().unwrap().git.is_none() {
        prompt_history::mark_committed();
        return;
    }
    run(&["add", "-A"]);
    // 没有改动（--quiet 退出码 0）就不提交
    if run(&["diff", "--cached", "--quiet"]).0 != 0 {
        let text = message.unwrap_or_else(prompt_history::describe_changes);
        let (code, output) = run(&["commit", "-q", "-m", &text]);
        update(|store| store.last_error = if code != 0 { format!("提交没成功：{}", output.trim()) } else { String::new() });
    }
    prompt_history::mark_committed();
    load_history();
}

fn load_history() {
    if STORE.lock().unwrap().git.is_none() || !has_repository() {
        return;
    }
    let (code, output) = run(prompt_history::LOG_ARGUMENTS);
    if code != 0 {
        return;
    }
    let history = prompt_history::parse_log(&output);
    update(|store| store.history = history);
}

/// 两端给的 git（Some）或者请两端找（None：在 git 的线程上发 promptStore.findGit 请求，回话是路径或 null）
fn resolve_git(given: Option<Option<String>>) -> Option<String> {
    let git = match given {
        Some(git) => git,
        None => events::request("promptStore.findGit", json!({}), Some(FIND_GIT_TIMEOUT))
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .filter(|git| !git.is_empty()),
    };
    update(|store| store.git = git.clone());
    git
}

// MARK: - 对外

/// 开始看文件、建仓库、提交启动时的变化（两端建好后调一次；再调一次换成新的，测试换文件夹时用）。
/// git：Some(路径或 None) 是两端找好的，None 是请两端找；environment：跑 git 用的环境变量；version：这一版的名字（启动时那次提交的说明里用）
pub fn start(git: Option<Option<String>>, environment: Vec<(String, String)>, version: &str) -> Value {
    let directory = prompts::directory();
    let merge = prompts::last_merge();
    prompt_history::mark_committed();
    let generation = {
        let mut store = STORE.lock().unwrap();
        let generation = store.generation + 1;
        *store = Store {
            generation,
            directory: directory.clone(),
            git: git.clone().flatten(),
            environment,
            problems: prompts::problems(),
            kept_local: merge.kept_local.clone(),
            revision: store.revision + 1,
            queued: store.queued,
            ..Store::default()
        };
        if let Some(directory) = &directory {
            store.signature = signature(directory);
            store.files = read_files(directory);
        }
        generation
    };
    if directory.is_some() {
        std::thread::Builder::new().name("friday-prompt-files".into()).spawn(move || poll(generation)).ok();
        // 启动时的这一次：第一次写入 / 合并了新版本自带的，写清楚；其余写「Friday 没开时改过的提示词」
        let message = prompt_history::startup_message(version, &merge);
        enqueue(move || {
            if resolve_git(git).is_some() && ensure_repository() {
                commit(Some(message));
            }
        });
    }
    let state = state();
    events::emit("promptStore.state", state.clone());
    state
}

/// 重新找 git（装好以后点「重新检查」）、重新读历史
pub fn refresh(git: Option<Option<String>>) {
    if STORE.lock().unwrap().directory.is_none() {
        return;
    }
    enqueue(move || {
        let had = STORE.lock().unwrap().git.is_some();
        if resolve_git(git).is_none() {
            return;
        }
        if had {
            load_history();
        } else if ensure_repository() {
            commit(Some("装好 git 后的第一次提交".into()));
        }
    });
}

/// 先占上 busy（正在回滚、提交时不再接）；占上了返回 true
fn begin(require_git: bool) -> bool {
    let mut store = STORE.lock().unwrap();
    if store.busy || store.directory.is_none() || (require_git && store.git.is_none()) {
        return false;
    }
    store.busy = true;
    store.pending_commit = None;
    true
}

fn finish() {
    update(|store| store.busy = false);
}

/// 回滚到某个版本（git checkout <id> -- .，再提交一次；历史不丢，回滚也能再撤）；message 不给是「回滚到 <时间> 的版本（说明）」
pub fn rollback(id: &str, message: Option<String>) -> bool {
    if id.is_empty() {
        return false;
    }
    let label = {
        let store = STORE.lock().unwrap();
        prompt_history::version_label(&store.history, id)
    };
    if !begin(true) {
        return false;
    }
    update(|_| {});
    let (id, message) = (id.to_string(), message.unwrap_or_else(|| format!("回滚到 {label}")));
    enqueue(move || {
        let (code, output) = run(&["checkout", &id, "--", "."]);
        if code != 0 {
            update(|store| store.last_error = format!("回滚没成功：{}", output.trim()));
        } else {
            reload_now(false);
            commit(Some(message));
        }
        finish();
    });
    true
}

/// 全部换成自带的，再提交一次
pub fn restore_bundled() -> bool {
    if !begin(false) {
        return false;
    }
    update(|_| {});
    enqueue(|| {
        prompts::restore_bundled();
        reload_now(false);
        commit(Some("恢复成自带的提示词".into()));
        finish();
    });
    true
}

/// 改进建议被采用：把这几段写进数据目录的文件，提交成一个版本。返回空 = 好了，否则原因。
/// token 给了的提交完推 promptStore.committed {token, version}（新的版本号，7 位；没装 git 为空）
pub fn apply_sections(sections: &BTreeMap<String, String>, message: &str, token: Option<String>) -> String {
    {
        let store = STORE.lock().unwrap();
        if store.directory.is_none() {
            return "数据目录里没有提示词文件，写不了".into();
        }
        if store.busy {
            return "提示词正在回滚或提交，过会儿再试".into();
        }
    }
    if !begin(false) {
        return "提示词正在回滚或提交，过会儿再试".into();
    }
    update(|_| {});
    let failed: Vec<&str> = sections.iter().filter(|(key, text)| !prompts::set_section(key, text)).map(|(key, _)| key.as_str()).collect();
    let message = message.to_string();
    enqueue(move || {
        reload_now(false);
        commit(Some(message));
        finish();
        if let Some(token) = token {
            let has_git = STORE.lock().unwrap().git.is_some();
            let version = if has_git {
                let (code, output) = run(&["rev-parse", "HEAD"]);
                if code == 0 { output.trim().chars().take(7).collect() } else { String::new() }
            } else {
                String::new()
            };
            events::emit("promptStore.committed", json!({ "token": token, "version": version }));
        }
    });
    if failed.is_empty() { String::new() } else { format!("这几段没写进去：{}", failed.join("、")) }
}

/// 马上看一遍文件、把要提交的提交掉，等 git 跑完（最多 timeout）；返回等到了没有
pub fn settle(timeout: Duration) -> bool {
    check_files();
    let due = STORE.lock().unwrap().pending_commit.take().is_some();
    if due {
        enqueue(|| commit(None));
    }
    let deadline = Instant::now() + timeout;
    let mut store = STORE.lock().unwrap();
    while store.queued > 0 {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        store = IDLE.wait_timeout(store, left).unwrap().0;
    }
    true
}

/// 某个版本的全部段（git show 出那个版本的文件，按 prompts::compose 校验）；没装 git、版本不对时为空（在调用的线程上跑 git）
pub fn sections_at(id: &str) -> HashMap<String, String> {
    if STORE.lock().unwrap().git.is_none() || id.is_empty() {
        return HashMap::new();
    }
    let mut contents = Vec::new();
    for name in prompts::file_names() {
        let (code, output) = run(&["show", &format!("{id}:{name}")]);
        // 那个版本还没有这个文件（之后才加的）：不算错，缺的段用自带的
        if code == 0 {
            contents.push(output);
        } else if output.contains("bad revision") || output.contains("invalid object name") {
            return HashMap::new();
        }
    }
    prompts::compose(&contents).into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git() -> Option<String> {
        Command::new("git").arg("--version").output().ok().filter(|output| output.status.success()).map(|_| "git".to_string())
    }

    fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(name, text)| (name.to_string(), text.to_string())).collect()
    }

    /// == Swift 自检「提示词的版本历史」、Qt TestUtil::promptHistory：第一次提交；改了文件自动提交；回滚；改一段；恢复成自带的
    #[test]
    fn history() {
        let Some(git) = git() else { return };
        let _lock = prompts::TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let _events = events::TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("friday-core-prompt-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bundled = files(&[("prompts-main.md", "@@ resume\n（上一步因内部错误中断：{{reason}}。）\n\n@@ other\n别的\n")]);
        prompts::setup(bundled, Some(dir.clone()), Vec::new());
        let changed = std::sync::Arc::new(Mutex::new(Vec::<Value>::new()));
        let seen = changed.clone();
        events::set_handler(Some(Box::new(move |text: &str| {
            let message: Value = serde_json::from_str(text).unwrap();
            if message["event"] == "promptStore.promptsChanged" {
                seen.lock().unwrap().push(message["data"]["keys"].clone());
            }
        })));
        start(Some(Some(git)), Vec::new(), "测试版");
        assert!(settle(Duration::from_secs(20)));
        let messages = || STORE.lock().unwrap().history.iter().map(|version| version.message.clone()).collect::<Vec<_>>();
        assert_eq!(messages(), ["Friday 测试版 自带的提示词"]);
        assert_eq!(state()["version"], json!(STORE.lock().unwrap().history[0].short_id));
        // 改文件：热重载、自动提交
        std::thread::sleep(Duration::from_millis(1_100));
        let path = dir.join("prompts-main.md");
        let content = std::fs::read_to_string(&path).unwrap().replace("（上一步因内部错误中断", "（刚才出错了");
        std::fs::write(&path, content).unwrap();
        assert!(settle(Duration::from_secs(20)));
        assert_eq!(messages()[0], "改了 1 段：resume");
        assert_eq!(changed.lock().unwrap().first(), Some(&json!(["resume"])));
        assert!(prompts::text("resume", &prompts::Args::from([("reason".into(), "x".into())]), None).starts_with("（刚才出错了"));
        // 回滚到第一个版本
        let first = STORE.lock().unwrap().history.last().unwrap().id.clone();
        assert!(rollback(&first, None));
        assert!(settle(Duration::from_secs(20)));
        assert!(!STORE.lock().unwrap().busy);
        assert!(messages()[0].starts_with("回滚到 "), "{:?}", messages());
        assert!(prompts::text("resume", &prompts::Args::from([("reason".into(), "x".into())]), None).starts_with("（上一步因内部错误中断"));
        // 读那个版本的段
        assert_eq!(sections_at(&first).get("resume").map(String::as_str), Some("（上一步因内部错误中断：{{reason}}。）"));
        // 改一段、恢复成自带的
        assert_eq!(apply_sections(&files(&[("other", "改过的")]), "改了别的", None), "");
        assert!(settle(Duration::from_secs(20)));
        assert_eq!(messages()[0], "改了别的");
        assert!(restore_bundled());
        assert!(settle(Duration::from_secs(20)));
        assert_eq!(messages()[0], "恢复成自带的提示词");
        assert_eq!(messages().len(), 5);
        assert_eq!(STORE.lock().unwrap().last_error, "");
        // 停掉看文件的线程
        STORE.lock().unwrap().generation += 1;
        events::set_handler(None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
