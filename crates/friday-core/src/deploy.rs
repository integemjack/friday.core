//! 自动安装 / 升级远程机器上的 Friday（docs/REMOTE.md §9；== Swift Remote/RemoteDeploy、Qt remote/RemoteDeploy）里两端共有的部分。
//! Friday v2.87 起一律装服务端包（不再装桌面端跑无头模式、不再替用户装缺的系统库）：
//! - 远程脚本（看装了什么、装 / 换上服务端包，以前的客户端装的桌面端在 Linux 上没有图形桌面或跑不起来时换上后删掉）只留这一份；
//!   拼成 sh -c '…' 或 PowerShell 的 -EncodedCommand
//! - 远程输出里「FRIDAY-DEPLOY …」那几行的解析，按系统和架构定包的键（linux-x64、windows-x64、macos-arm64…）
//! - 从 latest.json 挑这个渠道、这个平台的服务端包（Beta 渠道在正式版和 Beta 里取版本高的），或从本地目录挑
//! - 版本比较、该不该升级、客户端能不能管这台的安装、失败原因的说法
//!
//! 经 ssh 跑命令、下载和核对包、把包流过去留在两端。

use std::path::Path;

use serde_json::{Value, json};

const POSIX_INSPECT: &str = r##"echo "FRIDAY-DEPLOY uname $(uname -s) $(uname -m)"
ver() { o="$("$1" server --version 2>/dev/null)" || return 1; printf %s "$o" | sed -n "s/.*\"version\" *: *\"\([0-9.]*\)\".*/\1/p"; }
if [ "$(uname -s)" != Darwin ]; then
  f="$HOME/.local/opt/Friday/usr/bin/friday"
  if [ -e "$f" ]; then v="$(ver "$f")"; echo "FRIDAY-DEPLOY managed ${v:-broken}"; fi
  if [ -e /opt/friday/usr/bin/friday ]; then v="$(ver /opt/friday/usr/bin/friday)"; echo "FRIDAY-DEPLOY system ${v:-broken}"; fi
fi
l="$HOME/.friday-server/bin/friday-server"
if [ -e "$l" ] || [ -L "$l" ]; then
  if [ "$(readlink "$l")" != "$HOME/.local/opt/Friday/usr/bin/friday" ]; then
    v="$("$l" --version 2>/dev/null | sed -n "s/.*\"version\" *: *\"\([0-9.]*\)\".*/\1/p")"; echo "FRIDAY-DEPLOY server ${v:-broken}"
  fi
fi
for x in "$HOME"/.friday-server/versions/*/bin/friday-server; do
  [ -f "$x" ] || continue
  n="${x%/bin/friday-server}"; echo "FRIDAY-DEPLOY serverhas ${n##*/}"
done
exit 0"##;

const POSIX_SERVER_DEPLOY: &str = r##"d="$HOME/.friday-server"; v="@VERSION@"; k="@KEEP@"; t="$d/versions/$v"; m="$HOME/.local/opt/Friday"; b=0
mkdir -p "$d/versions" || { echo "FRIDAY-DEPLOY fail mkdir"; exit 1; }
if [ "@UPLOAD@" = 1 ]; then
  rm -rf "$t.part" && mkdir -p "$t.part" || { echo "FRIDAY-DEPLOY fail mkdir"; exit 1; }
  tar -xzf - -C "$t.part" || { rm -rf "$t.part"; echo "FRIDAY-DEPLOY fail extract"; exit 1; }
  [ -f "$t.part/friday-server/bin/friday-server" ] || { rm -rf "$t.part"; echo "FRIDAY-DEPLOY fail layout"; exit 1; }
  chmod +x "$t.part/friday-server/bin/friday"
  if ! o="$("$t.part/friday-server/bin/friday-server" --version 2>&1)"; then
    rm -rf "$t.part"; echo "FRIDAY-DEPLOY fail run $(printf %s "$o" | tr -s "\n" " " | head -c 600)"; exit 1
  fi
  { rm -rf "$t" && mv "$t.part/friday-server" "$t" && rm -rf "$t.part"; } || { echo "FRIDAY-DEPLOY fail move"; exit 1; }
fi
[ -f "$t/bin/friday-server" ] || { echo "FRIDAY-DEPLOY fail layout"; exit 1; }
o="$("$t/bin/friday-server" --version 2>&1)" || { echo "FRIDAY-DEPLOY fail run $(printf %s "$o" | tr -s "\n" " " | head -c 600)"; exit 1; }
echo "FRIDAY-DEPLOY installed $(printf %s "$o" | sed -n "s/.*\"version\" *: *\"\([0-9.]*\)\".*/\1/p")"
if [ -d "$d/bin" ] && [ ! -L "$d/bin" ]; then
  if [ -n "$k" ] && [ ! -e "$d/versions/$k" ]; then
    mkdir -p "$d/versions/$k" && mv "$d/bin" "$d/versions/$k/bin"
  else
    mv "$d/bin" "$d/versions/old-$$"
  fi || { echo "FRIDAY-DEPLOY busy"; exit 0; }
fi
ln -sfn "versions/$v/bin" "$d/bin" || { echo "FRIDAY-DEPLOY busy"; exit 0; }
if [ "$(uname -s)" != Darwin ] && [ -e "$m/usr/bin/friday" ]; then
  g=no
  for x in /usr/share/xsessions/* /usr/share/wayland-sessions/* /etc/systemd/system/display-manager.service; do [ -e "$x" ] && g=yes; done
  if [ "$g" = no ] || ! "$m/usr/bin/friday" server --version >/dev/null 2>&1; then
    for p in /proc/[0-9]*; do x="$(readlink "$p/exe" 2>/dev/null)" || continue; case "$x" in "$m"/*) b=1; break ;; esac; done
    if [ "$b" = 0 ]; then
      [ "$(readlink "$HOME/.local/bin/friday")" = "$m/usr/bin/friday" ] && rm -f "$HOME/.local/bin/friday"
      rm -rf "$m" "$HOME"/.local/opt/.friday-stage-*
    fi
  fi
fi
if [ "$b" = 1 ]; then echo "FRIDAY-DEPLOY busy"; else echo "FRIDAY-DEPLOY switched"; fi
for x in "$d"/versions/*; do
  n="${x##*/}"
  [ "$n" = "$v" ] || [ "$n" = "$k" ] || [ "$n" = "*" ] || rm -rf "$x"
done
exit 0"##;

const WINDOWS_INSPECT: &str = r##""FRIDAY-DEPLOY uname Windows $env:PROCESSOR_ARCHITECTURE"
function Ver($exe) { try { $o = (& $exe server --version 2>$null | Out-String); if ($o -match '"version"\s*:\s*"([0-9.]+)"') { return $Matches[1] } } catch {}; return 'broken' }
$u = Join-Path $env:LOCALAPPDATA 'Programs\Friday\friday.com'
if (Test-Path -LiteralPath $u) { "FRIDAY-DEPLOY managed $(Ver $u)" }
$m = Join-Path $env:ProgramFiles 'Friday\friday.com'
if (Test-Path -LiteralPath $m) { "FRIDAY-DEPLOY system $(Ver $m)" }
$l = Join-Path $env:USERPROFILE '.friday-server\bin\friday-server.exe'
if (Test-Path -LiteralPath $l) {
  $v = 'broken'; try { $o = (& $l --version 2>$null | Out-String); if ($o -match '"version"\s*:\s*"([0-9.]+)"') { $v = $Matches[1] } } catch {}
  "FRIDAY-DEPLOY server $v"
}
$vs = Join-Path $env:USERPROFILE '.friday-server\versions'
if (Test-Path -LiteralPath $vs) {
  Get-ChildItem -LiteralPath $vs -Directory | ForEach-Object {
    if (Test-Path -LiteralPath (Join-Path $_.FullName 'bin\friday-server.exe')) { "FRIDAY-DEPLOY serverhas $($_.Name)" }
  }
}
exit 0"##;

const WINDOWS_SERVER_DEPLOY: &str = r##"$d = Join-Path $env:USERPROFILE '.friday-server'; $v = '@VERSION@'; $k = '@KEEP@'
$vs = Join-Path $d 'versions'; $t = Join-Path $vs $v; $p = "$t.part"
function Fail($m) { "FRIDAY-DEPLOY fail $m"; exit 1 }
function Version($exe) {
  $o = (& $exe --version 2>&1 | Out-String).Trim()
  if ($LASTEXITCODE -ne 0) { if ($o.Length -gt 600) { $o = $o.Substring(0, 600) }; Fail "run $o (exit $LASTEXITCODE)" }
  if ($o -match '"version"\s*:\s*"([0-9.]+)"') { return $Matches[1] }
  return $o
}
try { New-Item -ItemType Directory -Force -Path $vs -ErrorAction Stop | Out-Null } catch { Fail 'mkdir' }
if ('@UPLOAD@' -eq '1') {
  try {
    if (Test-Path -LiteralPath $p) { Remove-Item -LiteralPath $p -Recurse -Force -ErrorAction Stop }
    New-Item -ItemType Directory -Force -Path $p -ErrorAction Stop | Out-Null
  } catch { Fail 'mkdir' }
  & "$env:SystemRoot\System32\tar.exe" -xzf - -C $p
  if ($LASTEXITCODE -ne 0) { Remove-Item -LiteralPath $p -Recurse -Force -ErrorAction SilentlyContinue; Fail 'extract' }
  $e = Join-Path $p 'friday-server\bin\friday-server.exe'
  if (-not (Test-Path -LiteralPath $e)) { Remove-Item -LiteralPath $p -Recurse -Force -ErrorAction SilentlyContinue; Fail 'layout' }
  $null = Version $e
  try {
    if (Test-Path -LiteralPath $t) { Remove-Item -LiteralPath $t -Recurse -Force -ErrorAction Stop }
    Move-Item -LiteralPath (Join-Path $p 'friday-server') -Destination $t -ErrorAction Stop
    Remove-Item -LiteralPath $p -Recurse -Force -ErrorAction SilentlyContinue
  } catch { Fail 'move' }
}
$e = Join-Path $t 'bin\friday-server.exe'
if (-not (Test-Path -LiteralPath $e)) { Fail 'layout' }
"FRIDAY-DEPLOY installed $(Version $e)"
$b = Join-Path $d 'bin'
try {
  $i = Get-Item -LiteralPath $b -Force -ErrorAction SilentlyContinue
  if ($i -and ($i.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
    [IO.Directory]::Delete($b)
  } elseif ($i) {
    if ($k -and -not (Test-Path -LiteralPath (Join-Path $vs $k))) {
      New-Item -ItemType Directory -Path (Join-Path $vs $k) -ErrorAction Stop | Out-Null
      Move-Item -LiteralPath $b -Destination (Join-Path $vs "$k\bin") -ErrorAction Stop
    } else {
      Move-Item -LiteralPath $b -Destination (Join-Path $vs "old-$PID") -ErrorAction Stop
    }
  }
  New-Item -ItemType Junction -Path $b -Target (Join-Path $t 'bin') -ErrorAction Stop | Out-Null
} catch { 'FRIDAY-DEPLOY busy'; exit 0 }
'FRIDAY-DEPLOY switched'
Get-ChildItem -LiteralPath $vs -Directory | Where-Object { $_.Name -ne $v -and $_.Name -ne $k } | ForEach-Object {
  $x = $_.FullName
  try { Rename-Item -LiteralPath $x -NewName "$($_.Name).del" -ErrorAction Stop; Remove-Item -LiteralPath "$x.del" -Recurse -Force -ErrorAction SilentlyContinue } catch {}
}
exit 0"##;

/// 远程输出里的「FRIDAY-DEPLOY …」那几行
pub fn parse_report(output: &str) -> Value {
    let marker = "FRIDAY-DEPLOY ";
    let mut report = json!({ "system": "", "machine": "", "managed": "", "systemInstall": "", "serverPackage": "", "serverVersions": [],
        "installedVersion": "", "switched": false, "busy": false, "failure": "" });
    for raw in crate::task_state::split_lines(output) {
        let line = raw.trim_matches([' ', '\t']);
        let Some(at) = line.find(marker) else { continue };
        let rest = line[at + marker.len()..].trim_matches([' ', '\t']);
        let word = rest.split(' ').next().unwrap_or("");
        let value = rest[word.len()..].trim_matches([' ', '\t']);
        let parts: Vec<&str> = value.split(' ').filter(|part| !part.is_empty()).collect();
        match word {
            "uname" => {
                report["system"] = json!(parts.first().copied().unwrap_or(""));
                report["machine"] = json!(parts.get(1).copied().unwrap_or(""));
            }
            "managed" => report["managed"] = json!(value),
            "system" => report["systemInstall"] = json!(value),
            "server" => report["serverPackage"] = json!(value),
            "serverhas" => {
                if let Some(items) = report["serverVersions"].as_array_mut()
                    && !value.is_empty()
                    && !items.iter().any(|item| item == value)
                {
                    items.push(json!(value));
                }
            }
            "installed" => report["installedVersion"] = json!(value),
            "switched" => report["switched"] = json!(true),
            "busy" => report["busy"] = json!(true),
            "fail" => report["failure"] = json!(if value.is_empty() { "unknown" } else { value }),
            _ => {}
        }
    }
    report
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// uname 或 Windows 的 PROCESSOR_ARCHITECTURE → 包的键；认不出为空
pub fn platform_key(system: &str, machine: &str) -> String {
    let os = system.to_lowercase();
    let platform = match os.as_str() {
        "linux" => "linux",
        "windows" => "windows",
        "darwin" => "macos",
        _ if os.starts_with("mingw") || os.starts_with("msys") => "windows",
        _ => "",
    };
    let arch = match machine.to_lowercase().as_str() {
        "x86_64" | "amd64" | "x64" => "x64",
        "aarch64" | "arm64" => "arm64",
        _ => "",
    };
    if platform.is_empty() || arch.is_empty() { String::new() } else { format!("{platform}-{arch}") }
}

/// Release 标签：v1.0.3（正式版）、v1.0.3-beta（预发布）；旧格式 v1.0-beta.12 / v1.0-build.12 也按 Beta 算
#[derive(Debug, Clone, PartialEq)]
pub struct Version {
    pub components: Vec<i64>,
    pub beta: bool,
    pub build: i64,
}

impl Version {
    pub fn parse(tag: &str) -> Option<Version> {
        let mut text = tag.trim();
        if let Some(rest) = text.strip_prefix(['v', 'V']) {
            text = rest;
        }
        let (head, suffix) = match text.split_once('-') {
            Some((head, suffix)) => (head, Some(suffix)),
            None => (text, None),
        };
        if head.is_empty() {
            return None;
        }
        let components: Vec<i64> = head.split('.').map(|part| part.parse().ok()).collect::<Option<_>>()?;
        let (beta, build) = match suffix {
            Some(suffix) => {
                let suffix = suffix.to_lowercase();
                if !(suffix.starts_with("beta") || suffix.starts_with("build") || suffix.starts_with("dev")) {
                    return None;
                }
                let digits: String = suffix.chars().skip_while(|c| !c.is_ascii_digit()).collect();
                (true, digits.parse().unwrap_or(0))
            }
            None => (false, 0),
        };
        Some(Version { components, beta, build })
    }

    pub fn version_string(&self) -> String {
        self.components.iter().map(i64::to_string).collect::<Vec<_>>().join(".")
    }

    pub fn less_than(&self, other: &Version) -> bool {
        let count = self.components.len().max(other.components.len());
        for index in 0..count {
            let left = self.components.get(index).copied().unwrap_or(0);
            let right = other.components.get(index).copied().unwrap_or(0);
            if left != right {
                return left < right;
            }
        }
        if self.beta != other.beta {
            return self.beta;
        }
        self.build < other.build
    }
}

/// latest.json 里这个渠道（stable / beta）、这个平台的服务端包（assets[键].server）：Beta 渠道在正式版和 Beta 里取版本高的，
/// 正式版渠道只看正式版；版本、协议版本是包里 server.json 的（条目里写的）。返回 {name, url, sha256, size, version, protocolVersion} 或 null
pub fn from_manifest(manifest: &Value, channel: &str, key: &str) -> Value {
    let candidates: Vec<&Value> = if channel == "beta" { vec![&manifest["stable"], &manifest["beta"]] } else { vec![&manifest["stable"]] };
    let mut best: Option<(Value, Version)> = None;
    for entry in candidates.into_iter().filter(|entry| entry.is_object()) {
        let Some(version) = Version::parse(text(entry, "tag")) else { continue };
        if let Some((_, current)) = &best
            && !current.less_than(&version)
        {
            continue;
        }
        let file = &entry["assets"][key]["server"];
        if text(file, "url").is_empty() {
            continue;
        }
        let name = match text(file, "name") {
            "" => text(file, "url").rsplit('/').next().unwrap_or("").to_string(),
            name => name.to_string(),
        };
        let own = text(file, "version");
        let package = json!({ "name": name, "url": text(file, "url"), "sha256": text(file, "sha256").to_lowercase(),
            "size": file.get("size").and_then(Value::as_i64).unwrap_or(0),
            "version": if own.is_empty() { version.version_string() } else { own.to_string() },
            "protocolVersion": file.get("protocol").and_then(Value::as_i64).unwrap_or(0) });
        best = Some((package, version));
    }
    best.map_or(Value::Null, |(package, _)| package)
}

/// 目录里这个平台的服务端包（Friday-<标签>-server-<键>.tar.gz，标签最新的）；协议版本是 protocol（本机的）。
/// 返回 {file, name, size, version, protocolVersion} 或 null
pub fn from_directory(directory: &Path, key: &str, protocol: i64) -> Value {
    let suffix = format!("-server-{key}.tar.gz");
    let mut best: Option<(Value, Version)> = None;
    for entry in std::fs::read_dir(directory).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("Friday-") || !name.ends_with(&suffix) || name.len() < "Friday-".len() + suffix.len() {
            continue;
        }
        let Some(version) = Version::parse(&name["Friday-".len()..name.len() - suffix.len()]) else { continue };
        if let Some((_, current)) = &best
            && !current.less_than(&version)
        {
            continue;
        }
        let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
        best = Some((
            json!({ "file": crate::tool_files::slash(&entry.path()), "name": name, "size": size, "version": version.version_string(), "protocolVersion": protocol }),
            version,
        ));
    }
    best.map_or(Value::Null, |(package, _)| package)
}

/// X.Y.Z 比较（不是数字的当空：-1 / 0 / 1）
pub fn compare_versions(a: &str, b: &str) -> i64 {
    let components = |text: &str| -> Vec<i64> {
        let parts: Option<Vec<i64>> = text.trim_matches([' ', '\t']).split('.').map(|part| part.parse().ok()).collect();
        parts.unwrap_or_default()
    };
    let (mut left, mut right) = (components(a), components(b));
    while left.len() < right.len() {
        left.push(0);
    }
    while right.len() < left.len() {
        right.push(0);
    }
    for (l, r) in left.iter().zip(right.iter()) {
        if l != r {
            return if l < r { -1 } else { 1 };
        }
    }
    0
}

/// 正式的版本号：X.Y.Z 且不是 0.0.0（自己编的是 0.0.0）
pub fn is_release_version(version: &str) -> bool {
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
        && compare_versions(version, "0.0.0") > 0
}

/// 服务端该升级：两边都是正式版本号、服务端比包旧；开发版客户端只在用本地包时升级；正式版客户端不把服务端升到比自己还新
pub fn should_upgrade(server: &str, package: &str, client: &str, client_is_development: bool, local_packages: bool) -> bool {
    if !is_release_version(server) || !is_release_version(package) {
        return false;
    }
    if client_is_development && !local_packages {
        return false;
    }
    if compare_versions(server, package) >= 0 {
        return false;
    }
    if !client_is_development && !local_packages && is_release_version(client) && compare_versions(server, client) >= 0 {
        return false;
    }
    true
}

/// 客户端可以管这台远程上的 Friday：服务端包、以前的客户端装的桌面端（升级时改装服务端包）、什么都没装；
/// 有别处装的桌面端（deb、%ProgramFiles%）时不管，桥先找它、它自己更新
pub fn manages_install(report: &Value) -> bool {
    !text(report, "system").is_empty() && text(report, "systemInstall").is_empty()
}

/// 版本号只能是数字和点：要拼进远程的命令里
pub fn is_version_text(text: &str) -> bool {
    !text.is_empty() && text.split('.').all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

/// 失败原因（远程脚本的 fail 那行）的说法
pub fn describe_failure(failure: &str) -> String {
    let code = failure.split(' ').next().unwrap_or("");
    let detail = failure[code.len()..].trim_matches([' ', '\t']);
    match code {
        "mkdir" => "在远程建不了放 Friday 的目录".into(),
        "extract" => "远程解不开 Friday 的安装包（Linux 要有 tar）".into(),
        "layout" => "Friday 的安装包里的文件不对".into(),
        "move" => "在远程放不好 Friday 的文件".into(),
        "run" => {
            let hint = if detail.contains("GLIBC") {
                "（远程的系统太旧：x64 的包要 glibc 2.35 以上，arm64 的要 2.39 以上）"
            } else {
                ""
            };
            format!("装上的 Friday 在远程跑不起来{hint}：{detail}")
        }
        _ => format!("装 Friday 没成功：{failure}"),
    }
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let value = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(TABLE[(value >> (18 - 6 * index) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// POSIX：sh -c '…'（脚本里没有单引号）
fn posix_command(script: &str) -> String {
    format!("sh -c '{script}'")
}

/// PowerShell：脚本按 UTF-16LE 编成 base64 交给 -EncodedCommand
fn power_shell_command(script: &str) -> String {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    format!("powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand {}", base64(&bytes))
}

/// 看远程装了什么的命令（shell：posix / 其余按 PowerShell）
pub fn inspect_command(shell: &str) -> String {
    if shell == "posix" { posix_command(POSIX_INSPECT) } else { power_shell_command(WINDOWS_INSPECT) }
}

/// 装 / 换上服务端包的命令（keep 是服务正在用的版本）；版本号不对时为空
pub fn deploy_command(shell: &str, version: &str, upload: bool, keep: &str) -> String {
    if !is_version_text(version) || !(keep.is_empty() || is_version_text(keep)) {
        return String::new();
    }
    let posix = shell == "posix";
    let template = if posix { POSIX_SERVER_DEPLOY } else { WINDOWS_SERVER_DEPLOY };
    let script = template.replace("@VERSION@", version).replace("@KEEP@", keep).replace("@UPLOAD@", if upload { "1" } else { "0" });
    if posix { posix_command(&script) } else { power_shell_command(&script) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports() {
        let report = parse_report(
            "noise\n FRIDAY-DEPLOY uname Linux x86_64\nFRIDAY-DEPLOY managed 2.55.1\nFRIDAY-DEPLOY serverhas 2.56.0\nFRIDAY-DEPLOY serverhas 2.56.0\nFRIDAY-DEPLOY server broken\nFRIDAY-DEPLOY fail\n",
        );
        assert_eq!((text(&report, "system"), text(&report, "machine"), text(&report, "managed")), ("Linux", "x86_64", "2.55.1"));
        assert_eq!(report["serverVersions"], json!(["2.56.0"]));
        assert_eq!(report["serverPackage"], "broken");
        assert_eq!(report["failure"], "unknown");
        assert!(manages_install(&report));
        assert!(!manages_install(&json!({ "system": "Linux", "systemInstall": "2.80.0" })));
        assert!(!manages_install(&json!({ "system": "" })));
        assert_eq!(platform_key("Linux", "aarch64"), "linux-arm64");
        assert_eq!(platform_key("MINGW64_NT", "AMD64"), "windows-x64");
        assert_eq!(platform_key("FreeBSD", "x86_64"), "");
    }

    #[test]
    fn versions_and_upgrades() {
        assert!(Version::parse("v2.55.0-beta").unwrap().less_than(&Version::parse("v2.55.0").unwrap()));
        assert!(Version::parse("v1.0-beta.11").unwrap().less_than(&Version::parse("v1.0-beta.12").unwrap()));
        assert!(Version::parse("v1.0-rc1").is_none());
        assert_eq!(compare_versions("2.10.0", "2.9.9"), 1);
        assert!(is_release_version("2.55.0") && !is_release_version("0.0.0") && !is_release_version("2.55"));
        assert!(should_upgrade("2.55.0", "2.56.0", "2.56.0", false, false));
        assert!(!should_upgrade("2.55.0", "2.56.0", "2.55.0", false, false));
        assert!(!should_upgrade("2.55.0", "2.56.0", "0.0.0", true, false));
        assert!(should_upgrade("2.55.0", "2.56.0", "0.0.0", true, true));
        assert!(describe_failure("run GLIBC_2.39 not found").contains("glibc 2.35"));
    }

    #[test]
    fn manifests() {
        let manifest = json!({
            "stable": { "tag": "v2.55.0", "assets": { "linux-x64": { "tarball": { "url": "https://x/Friday-v2.55.0-linux-x64.tar.gz" },
                "server": { "url": "https://x/Friday-v2.55.0-server-linux-x64.tar.gz", "sha256": "AB", "protocol": 3 } } } },
            "beta": { "tag": "v2.56.0-beta", "assets": { "linux-x64": { "server": { "url": "https://x/b.tar.gz", "name": "b", "version": "2.56.1" } } } },
        });
        let stable = from_manifest(&manifest, "stable", "linux-x64");
        assert_eq!(
            (stable["version"].as_str(), stable["protocolVersion"].as_i64(), stable["sha256"].as_str()),
            (Some("2.55.0"), Some(3), Some("ab"))
        );
        assert_eq!(stable["name"], "Friday-v2.55.0-server-linux-x64.tar.gz");
        let beta = from_manifest(&manifest, "beta", "linux-x64");
        assert_eq!((beta["version"].as_str(), beta["name"].as_str()), (Some("2.56.1"), Some("b")));
        assert!(from_manifest(&manifest, "stable", "macos-arm64").is_null());
        let dir = std::env::temp_dir().join(format!("friday-deploy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in
            ["Friday-v2.58.0-linux-x64.tar.gz", "Friday-v2.56.0-beta-server-linux-x64.tar.gz", "Friday-v2.57.0-server-linux-x64.tar.gz"]
        {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let local = from_directory(&dir, "linux-x64", 9);
        assert_eq!((local["version"].as_str(), local["protocolVersion"].as_i64()), (Some("2.57.0"), Some(9)));
        assert!(from_directory(&dir, "windows-x64", 9).is_null());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commands() {
        assert!(inspect_command("posix").starts_with("sh -c 'echo \"FRIDAY-DEPLOY uname"));
        assert!(
            inspect_command("powershell").starts_with("powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand ")
        );
        let posix = deploy_command("posix", "2.55.0", true, "2.54.0");
        assert!(posix.contains("v=\"2.55.0\"; k=\"2.54.0\"") && posix.contains("\"1\" = 1"));
        assert!(deploy_command("posix", "2.55.0; rm", true, "").is_empty());
        assert!(deploy_command("posix", "2.55.0", false, "x").is_empty());
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert_eq!(base64(b"hi"), "aGk=");
    }
}
