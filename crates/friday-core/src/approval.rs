//! 一个工具调用要不要先问用户（== Swift AgentRunner 的 approval / isDestructive / isSystemChange；Qt AgentRunner::approvalFor 等）：
//! - 元工具（创建 / 修改工具等）、付费生成（设置里要确认时）始终确认，命令的 allowed-tools 也不免（forced）
//! - 其余按这段会话的权限模式：自动 = 删除类操作先问；计划 = 会改动系统的都问；完全 = 都不问
//! - v2.8 往系统目录装软件、写文件、改系统设置：「这些事停下来交给我」勾着「系统」时自动、计划模式下也要确认（forced）
//! - v2.12 浏览器里往密码框打字、点登录、点付款：勾着账号 / 钱时也要确认（forced；是不是这种操作由两端的浏览器判断）
//!
//! Windows 上 run_shell 是 PowerShell，模型也常写 cmd 的命令：删除类再认 Remove-Item、del、rd /s 这些（Qt 版加的；macOS 用不到）。

use std::sync::LazyLock;

use fancy_regex::Regex;

/// 删除类的命令片段（Unix 写法）
const DANGEROUS: [&str; 15] = [
    "rm ",
    "rm\t",
    "rmdir",
    "unlink ",
    "shred ",
    "trash ",
    "git clean",
    "git reset --hard",
    "git checkout -- ",
    "drop table",
    "truncate ",
    "mkfs",
    "diskutil erase",
    "> /dev/",
    "srm ",
];

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("审批规则的正则")
}

/// cmdlet 全名在哪儿出现都算（整词：前面不是字母数字、「-」「.」「$」或路径分隔符，后面是空白、语句分隔符或结尾）
static PS_COMMANDS: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r#"(?i)(?<![\w.$\-\\/])(remove-item|remove-itemproperty|clear-content|clear-item|clear-recyclebin|format-volume|clear-disk|initialize-disk|remove-partition|remove-volume)(?=$|[\s;|&)}'"])"#,
    )
});
/// 短别名（del、rd、ri…）只认出现在命令的位置：开头、换行或 ; | & ( { 之后、cmd 的 /c 之后
static PS_ALIASES: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"(?i)(?:^|[\n;|&({]|\s/[ck]\s)\s*["']?(ri|del|erase|rd|rmdir|rp|clc|diskpart)(?=$|[\s;|&)}'"])"#));
/// 格式化磁盘、.NET 的删除、robocopy 的镜像删除、注册表 / 服务 / 计划任务的删除
static PS_OTHERS: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)(?<![\w.$\-\\/])format\s+[a-z]:|::delete\s*\(|\.delete\s*\(\s*\)|\brobocopy\b[^\n;|&]*\s/(mir|purge)\b|\breg(\.exe)?\s+delete\b|\bsc(\.exe)?\s+delete\b|\bschtasks(\.exe)?\s[^\n;|&]*/delete\b|\bvssadmin(\.exe)?\s+delete\b|\bremove-service\b",
    )
});

/// 系统目录：Windows 的 Program Files、ProgramData、Windows（及环境变量写法），Unix / macOS 的 /usr、/etc、/opt、/bin、/sbin、/Library、/System、/Applications
const SYSTEM_PATH: &str = r#"(?:[A-Za-z]:[\\/]+(?:Program Files(?: \(x86\))?|ProgramData|Windows)(?=$|[\\/"' ])|%(?:ProgramFiles(?:\(x86\))?|ProgramData|SystemRoot|windir)%|\$env:(?:ProgramFiles|ProgramData|SystemRoot|windir)\b|(?<![\w.~$-])/(?:usr|etc|opt|bin|sbin|Library|System|Applications)(?=/|$|["' ]))"#;
static SYSTEM_PATH_RE: LazyLock<Regex> = LazyLock::new(|| regex(&format!("(?i){SYSTEM_PATH}")));
/// 装软件、提权
static INSTALL: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)\bcmake\b[^\n;|&]*--install\b(?![^\n;|&]*--prefix)|--target\s+install\b|(?:^|[\s;|&(])(?:make|gmake|nmake|mingw32-make|jom|ninja)\b[^\n;|&]*\binstall\b|\bmsiexec\b|\b(?:choco|winget|scoop)\s+(?:install|upgrade|uninstall)\b|\b(?:apt|apt-get|yum|dnf|pacman|zypper|brew|port|snap)\s+(?:install|remove|uninstall|upgrade)\b|(?:^|[\s;|&(])sudo\s|\bnpm\s+(?:i|install|uninstall|rm)\b[^\n;|&]*\s(?:-g|--global)\b|\bInstall-(?:Module|Package|Script)\b|\bStart-Process\b[^\n;|&]*-Verb\s+RunAs\b",
    )
});
/// 改系统设置：注册表 HKLM、系统环境变量、服务、计划任务、执行策略、网络和启动配置
static SETTINGS: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)\breg(?:\.exe)?\s+(?:add|import|load|copy|restore)\b|\b(?:Set|New|Remove|Rename)-ItemProperty\b[^\n;|&]*\b(?:HKLM|HKEY_LOCAL_MACHINE)\b|\bsetx\b[^\n|&]*\s/m\b|\bSet-ExecutionPolicy\b(?![^\n;|&]*-Scope\s+(?:Process|CurrentUser))|\bnetsh\b[^\n;|&]*\b(?:set|add|delete|reset)\b|\bbcdedit\b|\bsc(?:\.exe)?\s+(?:config|create|stop|start)\b|\b(?:New|Set|Stop|Start|Restart)-Service\b|\bschtasks(?:\.exe)?\s[^\n;|&]*/create\b|\bRegister-ScheduledTask\b|\bsystemctl\s+(?:enable|disable|start|stop|restart|mask)\b|\blaunchctl\s+(?:load|unload|bootstrap|bootout|enable|disable)\b",
    )
});
/// 往系统目录写：复制、移动、新建、写入一类的动作（只是列出、读取的不算）
static WRITES: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r#"(?i)(?<![\w.$-])(?:Copy-Item|Move-Item|New-Item|Set-Content|Add-Content|Out-File|Rename-Item|Expand-Archive|cp|mv|copy|move|xcopy|robocopy|mkdir|md|ln|install|tee|tar|unzip|7z|chmod|chown|ditto)(?=$|[\s;|&)'"])"#,
    )
});

fn matches(pattern: &Regex, text: &str) -> bool {
    pattern.is_match(text).unwrap_or(false)
}

/// 命令是不是删除类的（power_shell：Windows 上再认 PowerShell / cmd 的写法）
pub fn is_destructive_command(command: &str, power_shell: bool) -> bool {
    let lowered = command.to_lowercase();
    if DANGEROUS.iter().any(|piece| lowered.contains(piece)) {
        return true;
    }
    power_shell && (matches(&PS_COMMANDS, command) || matches(&PS_ALIASES, command) || matches(&PS_OTHERS, command))
}

/// 删除类操作：删工具、删 MCP 服务、名字里带 delete / remove / 以 rm 开头的工具、删东西的命令（run_shell、run_in_terminal）
pub fn is_destructive(name: &str, command: Option<&str>, power_shell: bool) -> bool {
    if name == "delete_tool" || name == "remove_mcp_server" {
        return true;
    }
    let lower = name.to_lowercase();
    if lower.contains("delete") || lower.contains("remove") || lower.starts_with("rm") {
        return true;
    }
    if name == "run_shell" || name == "run_in_terminal" {
        return command.is_some_and(|command| is_destructive_command(command, power_shell));
    }
    false
}

pub fn is_system_path(path: &str) -> bool {
    matches(&SYSTEM_PATH_RE, path.trim_matches([' ', '\t']))
}

/// 装软件、改系统设置、往系统目录写东西（只读系统目录的不算）
pub fn is_system_command(command: &str) -> bool {
    matches(&INSTALL, command) || matches(&SETTINGS, command) || (matches(&SYSTEM_PATH_RE, command) && matches(&WRITES, command))
}

/// 往系统目录装软件、写文件、改系统设置：命令看 is_system_command；write_file / edit_file 看路径
pub fn is_system_change(name: &str, command: Option<&str>, path: Option<&str>) -> bool {
    match name {
        "run_shell" | "run_in_terminal" => command.is_some_and(is_system_command),
        "write_file" | "edit_file" => path.is_some_and(is_system_path),
        _ => false,
    }
}

/// 要不要确认的输入
pub struct Request<'a> {
    pub name: &'a str,
    pub command: Option<&'a str>,
    pub path: Option<&'a str>,
    /// 元工具这类始终确认的
    pub always_confirm: bool,
    /// 付费生成，且设置里要确认付费调用
    pub paid: bool,
    /// auto / plan / full
    pub mode: &'a str,
    /// 工具的权限本身要确认（计划模式下）
    pub permission_needs_approval: bool,
    /// 「这些事停下来交给我」勾着「系统」
    pub stops_system: bool,
    /// 浏览器里要停下来的操作（两端判断）
    pub browser_stop: bool,
    pub power_shell: bool,
}

/// 返回 (needed, forced)
pub fn decide(request: &Request) -> (bool, bool) {
    if request.always_confirm || request.paid {
        return (true, true);
    }
    if request.mode == "full" {
        return (false, false);
    }
    let stop = (request.stops_system && is_system_change(request.name, request.command, request.path)) || request.browser_stop;
    let destructive = is_destructive(request.name, request.command, request.power_shell);
    let needed = match request.mode {
        "plan" => request.permission_needs_approval || destructive || stop,
        _ => destructive || stop,
    };
    (needed, stop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destructive() {
        assert!(is_destructive("delete_tool", None, false));
        assert!(is_destructive("browser_remove_tab", None, false));
        assert!(is_destructive("run_shell", Some("cd /tmp && RM -rf x"), false));
        assert!(!is_destructive("run_shell", Some("ls -la"), false));
        assert!(!is_destructive("run_shell", Some("Remove-Item x"), false));
        assert!(is_destructive("run_shell", Some("Remove-Item -Recurse x"), true));
        assert!(is_destructive("run_shell", Some("cmd /c del x.txt"), true));
        assert!(!is_destructive("run_shell", Some("echo 'del x' > a.py"), true));
        assert!(is_destructive("run_shell", Some("format d:"), true));
        assert!(!is_destructive("write_file", Some("rm -rf"), false));
    }

    #[test]
    fn system_changes() {
        assert!(is_system_path("/usr/local/bin/x"));
        assert!(is_system_path("C:\\Program Files\\Friday\\a"));
        assert!(!is_system_path("/Users/me/usr/x"));
        assert!(!is_system_path("~/Library/x"));
        assert!(is_system_command("sudo make install"));
        assert!(is_system_command("cmake --install build"));
        assert!(!is_system_command("cmake --install build --prefix ~/x"));
        assert!(is_system_command("cp a /usr/local/bin/"));
        assert!(!is_system_command("ls /usr/local/bin"));
        assert!(is_system_command("Set-ExecutionPolicy Bypass"));
        assert!(!is_system_command("Set-ExecutionPolicy Bypass -Scope Process"));
        assert!(is_system_change("write_file", None, Some("/etc/hosts")));
        assert!(!is_system_change("read_file", None, Some("/etc/hosts")));
    }

    #[test]
    fn decisions() {
        let base = Request {
            name: "run_shell",
            command: Some("ls"),
            path: None,
            always_confirm: false,
            paid: false,
            mode: "auto",
            permission_needs_approval: true,
            stops_system: true,
            browser_stop: false,
            power_shell: false,
        };
        assert_eq!(decide(&base), (false, false));
        assert_eq!(decide(&Request { mode: "plan", ..base }), (true, false));
        assert_eq!(decide(&Request { command: Some("rm x"), ..base }), (true, false));
        assert_eq!(decide(&Request { command: Some("sudo apt install x"), ..base }), (true, true));
        assert_eq!(decide(&Request { command: Some("sudo apt install x"), stops_system: false, ..base }), (false, false));
        assert_eq!(decide(&Request { command: Some("rm x"), mode: "full", ..base }), (false, false));
        assert_eq!(decide(&Request { mode: "full", always_confirm: true, ..base }), (true, true));
        assert_eq!(decide(&Request { browser_stop: true, ..base }), (true, true));
    }
}
