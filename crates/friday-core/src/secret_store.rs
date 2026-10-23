//! 密钥库：API Key 等敏感值只存在这里，Agent 通过名字引用，永远拿不到明文。
//! 脚本工具按清单里声明的 secrets 以环境变量注入；MCP 配置和在线服务用 `${secret:NAME}` 引用。
//!
//! 存在数据目录的 secrets.json（{名字: 值}，pretty + 键排序，仅当前用户可读写 0600；Windows 上靠用户目录的 ACL）。
//! v1.16 密钥只以名字流转（见 [`crate::secret_guard`]）：入口处认出的新密钥自动收进来（[`Vault::protect`]），记下来源；
//! 用户说「不是密钥」的记进 ignored（只存 SHA-256），它的名字以后照样能换回原值（released）。
//! 这些记录在 secrets-meta.json，secrets.json 格式不变。两个文件的写法和两端原来的一样（降级回旧版也读得了）。
//!
//! 一个库一个句柄（[`open`]）：两端的 SecretStore 各持一个，界面要观察的名字、自动收进来的记录每次改动后取快照。
//! 库一变就把值交给 SecretGuard（发给模型前遮、认新密钥时跳过说过不是密钥的），两端不用再转交。
//! 写盘是同步的（文件很小）：改动返回时已经落盘。任意线程都能调。
//!
//! 原来是 Swift `Secrets/SecretStore.swift` 和 Qt `secrets/SecretStore.cpp` 各写一份。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use chrono::{DateTime, SecondsFormat, Utc};
use regex::{Captures, Regex};
use serde_json::{Map, Value, json};
use unicode_segmentation::UnicodeSegmentation;

use crate::secret_guard::{self, Candidate};
use crate::swift_json;

/// 自动收进来的一条：从哪里来、什么时候
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoInfo {
    pub source: String,
    pub added_at: DateTime<Utc>,
}

#[derive(Debug, Default)]
pub struct Vault {
    /// 只在内存里（远程副本、测试）：不读写文件，也不动 SecretGuard
    memory: bool,
    directory: PathBuf,
    values: BTreeMap<String, String>,
    auto: BTreeMap<String, AutoInfo>,
    ignored: BTreeSet<String>,
    /// 说了「不是密钥」的：名字 → 原值（不是密钥，存明文无妨），对话里留下的 ${secret:名字} 执行时照样换回去
    released: BTreeMap<String, String>,
    /// v1.16 之前存下的对话、学习记录、日志扫过一遍了
    history_scanned: bool,
}

static REFERENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\{secret:([A-Za-z_][A-Za-z0-9_]*)\}").unwrap());

/// 以前的存档里认出来的，来源记这个
pub const HISTORY_SOURCE: &str = "以前的对话记录";

impl Vault {
    /// 读数据目录里的 secrets.json 和 secrets-meta.json（没有就是空库）
    pub fn load(directory: &Path) -> Vault {
        let mut vault = Vault { directory: directory.to_path_buf(), ..Vault::default() };
        if let Some(Value::Object(saved)) = read_json(&vault.file()) {
            vault.values = saved.into_iter().filter_map(|(name, value)| value.as_str().map(|value| (name, value.to_string()))).collect();
        }
        if let Some(Value::Object(meta)) = read_json(&vault.meta_file()) {
            if let Some(Value::Object(auto)) = meta.get("auto") {
                for (name, info) in auto {
                    let source = info.get("source").and_then(Value::as_str);
                    let added_at = info.get("addedAt").and_then(Value::as_str).and_then(|text| DateTime::parse_from_rfc3339(text).ok());
                    if let (Some(source), Some(added_at)) = (source, added_at) {
                        vault.auto.insert(name.clone(), AutoInfo { source: source.to_string(), added_at: added_at.with_timezone(&Utc) });
                    }
                }
            }
            if let Some(Value::Array(ignored)) = meta.get("ignored") {
                vault.ignored = ignored.iter().filter_map(Value::as_str).map(str::to_string).collect();
            }
            if let Some(Value::Object(released)) = meta.get("released") {
                vault.released =
                    released.iter().filter_map(|(name, value)| value.as_str().map(|value| (name.clone(), value.to_string()))).collect();
            }
            vault.history_scanned = meta.get("historyScanned").and_then(Value::as_bool).unwrap_or(false);
        }
        let values = &vault.values;
        vault.auto.retain(|name, _| values.contains_key(name));
        vault
    }

    fn file(&self) -> PathBuf {
        self.directory.join("secrets.json")
    }

    fn meta_file(&self) -> PathBuf {
        self.directory.join("secrets-meta.json")
    }

    pub fn names(&self) -> Vec<String> {
        self.values.keys().cloned().collect()
    }

    pub fn auto(&self) -> &BTreeMap<String, AutoInfo> {
        &self.auto
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// 只在内存里的库
    pub fn memory() -> Vault {
        Vault { memory: true, ..Vault::default() }
    }

    /// 已知的值交给 SecretGuard；返回参与全文匹配的个数（只在内存里的库不交，返回 SecretGuard 现在的个数）
    pub fn publish(&self) -> usize {
        if self.memory {
            return secret_guard::shared().known_count();
        }
        let values: HashMap<String, String> = self.values.iter().map(|(name, value)| (name.clone(), value.clone())).collect();
        let released: HashMap<String, String> = self.released.iter().map(|(name, value)| (name.clone(), value.clone())).collect();
        secret_guard::shared().update(&values, self.ignored.iter().cloned().collect::<HashSet<_>>(), &released)
    }

    /// 保存（去掉首尾空白，空值忽略）；用户自己填的不再算自动收进来的、也不再是「不是密钥」。返回存了没有
    pub fn set(&mut self, name: &str, value: &str) -> bool {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return false;
        }
        self.values.insert(name.to_string(), trimmed.to_string());
        self.auto.remove(name);
        self.released.remove(name);
        self.save_meta();
        self.save();
        true
    }

    pub fn remove(&mut self, name: &str) {
        self.values.remove(name);
        if self.auto.remove(name).is_some() {
            self.save_meta();
        }
        self.save();
    }

    /// 展示用：只露出前后几位（abc••••••••xyz；8 个字以内全是 •）
    pub fn masked(&self, name: &str) -> String {
        let Some(value) = self.values.get(name) else { return String::new() };
        let characters: Vec<&str> = value.graphemes(true).collect();
        if characters.len() <= 8 {
            return "•".repeat(characters.len());
        }
        format!("{}{}{}", characters[..3].concat(), "•".repeat(8), characters[characters.len() - 3..].concat())
    }

    // MARK: - 自动收进来（v1.16）

    /// 入口处（用户发的话、工具输出、报错）：认出的新密钥收进库，连同库里已有的一起换成 `${secret:名字}`。
    /// source：从哪里来的（显示在 设置 › 服务与密钥 里）。返回（换过的文本，库变了没有）
    pub fn protect(&mut self, text: &str, source: &str) -> (String, bool) {
        if text.is_empty() {
            return (String::new(), false);
        }
        let mut found = secret_guard::shared().detect(text, false);
        let mut result = text.to_string();
        let before = self.values.len();
        // 先按位置换掉这次认出的（短的密码不做全文匹配，只换认出来的那一处）；从后往前换，位置才不乱
        found.sort_by_key(|candidate| std::cmp::Reverse(candidate.start));
        for candidate in &found {
            let name = self.collect(candidate, source);
            result.replace_range(candidate.start..candidate.end, &secret_guard::reference(&name));
        }
        let changed = self.values.len() != before;
        if changed {
            self.save();
            self.save_meta();
            self.publish();
        }
        // 库里已有的（含刚收进来的）全文换
        (secret_guard::shared().mask(&result), changed)
    }

    /// 收进库（不写盘），返回名字：值已在库里就用原来的名字；名字被别的值占了就加 _2、_3
    fn collect(&mut self, candidate: &Candidate, source: &str) -> String {
        if let Some((name, _)) = self.values.iter().find(|(_, value)| **value == candidate.value) {
            return name.clone();
        }
        let mut name = candidate.name.clone();
        let mut index = 2;
        while self.values.contains_key(&name) || self.released.contains_key(&name) {
            name = format!("{}_{index}", candidate.name);
            index += 1;
        }
        self.values.insert(name.clone(), candidate.value.clone());
        self.auto.insert(name.clone(), AutoInfo { source: source.to_string(), added_at: Utc::now() });
        name
    }

    /// 以前存下的对话、学习记录（v2.0 之前的 learning.json）、日志里的密钥（v1.16 之前的、不在库里的，比如命令打出来的令牌）收进库；
    /// 之后的 SecretGuard 清理存档把它们换成名字。只用最可靠的几条规则。once：只做一次（做过就记在 secrets-meta.json）。返回库变了没有
    pub fn scan_stored_history(&mut self, directory: &Path, once: bool) -> bool {
        if once && self.history_scanned {
            return false;
        }
        let before = self.values.len();
        for file in ["conversations.json", "learning.json", "journal.json"] {
            let Ok(text) = std::fs::read_to_string(directory.join(file)) else { continue };
            // JSON 里「/」写成「\/」：先还原，网址才认得出；值里带反斜杠的（JSON 转义过的）不收，收进来的会和原文对不上
            let plain = text.replace("\\/", "/");
            for candidate in secret_guard::shared().detect(&plain, true) {
                if !candidate.value.contains('\\') {
                    self.collect(&candidate, HISTORY_SOURCE);
                }
            }
        }
        let changed = self.values.len() != before;
        if changed {
            self.save();
            self.publish();
        }
        if once {
            self.history_scanned = true;
        }
        if changed || once {
            self.save_meta();
        }
        changed
    }

    /// 用户说「不是密钥」：移出库、以后不再收它；对话里已经写成 ${secret:名字} 的，执行时照样换回原值。返回库变了没有
    pub fn mark_not_secret(&mut self, name: &str) -> bool {
        let Some(value) = self.values.remove(name) else { return false };
        self.ignored.insert(secret_guard::hash(&value));
        self.released.insert(name.to_string(), value);
        self.auto.remove(name);
        self.save();
        self.save_meta();
        self.publish();
        true
    }

    // MARK: - 执行时换回明文

    /// 把文本里的 `${secret:NAME}` 换成真实值；没有的引用原样保留，方便排查
    pub fn substitute(&self, text: &str) -> String {
        REFERENCE
            .replace_all(text, |captures: &Captures| {
                let name = &captures[1];
                self.values.get(name).or_else(|| self.released.get(name)).cloned().unwrap_or_else(|| captures[0].to_string())
            })
            .into_owned()
    }

    /// 工具参数里的 `${secret:名字}` 换成明文（执行的那一刻；JSON 里每个字符串都换）
    pub fn substitute_json(&self, value: Value) -> Value {
        match value {
            Value::String(text) if text.contains("${secret:") => Value::String(self.substitute(&text)),
            Value::Array(items) => Value::Array(items.into_iter().map(|item| self.substitute_json(item)).collect()),
            Value::Object(object) => Value::Object(object.into_iter().map(|(key, item)| (key, self.substitute_json(item))).collect()),
            other => other,
        }
    }

    /// 文本里引用了但库里没有的密钥名（按出现的顺序）
    pub fn missing_references(&self, text: &str) -> Vec<String> {
        REFERENCE.captures_iter(text).map(|captures| captures[1].to_string()).filter(|name| !self.values.contains_key(name)).collect()
    }

    /// 脚本工具声明需要的密钥 → 环境变量（只含库里有的）
    pub fn environment(&self, names: &[String]) -> BTreeMap<String, String> {
        names.iter().filter_map(|name| self.values.get(name).map(|value| (name.clone(), value.clone()))).collect()
    }

    // MARK: - 写盘

    fn save(&self) {
        if self.memory {
            return;
        }
        let object: Map<String, Value> = self.values.iter().map(|(name, value)| (name.clone(), Value::from(value.clone()))).collect();
        let _ = write_private(&self.file(), swift_json::pretty(&Value::Object(object)).as_bytes());
    }

    fn save_meta(&self) {
        if self.memory {
            return;
        }
        let auto: Map<String, Value> = self
            .auto
            .iter()
            .map(|(name, info)| {
                (name.clone(), json!({ "source": info.source, "addedAt": info.added_at.to_rfc3339_opts(SecondsFormat::Secs, true) }))
            })
            .collect();
        let released: Map<String, Value> = self.released.iter().map(|(name, value)| (name.clone(), Value::from(value.clone()))).collect();
        let mut meta = json!({ "auto": auto, "ignored": self.ignored.iter().collect::<Vec<_>>(), "released": released });
        if self.history_scanned {
            meta["historyScanned"] = Value::Bool(true);
        }
        let _ = write_private(&self.meta_file(), swift_json::pretty(&meta).as_bytes());
    }

    /// 界面要的快照：名字、自动收进来的记录（时间用 Unix 毫秒）、参与全文匹配的值有几个
    pub fn snapshot(&self) -> Value {
        let auto: Map<String, Value> = self
            .auto
            .iter()
            .map(|(name, info)| (name.clone(), json!({ "source": info.source, "addedAt": info.added_at.timestamp_millis() })))
            .collect();
        let known = self.values.values().filter(|value| value.chars().count() >= secret_guard::MINIMUM_KNOWN_LENGTH).count();
        json!({ "names": self.names(), "auto": auto, "known": known })
    }
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// 先写到同目录的临时文件再换名，只有本用户可读写（0600）
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("file");
    let temporary = path.with_file_name(format!(".{name}.friday-core-{}", std::process::id()));
    std::fs::write(&temporary, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

// MARK: - 句柄

static VAULTS: LazyLock<Mutex<HashMap<u64, Vault>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT: AtomicU64 = AtomicU64::new(1);

/// 打开数据目录里的库，返回句柄；已知的值同时交给 SecretGuard。directory 为 None：只在内存里的库
pub fn open(directory: Option<&Path>) -> u64 {
    let vault = match directory {
        Some(directory) => Vault::load(directory),
        None => Vault::memory(),
    };
    vault.publish();
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    VAULTS.lock().unwrap().insert(id, vault);
    id
}

/// 关掉句柄。还有别的库开着就把最后打开的那个的值交给 SecretGuard，都关了才撤掉（Swift 自检会临时建一个库再丢掉，
/// Qt 的测试会先后建好几个）。返回 SecretGuard 里现在参与全文匹配的个数
pub fn close(id: u64) -> usize {
    let mut vaults = VAULTS.lock().unwrap();
    match vaults.remove(&id) {
        None => return 0,
        Some(vault) if vault.memory => return secret_guard::shared().known_count(),
        Some(_) => {}
    }
    match vaults.iter().filter(|(_, vault)| !vault.memory).max_by_key(|(id, _)| **id) {
        Some((_, latest)) => latest.publish(),
        None => secret_guard::shared().update(&HashMap::new(), HashSet::new(), &HashMap::new()),
    }
}

/// 测试里会改全局 SecretGuard 的（打开、关掉句柄）拿着它轮流跑，免得并行的测试互相清掉对方的值
#[cfg(test)]
pub(crate) static GLOBAL_GUARD_TEST: Mutex<()> = Mutex::new(());

/// 在句柄对应的库上做事；没有这个句柄返回 None
pub fn with<T>(id: u64, body: impl FnOnce(&mut Vault) -> T) -> Option<T> {
    VAULTS.lock().unwrap().get_mut(&id).map(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GITHUB: &str = concat!("ghp_", "VaultVaultVaultVaultVault0123456789");

    fn directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("friday-core-vault-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    /// == Qt TestModels::secretGuard 后半、Swift 自检 secretGuard 的收进库部分（不碰全局的 SecretGuard 以外的状态）
    #[test]
    fn store() {
        let directory = directory("store");
        let mut vault = Vault::load(&directory);
        assert!(vault.names().is_empty());
        assert!(!vault.set("EMPTY", "  \n"));
        assert!(vault.set("OPENAI_API_KEY", "  sk-abc  "));
        assert_eq!(vault.get("OPENAI_API_KEY"), Some("sk-abc"));
        assert_eq!(vault.masked("OPENAI_API_KEY"), "••••••");
        vault.set("LONG", "abcdefghijklmnop");
        assert_eq!(vault.masked("LONG"), "abc••••••••nop");
        assert_eq!(vault.substitute("key=${secret:LONG} other=${secret:NOPE}"), "key=abcdefghijklmnop other=${secret:NOPE}");
        assert_eq!(vault.missing_references("${secret:LONG} ${secret:NOPE} ${secret:NOPE}"), ["NOPE", "NOPE"]);
        assert_eq!(
            vault.substitute_json(json!({"command": "echo ${secret:LONG}", "list": ["${secret:LONG}", 1]})),
            json!({"command": "echo abcdefghijklmnop", "list": ["abcdefghijklmnop", 1]})
        );
        let environment = vault.environment(&["LONG".to_string(), "NOPE".to_string()]);
        assert_eq!(environment.len(), 1);
        // 写盘：pretty + 键排序（== 两端原来的写法），重新读回来一样
        let text = std::fs::read_to_string(directory.join("secrets.json")).unwrap();
        assert_eq!(text, "{\n  \"LONG\" : \"abcdefghijklmnop\",\n  \"OPENAI_API_KEY\" : \"sk-abc\"\n}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(directory.join("secrets.json")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let reloaded = Vault::load(&directory);
        assert_eq!(reloaded.names(), ["LONG", "OPENAI_API_KEY"]);
        vault.remove("LONG");
        assert_eq!(Vault::load(&directory).names(), ["OPENAI_API_KEY"]);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    /// 收进库、同一个值同一个名字、名字被占加 _2、不是密钥、扫以前的存档（用全局 SecretGuard：只在这一个测试里改它）
    #[test]
    fn collecting() {
        let _guard = GLOBAL_GUARD_TEST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let directory = directory("collect");
        let id = open(Some(&directory));
        let remote = format!("origin\thttps://integemjack:{GITHUB}@github.com/integemjack/Friday.git (fetch)");
        let (protected, changed) = with(id, |vault| vault.protect(&remote, "测试")).unwrap();
        assert!(changed);
        assert_eq!(protected, remote.replace(GITHUB, "${secret:GITHUB_TOKEN}"));
        with(id, |vault| {
            assert_eq!(vault.get("GITHUB_TOKEN"), Some(GITHUB));
            assert_eq!(vault.auto()["GITHUB_TOKEN"].source, "测试");
            assert_eq!(vault.substitute(&protected), remote);
            // 同一个值再出现：同一个名字、库不变
            assert_eq!(vault.protect(&format!("token={GITHUB}"), "测试"), ("token=${secret:GITHUB_TOKEN}".to_string(), false));
            // 名字被别的值占了：加 _2
            let other = concat!("ghp_", "ZyXwVuTsRqPoNmLkJiHgFeDcBa9876543210");
            assert_eq!(vault.protect(&format!("用 {other}"), "测试").0, "用 ${secret:GITHUB_TOKEN_2}");
            // 短密码只换认出来的那一处（不全文匹配）
            assert_eq!(vault.protect("密码是 ab12cd，就这样", "测试").0, "密码是 ${secret:PASSWORD}，就这样");
            assert_eq!(vault.protect("ab12cd 原样", "测试").0, "ab12cd 原样");
        });
        // 记录存进 secrets-meta.json，重新读回来还在
        let reloaded = Vault::load(&directory);
        assert_eq!(reloaded.auto()["GITHUB_TOKEN_2"].source, "测试");
        let meta = std::fs::read_to_string(directory.join("secrets-meta.json")).unwrap();
        assert!(meta.starts_with("{\n  \"auto\" : {\n") && meta.contains("\"ignored\" : [\n\n  ]"), "{meta}");

        // 「不是密钥」：移出库、以后不认，名字照样能换回原值
        with(id, |vault| {
            assert!(vault.mark_not_secret("GITHUB_TOKEN"));
            assert!(vault.get("GITHUB_TOKEN").is_none());
            assert_eq!(vault.substitute("${secret:GITHUB_TOKEN}"), GITHUB);
        });
        assert!(secret_guard::shared().detect(&remote, false).is_empty());
        assert_eq!(secret_guard::shared().mask_stored(r#"{"t":"${secret:GITHUB_TOKEN}"}"#), format!(r#"{{"t":"{GITHUB}"}}"#));
        // 名字被说过不是密钥的占着：新收的也加 _N
        let reloaded = Vault::load(&directory);
        assert!(reloaded.get("GITHUB_TOKEN").is_none() && reloaded.substitute("${secret:GITHUB_TOKEN}") == GITHUB);

        // 以前的存档：扫一遍收进库（只做一次），JSON 里的「\/」也认得
        let old = concat!("ghp_", "OldOldOldOldOldOldOldOldOld0123456789");
        std::fs::write(directory.join("journal.json"), format!(r#"[{{"detail":"https:\/\/x:{old}@github.com\/a.git"}}]"#)).unwrap();
        with(id, |vault| {
            assert!(vault.scan_stored_history(&directory, true));
            let name = vault.names().into_iter().find(|name| vault.get(name) == Some(old)).unwrap();
            assert_eq!(vault.auto()[&name].source, HISTORY_SOURCE);
            vault.remove(&name);
            assert!(!vault.scan_stored_history(&directory, true), "只做一次");
            assert!(vault.names().iter().all(|name| vault.get(name) != Some(old)));
        });
        assert!(Vault::load(&directory).history_scanned);
        // 还有别的库开着：关掉一个不清空 SecretGuard，换成剩下那个的值
        let other = open(Some(&directory));
        let token = concat!("ghp_", "KeepKeepKeepKeepKeepKeep0123456789ab");
        with(other, |vault| vault.set("KEEP", token));
        with(other, |vault| vault.publish());
        assert!(close(id) > 0);
        assert!(with(id, |_| ()).is_none());
        assert_eq!(secret_guard::shared().mask(token), "${secret:KEEP}");
        // 只在内存里的库：不写文件、不动 SecretGuard
        let memory = open(None);
        with(memory, |vault| {
            assert!(vault.set("TELEGRAM_BOT_TOKEN", "123:test-token"));
            assert_eq!(vault.get("TELEGRAM_BOT_TOKEN"), Some("123:test-token"));
        });
        assert!(close(memory) > 0, "关掉内存里的库不影响 SecretGuard");
        assert_eq!(secret_guard::shared().mask(token), "${secret:KEEP}");
        assert_eq!(close(other), 0);
        assert_eq!(secret_guard::shared().mask(token), token);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn reads_swift_files() {
        // Swift 的 JSONEncoder 把「/」写成「\/」、日期是 ISO 8601（不带小数秒）
        let directory = directory("swift");
        std::fs::write(directory.join("secrets.json"), "{\n  \"URL_TOKEN\" : \"https:\\/\\/x\",\n  \"A\" : \"b\"\n}").unwrap();
        std::fs::write(
            directory.join("secrets-meta.json"),
            r#"{"auto":{"URL_TOKEN":{"addedAt":"2026-10-04T06:00:00Z","source":"自检"},"GONE":{"addedAt":"2026-10-04T06:00:00Z","source":"x"}},"ignored":["ab"],"released":{"R":"v"},"historyScanned":true}"#,
        )
        .unwrap();
        let vault = Vault::load(&directory);
        assert_eq!(vault.get("URL_TOKEN"), Some("https://x"));
        assert_eq!(vault.auto().keys().collect::<Vec<_>>(), ["URL_TOKEN"], "库里没有的记录丢掉");
        assert_eq!(vault.auto()["URL_TOKEN"].added_at, crate::quota::utc(2026, 10, 4, 6, 0, 0));
        assert!(vault.history_scanned && vault.ignored.contains("ab") && vault.released["R"] == "v");
        vault.save_meta();
        let meta = std::fs::read_to_string(directory.join("secrets-meta.json")).unwrap();
        assert!(meta.contains("\"addedAt\" : \"2026-10-04T06:00:00Z\"") && meta.contains("\"historyScanned\" : true"), "{meta}");
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
