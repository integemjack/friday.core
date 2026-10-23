//! run_shell 的规则（== Swift `AgentTools` 的 shellTimeoutSeconds / selfKillRefusal / runShellHint / runShell 的结果、Qt 同名函数）：
//! 超时秒数、会结束 Friday 自己的命令要拒绝、结果后面的提示（sleep 干等、只读查看该用专用工具）、结果的写法。
//! 起进程（Swift 的 zsh -lc、Qt 的 PowerShell / cmd 和代码页解码）、超时和停止时结束进程树留在两端。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use fancy_regex::Regex;

use crate::tool_files;

/// run_shell 这次调用的超时（秒）：timeout_seconds，默认 60、限在 1–600
pub fn timeout_seconds(requested: Option<i64>) -> i64 {
    requested.unwrap_or(60).clamp(1, 600)
}

/// 这条命令会不会结束 Friday 自己（按名字结束 Friday / friday-cli / friday-server / friday-headless，或结束本进程的 PID）：会就返回拒绝的说明
pub fn self_kill_refusal(command: &str, process_name: &str, pid: i64) -> Option<String> {
    // 结束进程的命令（Unix / PowerShell / cmd；CIM、wmic 的 Terminate 也算）
    static KILLER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(?<![\w-])(stop-process|spps|taskkill|tskill|pkill|killall|kill|terminate)(?![\w-])").unwrap());
    // 按名字指向 Friday（可带 .exe、通配符），不算路径里的一段（/Users/x/Friday/qt、E:\Friday\qt）和 friday_tests、friday-llama 这类
    static BY_NAME: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(?<![\w.\\/-])friday(?:-cli|-server|-headless)?(?:\.exe)?(?![\w\\/-])").unwrap());
    if !KILLER.is_match(command).unwrap_or(false) {
        return None;
    }
    let by_pid = Regex::new(&format!(r"(?<!\d){pid}(?!\d)")).ok()?;
    if !BY_NAME.is_match(command).unwrap_or(false) && !by_pid.is_match(command).unwrap_or(false) {
        return None;
    }
    Some(format!(
        "这条命令会结束 Friday 自己（{process_name}，PID {pid}）：你就运行在这个进程里，结束它等于把你、别的子 Agent 和整段对话一起停掉。\
         要释放被占用的文件，先查清是谁占着（多半是别的编译或测试进程）；要停自己启动的测试程序，按启动时拿到的 PID 结束，\
         不要按名字结束 Friday。"
    ))
}

/// 只读查看的提示每段对话已经提了几次
static VIEW_HINTS: LazyLock<Mutex<HashMap<String, usize>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// run_shell 结果后面的提示：sleep 30 秒以上提示等命令的办法；只读查看提示用专用工具（每段对话最多提两次）。
/// 也认 PowerShell 的 Start-Sleep（-Seconds / -Milliseconds）、cmd 的 timeout /t。conversation：对话 id（没有的传空）
pub fn hint(command: &str, conversation: &str) -> String {
    static SLEEP: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(?:^|[;&|\s({])(?:start-sleep|sleep)\s+(?:-(?:seconds|s)\s+)?(\d+)(?![\d.])").unwrap());
    static SLEEP_MS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(?:^|[;&|\s({])start-sleep\s+-(?:milliseconds|m)\s+(\d+)").unwrap());
    static TIMEOUT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)(?:^|[;&|\s({])timeout(?:\.exe)?\s+/t\s+(\d+)").unwrap());
    static SEPARATORS: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"&&|\|\||;|\||\n").unwrap());
    let number = |pattern: &Regex| -> Option<i64> {
        pattern.captures(command).ok().flatten().and_then(|captures| captures.get(1)).and_then(|group| group.as_str().parse().ok())
    };
    let seconds = number(&SLEEP).or_else(|| number(&SLEEP_MS).map(|ms| ms / 1000)).or_else(|| number(&TIMEOUT)).unwrap_or(0);
    if seconds >= 30 {
        return "\n（提示：固定 sleep 干等不划算——要等的命令直接用 run_shell 跑到结束（timeout_seconds 最多 600），或者 run_in_terminal 开着、read_terminal 带 wait_for 等它打出某句话）".into();
    }
    const VIEWERS: [&str; 15] = [
        "cat",
        "head",
        "tail",
        "grep",
        "rg",
        "find",
        "ls",
        "tree",
        "wc",
        "sed",
        "type",
        "findstr",
        "Get-Content",
        "Select-String",
        "Get-ChildItem",
    ];
    let mut viewing = false;
    for segment in SEPARATORS.split(command) {
        let trimmed = segment.trim_matches([' ', '\t']);
        let first = trimmed.split(' ').find(|part| !part.is_empty()).unwrap_or("");
        let word = tool_files::last_component(first);
        if word.is_empty() || word == "cd" {
            continue;
        }
        // sed 只有 -n 打印才算看
        if !VIEWERS.contains(&word.as_str()) || (word == "sed" && !trimmed.contains(" -n")) {
            return String::new();
        }
        viewing = true;
    }
    if !viewing {
        return String::new();
    }
    let mut counts = VIEW_HINTS.lock().unwrap();
    let count = counts.entry(conversation.to_string()).or_insert(0);
    *count += 1;
    if *count > 2 {
        return String::new();
    }
    "\n（提示：看文件、搜内容用 read_file / search_files / find_files 更快：结果带行号，自动跳过构建和依赖目录，不会被输出长度截断）".into()
}

/// 命令的结果：「退出码 N」（exit_code 为 None：超时或被终止）+ 输出（太长的留头尾）
pub fn result(exit_code: Option<i64>, output: &str) -> String {
    let status = exit_code.map_or_else(|| "（超时或被终止）".to_string(), |code| format!("退出码 {code}"));
    format!("{status}\n{}", tool_files::truncate(if output.is_empty() { "（无输出）" } else { output }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// == Swift 自检「结束 Friday 自己的命令」「run_shell 的提示」、Qt TestTools
    #[test]
    fn refusals() {
        let refused = |command: &str| self_kill_refusal(command, "Friday", 4321).is_some();
        assert!(refused("pkill Friday") && refused("killall -9 friday") && refused("taskkill /IM friday.exe /F") && refused("kill 4321"));
        assert!(refused("Stop-Process -Name friday-server") && refused("pkill -f 'friday*'"));
        assert!(
            !refused("pkill friday_tests") && !refused("kill 43210") && !refused("cd /Users/x/Friday/qt && make") && !refused("ls Friday")
        );
        assert!(!refused("pkill -f /Users/x/Friday/qt/build/friday-llama"));
        assert_eq!(timeout_seconds(None), 60);
        assert_eq!(timeout_seconds(Some(0)), 1);
        assert_eq!(timeout_seconds(Some(9_000)), 600);
    }

    #[test]
    fn hints() {
        assert!(hint("make && sleep 45", "c").contains("固定 sleep 干等不划算"));
        assert!(hint("Start-Sleep -Milliseconds 60000", "c").contains("固定 sleep"));
        assert!(hint("timeout /t 40", "c").contains("固定 sleep"));
        assert_eq!(hint("sleep 2.5", "c"), "");
        assert_eq!(hint("sleep 10", "c"), "");
        assert_eq!(hint("make test", "hints"), "");
        assert_eq!(hint("sed -i s/a/b/ x", "hints"), "");
        assert!(hint("cd src && cat a.txt | grep x", "hints").contains("read_file / search_files"));
        assert!(hint("/bin/ls -la", "hints").contains("read_file"));
        assert_eq!(hint("head x", "hints"), "");
        assert!(hint("head x", "other").contains("read_file"));
        assert_eq!(result(Some(0), ""), "退出码 0\n（无输出）");
        assert_eq!(result(None, "x"), "（超时或被终止）\nx");
    }
}
