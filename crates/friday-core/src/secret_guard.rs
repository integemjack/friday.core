//! v1.16 密钥只以名字流转：明文只出现在两个地方——密钥库里、真正执行的那一刻；其余地方（模型、对话存档、子 Agent 的任务、
//! 日志）一律是 `${secret:名字}`。
//! - 入口（用户发的话、工具输出、报错）：认出来的新密钥收进密钥库（标「自动收进来」和来源），连同库里已有的一起换成名字
//!   （两端的 SecretStore.protect：调这里的 [`Guard::detect`] 和 [`Guard::mask`]）；
//! - 执行（跑命令、写文件、发请求、MCP、脚本工具）：名字换回明文（两端的 SecretStore / ToolRegistry），结果里带回的明文再换回名字；
//! - 发给在线模型之前：按库里的值最后再遮一遍（[`Guard::mask_json`]，任何漏网的都拦在这里）。
//!
//! 认密钥按可靠程度：库里已有的值（精确匹配，含 URL 编码；8 个字符以上才全文匹配，免得把正文里的短串换掉）→ 出现的位置
//! （网址里 `用户:XXX@`、Authorization / X-Api-Key 头、`KEY=` 赋值、「密码是…」、私钥块）→ 已知格式（sk-、ghp_、AKIA… 前缀）。
//! 只看「长得随机」的不算（git 提交号、UUID、哈希都长那样）。判错的代价小：误收的也只是换成一个名字，执行时照样换回原样。
//!
//! 密钥库在两端（存储、收进库、起名字避重、执行时换回明文）；库每次变化时两端调 `secrets.update` 把值交过来，
//! 核心每个进程存一份（[`shared`]）。任意线程都能调。
//!
//! 原来是 Swift `Secrets/SecretGuard.swift` 和 Qt `secrets/SecretGuard.cpp` 各写一份，规则逐条照搬（Swift 为准）。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{LazyLock, RwLock};

use fancy_regex::Regex as FancyRegex;
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// 全文匹配的最短长度
pub const MINIMUM_KNOWN_LENGTH: usize = 8;

/// `${secret:名字}`
pub fn reference(name: &str) -> String {
    format!("${{secret:{name}}}")
}

/// 值的 SHA-256（小写十六进制）：用户说「不是密钥」的值只记这个
pub fn hash(value: &str) -> String {
    Sha256::digest(value.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 密钥名合不合法：大写字母、数字、下划线，2–64 个字符，不以数字开头（== 两端 SecretStore.isValidName）
pub fn is_valid_name(name: &str) -> bool {
    static NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Z_][A-Z0-9_]{1,63}$").unwrap());
    NAME.is_match(name)
}

/// 认出来的一个密钥：值、建议的名字、在原文里的位置
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub value: String,
    pub name: String,
    /// 在原文里的字节范围
    pub start: usize,
    pub end: usize,
    /// 名字取自上下文（赋值左边的名字），比按格式起的名字更准
    pub from_context: bool,
}

#[derive(Default)]
struct State {
    /// 库里的值（8 个字符以上），按长度从长到短：先换长的，短的不会把长的切开
    known: Vec<(String, String)>,
    /// 用户说「不是密钥」的值的 SHA-256
    ignored: HashSet<String>,
    /// 说了「不是密钥」的：名字 → 原值（启动时把存档里的名字换回原文）
    released: Vec<(String, String)>,
}

#[derive(Default)]
pub struct Guard {
    state: RwLock<State>,
}

/// 这个进程的那一份（C 接口的 secrets.* 都用它）
pub fn shared() -> &'static Guard {
    static SHARED: LazyLock<Guard> = LazyLock::new(Guard::default);
    &SHARED
}

impl Guard {
    /// 库变化时把值交过来：values 名字 → 值，ignored 不是密钥的值的哈希，released 说了不是密钥的名字 → 原值。返回参与全文匹配的个数
    pub fn update(&self, values: &HashMap<String, String>, ignored: HashSet<String>, released: &HashMap<String, String>) -> usize {
        let mut known: Vec<(String, String)> = values
            .iter()
            .filter(|(_, value)| value.chars().count() >= MINIMUM_KNOWN_LENGTH)
            .map(|(name, value)| (value.clone(), name.clone()))
            .collect();
        known.sort_by(|a, b| b.0.chars().count().cmp(&a.0.chars().count()).then_with(|| a.1.cmp(&b.1)));
        let mut released: Vec<(String, String)> = released.iter().map(|(name, value)| (name.clone(), value.clone())).collect();
        released.sort();
        let count = known.len();
        *self.state.write().unwrap() = State { known, ignored, released };
        count
    }

    pub fn is_ignored(&self, value: &str) -> bool {
        self.state.read().unwrap().ignored.contains(&hash(value))
    }

    // MARK: - 遮

    /// 按库里的值把明文换成名字（含 URL 编码过的写法）。不认新的
    pub fn mask(&self, text: &str) -> String {
        let state = self.state.read().unwrap();
        if state.known.is_empty() || text.is_empty() {
            return text.to_string();
        }
        let mut result = text.to_string();
        for (value, name) in &state.known {
            let reference = reference(name);
            if result.contains(value.as_str()) {
                result = result.replace(value.as_str(), &reference);
            }
            let encoded = percent_encoded(value);
            if encoded != *value && result.contains(encoded.as_str()) {
                result = result.replace(encoded.as_str(), &reference);
            }
        }
        result
    }

    /// JSON 里的每个字符串都遮一遍（发给在线模型之前）
    pub fn mask_json(&self, value: Value) -> Value {
        if self.state.read().unwrap().known.is_empty() {
            return value;
        }
        self.mask_value(value)
    }

    fn mask_value(&self, value: Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.mask(&text)),
            Value::Array(items) => Value::Array(items.into_iter().map(|item| self.mask_value(item)).collect()),
            Value::Object(object) => Value::Object(object.into_iter().map(|(key, item)| (key, self.mask_value(item))).collect()),
            other => other,
        }
    }

    /// 存下的对话、日志里还留着的明文（v1.16 之前存的、后来才收进库的）换成名字。启动时、这些文件载入之前做。
    /// learning.json 是 v2.0 之前自学习的记录，v2.0 起不再读写，但还留在磁盘上、可能带着明文，照样扫
    pub fn scrub_stored_files(&self, directory: &Path) {
        for file in ["conversations.json", "learning.json", "journal.json"] {
            let path = directory.join(file);
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let masked = self.mask_stored(&text);
            if masked != text {
                let _ = write_atomically(&path, masked.as_bytes());
            }
        }
    }

    /// JSON 文本里的明文换成名字：「/」可能写成「\/」（Swift 的 JSONEncoder），两种写法都换。
    /// 说了「不是密钥」的反过来：名字换回原文（按 JSON 字符串转义），误认替换掉的代码、文字就复原了
    pub fn mask_stored(&self, text: &str) -> String {
        let state = self.state.read().unwrap();
        let mut result = text.to_string();
        for (name, value) in &state.released {
            let reference = reference(name);
            if result.contains(reference.as_str()) {
                result = result.replace(reference.as_str(), &json_escaped(value));
            }
        }
        for (value, name) in &state.known {
            let reference = reference(name);
            if result.contains(value.as_str()) {
                result = result.replace(value.as_str(), &reference);
            }
            let escaped = value.replace('/', "\\/");
            if escaped != *value && result.contains(escaped.as_str()) {
                result = result.replace(escaped.as_str(), &reference);
            }
        }
        result
    }

    // MARK: - 认

    /// 认出文本里的密钥（不含用户说不是密钥的、`${secret:…}` 引用）。同一个值只报一次，名字取最准的那个。
    /// strong_only：只用最可靠的几条（格式、网址里的密码、请求头、「密码是…」），扫以前的存档时用——赋值类容易把旧代码里的测试值当密钥
    pub fn detect(&self, text: &str, strong_only: bool) -> Vec<Candidate> {
        if text.is_empty() {
            return Vec::new();
        }
        // 已经是引用的地方不算
        let references: Vec<(usize, usize)> = REFERENCE.find_iter(text).map(|m| (m.start(), m.end())).collect();
        let intersects = |a: (usize, usize), b: (usize, usize)| a.0.max(b.0) < a.1.min(b.1);
        let mut found: Vec<Candidate> = Vec::new();
        for rule in RULES.iter().filter(|rule| !strong_only || !matches!(rule.kind, Kind::Assignment | Kind::PrivateKey)) {
            for groups in rule.matches(text) {
                let span = |index: usize| groups.get(index).copied().flatten();
                let Some(range) = span(rule.value_group) else { continue };
                if references.iter().any(|reference| intersects(*reference, range)) {
                    continue;
                }
                let group_text = |index: usize| span(index).map_or("", |(start, end)| &text[start..end]);
                let value = &text[range.0..range.1];
                let name = match rule.kind {
                    Kind::Assignment | Kind::Phrase => {
                        if !plausible(value, if rule.kind == Kind::Phrase { 6 } else { 8 }, true, true) {
                            continue;
                        }
                        let left = rule.name_group.map_or("", group_text);
                        name_from_assignment(left).unwrap_or_else(|| "SECRET".to_string())
                    }
                    Kind::Url => {
                        if !plausible(value, 6, false, false) {
                            continue;
                        }
                        let (scheme, host) = (group_text(1), group_text(groups.len() - 1));
                        name_for_host(host, scheme, groups.len() == 4 || value.chars().count() >= 20)
                    }
                    Kind::Header => {
                        // 真的令牌够长或带数字（测试代码里的 Bearer my-test-token 不算）
                        if !plausible(value, 8, false, false) || !(value.chars().count() >= 16 || value.chars().any(char::is_numeric)) {
                            continue;
                        }
                        rule.fixed_name.unwrap_or("API_KEY").to_string()
                    }
                    Kind::Format => {
                        // 格式对还得像随机串：小写、大写、数字至少两类（自检代码里的 sk-abcdef… 不算；DeepSeek 的 sk-小写十六进制照样算）
                        if alphanumeric_classes(value) < 2 {
                            continue;
                        }
                        rule.fixed_name.unwrap_or("SECRET").to_string()
                    }
                    Kind::PrivateKey => rule.fixed_name.unwrap_or("SECRET").to_string(),
                };
                if self.is_ignored(value) {
                    continue;
                }
                let candidate = Candidate {
                    value: value.to_string(),
                    name,
                    start: range.0,
                    end: range.1,
                    from_context: matches!(rule.kind, Kind::Assignment | Kind::Phrase),
                };
                if let Some(existing) = found.iter_mut().find(|existing| existing.value == candidate.value) {
                    // 同一个值：上下文起的名字优先（OPENAI_API_KEY=sk-… 叫 OPENAI_API_KEY，不叫 API_KEY）
                    if candidate.from_context && !existing.from_context {
                        *existing = candidate;
                    }
                    continue;
                }
                // 和已认出的重叠（同一段被两条规则认出，取先认出的：规则按可靠程度排）
                if found.iter().any(|existing| intersects((existing.start, existing.end), range)) {
                    continue;
                }
                found.push(candidate);
            }
        }
        found
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Format,
    Assignment,
    Url,
    Header,
    PrivateKey,
    Phrase,
}

/// 一般用 fancy-regex（支持前后断言）；赋值规则用 regex（DFA：它是最慢的一条，见 ASSIGNMENT）。
/// 其余几条交给 regex 反而更慢（Unicode 的 \b 碰上中文时 regex 退回慢的引擎），实测过
enum Engine {
    Fancy(FancyRegex),
    Plain(Regex),
}

struct Rule {
    engine: Engine,
    /// 值在第几组（0 = 整个匹配）
    value_group: usize,
    /// 名字在第几组（赋值左边的名字）；None = 用 fixed_name 或按上下文起
    name_group: Option<usize>,
    fixed_name: Option<&'static str>,
    kind: Kind,
}

impl Rule {
    /// 依次每一处匹配的各组位置（字节；第 0 组是整个匹配）
    fn matches(&self, text: &str) -> Vec<Vec<Option<(usize, usize)>>> {
        let spans = |get: &dyn Fn(usize) -> Option<(usize, usize)>, count: usize| (0..count).map(get).collect::<Vec<_>>();
        match &self.engine {
            // 回溯超出上限（病态的长文本）的那一处起不再往后找，和 NSRegularExpression 超时一样放过
            Engine::Fancy(regex) => regex
                .captures_iter(text)
                .map_while(Result::ok)
                .map(|captures| spans(&|index| captures.get(index).map(|m| (m.start(), m.end())), captures.len()))
                .collect(),
            Engine::Plain(regex) => regex
                .captures_iter(text)
                .map(|captures| spans(&|index| captures.get(index).map(|m| (m.start(), m.end())), captures.len()))
                .collect(),
        }
    }
}

fn rule(pattern: &str, value_group: usize, name_group: Option<usize>, fixed_name: Option<&'static str>, kind: Kind) -> Rule {
    Rule { engine: Engine::Fancy(FancyRegex::new(pattern).unwrap()), value_group, name_group, fixed_name, kind }
}

static REFERENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\{secret:[A-Za-z_][A-Za-z0-9_]*\}").unwrap());

/// 前后不能紧挨着这些字符（否则是更长的一串里的一段）
const LEFT: &str = r"(?<![A-Za-z0-9_\-])";
const RIGHT: &str = r"(?![A-Za-z0-9_\-])";

/// 赋值规则。Swift / Qt 原来的写法在名字前后各有一个断言：`(?<![A-Za-z0-9_])` 名字前面不是字母数字、`(?![A-Za-z0-9_])` 密钥词后面不是。
/// 两个都是多余的（去掉后匹配完全一样，见测试 assignment_without_lookarounds）：名字的字符类包含断言里的字符，最左匹配总从这一串的开头起；
/// 密钥词后面只能是引号、空白、冒号或等号。去掉后能交给 regex（19 MB 的文本从 6 秒多降到 0.2 秒左右）
const ASSIGNMENT: &str = r#"(?i)([A-Za-z0-9_.\-]*(?:api[_\-]?key|apikey|secret[_\-]?key|secret|token|passw(?:or)?d|passwd|pwd|access[_\-]?key|private[_\-]?key|encryption[_\-]?key|signing[_\-]?key|master[_\-]?key|credentials?|auth[_\-]?key))["']?\s*(?::=|=>|[:=])\s*["']?([A-Za-z0-9!#$%&*+\-./:=?@^_|~]{6,})"#;

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    use Kind::*;
    let format = |pattern: &str, name: &'static str| rule(pattern, 0, None, Some(name), Format);
    vec![
        // 私钥块
        rule(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]+?-----END [A-Z0-9 ]*PRIVATE KEY-----", 0, None, Some("PRIVATE_KEY"), PrivateKey),
        // 已知格式（前缀）
        format(&format!(r"{LEFT}sk-ant-[A-Za-z0-9_\-]{{20,}}"), "ANTHROPIC_API_KEY"),
        format(&format!(r"{LEFT}sk-(?:proj-)?[A-Za-z0-9_\-]{{20,}}{RIGHT}"), "API_KEY"),
        format(&format!(r"{LEFT}gh[pousr]_[A-Za-z0-9]{{30,}}{RIGHT}"), "GITHUB_TOKEN"),
        format(&format!(r"{LEFT}github_pat_[A-Za-z0-9_]{{40,}}{RIGHT}"), "GITHUB_TOKEN"),
        format(&format!(r"{LEFT}glpat-[A-Za-z0-9_\-]{{20,}}{RIGHT}"), "GITLAB_TOKEN"),
        format(&format!(r"{LEFT}(?:AKIA|ASIA)[0-9A-Z]{{16}}{RIGHT}"), "AWS_ACCESS_KEY_ID"),
        format(&format!(r"{LEFT}AIza[0-9A-Za-z_\-]{{35}}{RIGHT}"), "GOOGLE_API_KEY"),
        format(&format!(r"{LEFT}xox[baprs]-[A-Za-z0-9\-]{{10,}}{RIGHT}"), "SLACK_TOKEN"),
        format(&format!(r"{LEFT}hf_[A-Za-z0-9]{{30,}}{RIGHT}"), "HF_TOKEN"),
        format(&format!(r"{LEFT}npm_[A-Za-z0-9]{{36}}{RIGHT}"), "NPM_TOKEN"),
        format(&format!(r"{LEFT}pypi-[A-Za-z0-9_\-]{{50,}}{RIGHT}"), "PYPI_TOKEN"),
        format(&format!(r"{LEFT}(?:sk|rk)_live_[A-Za-z0-9]{{20,}}{RIGHT}"), "STRIPE_KEY"),
        // 智谱：32 位十六进制.16 位字母数字
        format(&format!(r"{LEFT}[0-9a-f]{{32}}\.[A-Za-z0-9]{{16}}{RIGHT}"), "ZHIPU_API_KEY"),
        // JWT
        format(&format!(r"{LEFT}eyJ[A-Za-z0-9_\-]{{10,}}\.eyJ[A-Za-z0-9_\-]{{10,}}\.[A-Za-z0-9_\-]{{10,}}"), "JWT_TOKEN"),
        // 网址里的用户名:密码@
        rule(r#"(?i)\b([a-z][a-z0-9+.\-]*)://([^\s/:@"'<>]+):([^\s/@"'<>]{4,})@([A-Za-z0-9.\-]+)"#, 3, None, None, Url),
        // 网址里只有一长串用户名@（https://令牌@github.com 这种）
        rule(r"(?i)\b(https?)://([A-Za-z0-9_\-]{20,})@([A-Za-z0-9.\-]+)", 2, None, None, Url),
        // 请求头
        rule(
            r#"(?i)\b(?:proxy-)?authorization["']?\s*[:=]\s*["']?(?:bearer|token|basic)\s+([A-Za-z0-9._~+/=\-]{8,})"#,
            1,
            None,
            Some("AUTH_TOKEN"),
            Header,
        ),
        rule(
            r#"(?i)\b(?:x-api-key|api-key|x-auth-token|x-access-token|x-goog-api-key)["']?\s*[:=]\s*["']?([A-Za-z0-9._~+/=\-]{8,})"#,
            1,
            None,
            Some("API_KEY"),
            Header,
        ),
        // 赋值：左边的名字像密钥（KEY=、"password": "…"、api_key: …）
        // 名字要以密钥词结尾（GITHUB_TOKEN、client_secret、DB_PASSWORD；tokensPerSecond、SecretStore、secretName、maxTokens 不算），
        // 值只取 ASCII（后面跟着中文说明的不算进值里）
        Rule {
            engine: Engine::Plain(Regex::new(ASSIGNMENT).unwrap()),
            value_group: 2,
            name_group: Some(1),
            fixed_name: None,
            kind: Assignment,
        },
        // 中文：「密码是 xxx」「密钥：xxx」
        // 值只取 ASCII、不含括号引号冒号（中文注释「令牌：SecretStore::substitute(…)」里的代码不算）
        rule(r#"(密钥|密码|令牌|口令)\s*(?:是|为|[:：=])\s*["'“「]?([A-Za-z0-9!#$%&*+\-./=?@^_|~]{6,})"#, 2, Some(1), None, Phrase),
    ]
});

// MARK: - 判断与起名

/// 赋值 / 「密码是…」右边的值像不像真的密钥（不是占位符、变量名、表达式）。
/// code：赋值（多半出现在 Agent 读到的代码、配置、统计数据里）：开头得是字母数字，不以 . - : / _ , 结尾（截断的），
/// 不是数字（tokensPerSecond = 12.5），不带 :: -> ? :（代码里的表达式、参数标签）
pub fn plausible(value: &str, minimum: usize, require_mix: bool, code: bool) -> bool {
    static CONSTANT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Z][A-Z0-9_]*$").unwrap());
    static MEMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*(\.[A-Za-z_][A-Za-z0-9_]*)+(\(\))?$").unwrap());
    static IDENTIFIER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z_]+$").unwrap());
    static WORDS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z]+([-_][a-z]+)+$").unwrap());

    let count = value.chars().count();
    if count < minimum || count > 4096 {
        return false;
    }
    if code {
        let (Some(first), Some(last)) = (value.chars().next(), value.chars().last()) else { return false };
        if !(first.is_alphabetic() || first.is_numeric() || "_+/".contains(first)) {
            return false;
        }
        if ".-:/_,".contains(last) {
            return false;
        }
        if value.chars().all(|c| c.is_numeric() || ".eE+-_".contains(c)) {
            return false;
        }
        if value.contains("::") || value.contains("->") || value.contains('?') || value.contains(':') {
            return false;
        }
    }
    if ["$", "%", "{{", "<", "&"].iter().any(|prefix| value.starts_with(prefix)) {
        return false;
    }
    if value.contains("secret:") {
        return false;
    }
    // 环境变量名（SECRET_NAME: ZHIPU_API_KEY）、a.b.c 这样的表达式（process.env.API_KEY）
    if CONSTANT.is_match(value) || MEMBER.is_match(value) {
        return false;
    }
    // 只有字母和下划线的是标识符（token = get_token()、password: hunterTwo 这类不当密钥）
    if IDENTIFIER.is_match(value) {
        return false;
    }
    let lowered = value.to_lowercase();
    // 文档里的示例写法（https://user:password@host）
    if ["password", "passwd", "secret", "token", "pass", "username", "apikey", "api_key", "x-oauth-basic"].contains(&lowered.as_str()) {
        return false;
    }
    const PLACEHOLDERS: &[&str] = &[
        "your",
        "xxxx",
        "example",
        "placeholder",
        "changeme",
        "change_me",
        "dummy",
        "redacted",
        "replace",
        "sample",
        "****",
        "•••",
        "...",
        "<",
        "none",
        "null",
        "undefined",
        "true",
        "false",
        "required",
        "optional",
        "string",
    ];
    if PLACEHOLDERS.iter().any(|placeholder| lowered.contains(placeholder)) {
        return false;
    }
    if value.chars().collect::<HashSet<_>>().len() < 4 {
        return false;
    }
    // 小写单词用连字符 / 下划线连起来的（my-test-token、x-oauth-basic）是写给人看的名字，不是密钥
    if WORDS.is_match(value) {
        return false;
    }
    if !require_mix {
        return true;
    }
    let classes = [
        value.chars().any(char::is_lowercase),
        value.chars().any(char::is_uppercase),
        value.chars().any(char::is_numeric),
        value.chars().any(|c| !c.is_alphabetic() && !c.is_numeric()),
    ]
    .iter()
    .filter(|present| **present)
    .count();
    // 全是字母的单词（password: required、token: refresh）不算；够长的十六进制串（32 位以上）算
    classes >= 2 || (count >= 32 && value.chars().all(|c| c.is_ascii_hexdigit()))
}

/// 小写、大写、数字有几类
pub fn alphanumeric_classes(value: &str) -> usize {
    [value.chars().any(char::is_lowercase), value.chars().any(char::is_uppercase), value.chars().any(char::is_numeric)]
        .iter()
        .filter(|present| **present)
        .count()
}

/// 赋值左边的名字 → 密钥名：apiKey → API_KEY，openai-api-key → OPENAI_API_KEY，密码 → PASSWORD；起不出合法的名字返回 None
pub fn name_from_assignment(raw: &str) -> Option<String> {
    match raw {
        "密钥" => return Some("SECRET_KEY".to_string()),
        "密码" | "口令" => return Some("PASSWORD".to_string()),
        "令牌" => return Some("TOKEN".to_string()),
        _ => {}
    }
    let mut split = String::new();
    let mut previous: Option<char> = None;
    for character in raw.chars() {
        if character.is_uppercase() && previous.is_some_and(|previous| previous.is_lowercase() || previous.is_numeric()) {
            split.push('_');
        }
        split.push(character);
        previous = Some(character);
    }
    let mut result: String = split.to_uppercase().chars().map(|c| if c.is_alphabetic() || c.is_numeric() { c } else { '_' }).collect();
    while result.contains("__") {
        result = result.replace("__", "_");
    }
    let mut result = result.trim_matches('_').to_string();
    if result.chars().next().is_some_and(char::is_numeric) {
        result = format!("K_{result}");
    }
    if result.chars().count() > 60 {
        result = result.chars().take(60).collect();
    }
    is_valid_name(&result).then_some(result)
}

/// 网址里的密钥按主机起名：github.com → GITHUB_TOKEN，db.example.com 的密码 → EXAMPLE_PASSWORD；本机 / IP 按协议（POSTGRES_PASSWORD）
pub fn name_for_host(host: &str, scheme: &str, token: bool) -> String {
    let lowered = host.to_lowercase();
    let labels: Vec<&str> = lowered.split('.').filter(|label| !label.is_empty()).collect();
    let is_address = labels.iter().all(|label| label.chars().all(char::is_numeric)) || lowered == "localhost" || labels.len() < 2;
    let mut base = if is_address { scheme } else { labels[labels.len() - 2] };
    if ["com", "co", "org", "net"].contains(&base) && labels.len() >= 3 {
        base = labels[labels.len() - 3];
    }
    let suffix = if token { "_TOKEN" } else { "_PASSWORD" };
    name_from_assignment(base).unwrap_or_else(|| "SERVICE".to_string()) + suffix
}

/// == Swift `addingPercentEncoding(withAllowedCharacters: .urlQueryAllowed.subtracting("&+=?/:@"))`：
/// 字母、数字和 - . _ ~ ! $ ' ( ) * , ; 原样，其余按 UTF-8 写成 %XX
fn percent_encoded(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~!$'()*,;".contains(&byte) {
            result.push(byte as char);
        } else {
            result.push_str(&format!("%{byte:02X}"));
        }
    }
    result
}

/// 值写成 JSON 字符串时引号里面的样子（== Swift JSONEncoder：「/」也转义成「\/」）
fn json_escaped(value: &str) -> String {
    let quoted = serde_json::to_string(value).unwrap_or_default();
    quoted[1..quoted.len() - 1].replace('/', "\\/")
}

/// 先写到同目录的临时文件再换名（== Swift `.atomic`），写到一半不会留下半截文件
fn write_atomically(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("file");
    let temporary = path.with_file_name(format!(".{name}.friday-core-{}", std::process::id()));
    std::fs::write(&temporary, data)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

/// 字节下标 → UTF-16 下标（两端的字符串下标都按 UTF-16）
pub fn utf16_offset(text: &str, byte: usize) -> usize {
    text[..byte].encode_utf16().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(guard: &Guard, text: &str) -> Vec<String> {
        guard.detect(text, false).into_iter().map(|candidate| candidate.name).collect()
    }

    fn values(guard: &Guard, text: &str) -> Vec<String> {
        guard.detect(text, false).into_iter().map(|candidate| candidate.value).collect()
    }

    const GITHUB: &str = concat!("ghp_", "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789");

    /// == Swift 自检 secretGuard 和 Qt TestModels::secretGuard 的同一组
    #[test]
    fn detection() {
        let guard = Guard::default();
        let remote = format!("origin\thttps://integemjack:{GITHUB}@github.com/integemjack/Friday.git (fetch)");
        assert_eq!(values(&guard, &remote), [GITHUB]);
        assert_eq!(names(&guard, &remote), ["GITHUB_TOKEN"]);
        let openai = concat!("sk-proj-", "AbCdEfGhIjKlMnOpQrSt12345");
        assert_eq!(names(&guard, &format!("export OPENAI_API_KEY={openai}")), ["OPENAI_API_KEY"]);
        assert_eq!(values(&guard, r#"curl -H "Authorization: Bearer abcDEF123456ghiJKL" https://api.x.com"#), ["abcDEF123456ghiJKL"]);
        let postgres = "postgres://admin:S3cr3tPass!@db.example.com:5432/app";
        assert_eq!(values(&guard, postgres), ["S3cr3tPass!"]);
        assert_eq!(names(&guard, postgres), ["EXAMPLE_PASSWORD"]);
        assert_eq!(values(&guard, "我的密码是 hunter2abc，帮我登录"), ["hunter2abc"]);
        assert_eq!(names(&guard, "我的密码是 hunter2abc，帮我登录"), ["PASSWORD"]);
        assert_eq!(names(&guard, r#"{"apiKey": "AbC123dEf456GhI789"}"#), ["API_KEY"]);
        assert_eq!(names(&guard, concat!("key sk-", "0123456789abcdef0123456789abcdef")), ["API_KEY"]);
        assert_eq!(names(&guard, "DB_PASSWORD=Hunter2xyz9!"), ["DB_PASSWORD"]);
        assert_eq!(names(&guard, "SECRET_KEY='django-insecure-8f3k2l9x0q1w'"), ["SECRET_KEY"]);
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----";
        assert_eq!(names(&guard, &format!("key:\n{pem}")), ["PRIVATE_KEY"]);
        assert_eq!(names(&guard, "密钥 0123456789abcdef0123456789abcdef.AbCdEfGhIjKlMnOp 已配置"), ["ZHIPU_API_KEY"]);
        // 只扫最可靠的：赋值、私钥块不算
        assert!(guard.detect(&format!("key:\n{pem}"), true).is_empty());
        assert!(guard.detect("DB_PASSWORD=Hunter2xyz9!", true).is_empty());
        assert_eq!(guard.detect(&remote, true).len(), 1);
        // 长得随机但不是密钥的、示例写法、引用：都不认
        for text in [
            "commit 3f2a9c1d8e7b6a5f4e3d2c1b0a9f8e7d6c5b4a39",
            "id: 550e8400-e29b-41d4-a716-446655440000",
            r#""apiKeySecret": "ZHIPU_API_KEY""#,
            "token = get_token()",
            "api_key = os.environ.get",
            "max_tokens: 4096",
            "password: ${secret:DB_PASSWORD}",
            "https://user:password@example.com",
            "Authorization: Bearer ${secret:GITHUB_TOKEN}",
            r#""integrity": "sha512-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789==""#,
            "risk-assessment-framework-for-teams",
            "token: string",
            "password: required",
            // 在真实存档里试扫出来的误认（Agent 读到的代码、统计数据）
            "tokensPerSecond = 12.5",
            "maxTokens = m_settings.x",
            "secrets: capabilities:",
            "secretName = GITHUB_TOKEN",
            "SecretStore::names()",
            "secret = DASHSCOPE_API_KEY；异步请求头",
            "apiKey: example-",
            // Friday 移植代码时的历史里扫出来的误认：中文注释后面跟着代码、测试代码里的假令牌
            "令牌：SecretStore::substitute(text)",
            "密钥是 resolve(x)",
            "Authorization: Bearer my-test-token",
            "密钥是 sk-abcdefghijklmnopqrstuvwxyz",
        ] {
            assert!(guard.detect(text, false).is_empty(), "不是密钥：{text} → {:?}", names(&guard, text));
        }
    }

    #[test]
    fn positions_are_utf16() {
        let guard = Guard::default();
        let text = "😀密码是 hunter2abc".to_string();
        let candidate = &guard.detect(&text, false)[0];
        assert_eq!(&text[candidate.start..candidate.end], "hunter2abc");
        // 😀 是两个 UTF-16 单位，「密码是 」四个
        assert_eq!(utf16_offset(&text, candidate.start), 6);
    }

    #[test]
    fn masking() {
        let guard = Guard::default();
        let values = HashMap::from([("GITHUB_TOKEN".to_string(), GITHUB.to_string()), ("SHORT".to_string(), "ab12cd".to_string())]);
        assert_eq!(guard.update(&values, HashSet::new(), &HashMap::new()), 1);
        assert_eq!(guard.mask(&format!("看 {GITHUB} 和 ab12cd")), "看 ${secret:GITHUB_TOKEN} 和 ab12cd");
        // URL 编码过的写法
        let values = HashMap::from([("PASS".to_string(), "p@ss/w0rd!x".to_string())]);
        guard.update(&values, HashSet::new(), &HashMap::new());
        assert_eq!(guard.mask("u=p%40ss%2Fw0rd!x"), "u=${secret:PASS}");
        // 请求体里每个字符串
        let values = HashMap::from([("GITHUB_TOKEN".to_string(), GITHUB.to_string())]);
        guard.update(&values, HashSet::new(), &HashMap::new());
        let body = serde_json::json!({"messages": [{"role": "user", "content": format!("看 {GITHUB}")}], "n": 1});
        assert_eq!(
            guard.mask_json(body),
            serde_json::json!({"messages": [{"role": "user", "content": "看 ${secret:GITHUB_TOKEN}"}], "n": 1})
        );
        // 存档里的明文（含 \/ 的写法）
        assert_eq!(guard.mask_stored(&format!(r#"{{"text":"{GITHUB}"}}"#)), r#"{"text":"${secret:GITHUB_TOKEN}"}"#);
        let url = "https://x/y?t=AbCdEf123456";
        guard.update(&HashMap::from([("URL_TOKEN".to_string(), url.to_string())]), HashSet::new(), &HashMap::new());
        assert_eq!(guard.mask_stored(r#"{"u":"https:\/\/x\/y?t=AbCdEf123456"}"#), r#"{"u":"${secret:URL_TOKEN}"}"#);
    }

    #[test]
    fn not_a_secret() {
        let guard = Guard::default();
        let remote = format!("https://x:{GITHUB}@github.com");
        let released = HashMap::from([("GITHUB_TOKEN".to_string(), "a/b\"c".to_string())]);
        guard.update(&HashMap::new(), HashSet::from([hash(GITHUB)]), &released);
        assert!(guard.is_ignored(GITHUB));
        assert!(guard.detect(&remote, false).is_empty());
        // 下次启动时存档里的名字换回原文（按 JSON 字符串转义）
        assert_eq!(guard.mask_stored(r#"{"t":"${secret:GITHUB_TOKEN}"}"#), r#"{"t":"a\/b\"c"}"#);
        assert_eq!(hash("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn scrub_files() {
        let directory = std::env::temp_dir().join(format!("friday-core-scrub-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("conversations.json");
        std::fs::write(&path, format!(r#"[{{"text":"token {GITHUB} and https:\/\/x:{GITHUB}@github.com"}}]"#)).unwrap();
        let guard = Guard::default();
        guard.update(&HashMap::from([("GITHUB_TOKEN".to_string(), GITHUB.to_string())]), HashSet::new(), &HashMap::new());
        guard.scrub_stored_files(&directory);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(GITHUB));
        assert_eq!(text.matches("${secret:GITHUB_TOKEN}").count(), 2);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    /// 原来带断言的写法（Swift / Qt）和去掉断言的 ASSIGNMENT 在各种写法上匹配完全一样
    #[test]
    fn assignment_without_lookarounds() {
        let original = FancyRegex::new(r#"(?i)(?<![A-Za-z0-9_])([A-Za-z0-9_.\-]*(?:api[_\-]?key|apikey|secret[_\-]?key|secret|token|passw(?:or)?d|passwd|pwd|access[_\-]?key|private[_\-]?key|encryption[_\-]?key|signing[_\-]?key|master[_\-]?key|credentials?|auth[_\-]?key))(?![A-Za-z0-9_])["']?\s*(?::=|=>|[:=])\s*["']?([A-Za-z0-9!#$%&*+\-./:=?@^_|~]{6,})"#).unwrap();
        let plain = Regex::new(ASSIGNMENT).unwrap();
        let samples = [
            "export OPENAI_API_KEY=sk-AbCdEf123456",
            "x.api_key=AbCd1234efgh",
            "foo bar_token=Abc123def",
            "tokens=abc123def",
            "tokensPerSecond = 12.5",
            "maxTokens: 4096",
            "secretName = GITHUB_TOKEN",
            "a-b.token := Qw12Er34Ty",
            "Token => 'Zx98Cv76Bn'",
            r#"{"apiKey": "AbC123dEf456GhI789", "password":"Hunter2xyz9!"}"#,
            "token=Abc123xyz9password=Qwe456rty7",
            "_token=Abc12345",
            "9token=Abc12345",
            "passwd:abcdef1 pwd=XyZ987abc",
            "my-secret-key = abcDEF12345",
            "credentials: AbcDef123456",
            "SECRETx=Abc12345",
            "aTOKEN=Abc12345",
            "token = get_token()",
            "\u{4e2d}token=Abc12345",
            "token\t=\t\"AbCd1234\"",
            "-token=Abc12345",
            ".token=Abc12345",
            "keytoken=Abc12345token=Def67890",
            "api-key-token=ab12CD34ef",
            "x__password__=Abc12345!",
            "AUTH_KEY:=Ab12Cd34Ef",
        ];
        let corpus = samples.join("\n") + " " + &samples.join(" ") + &samples.concat();
        for text in samples.iter().map(|text| text.to_string()).chain([corpus]) {
            let a: Vec<_> = original
                .captures_iter(&text)
                .map(|c| c.unwrap())
                .map(|c| (c.get(0).map(|m| m.range()), c.get(1).map(|m| m.range()), c.get(2).map(|m| m.range())))
                .collect();
            let b: Vec<_> = plain
                .captures_iter(&text)
                .map(|c| (c.get(0).map(|m| m.range()), c.get(1).map(|m| m.range()), c.get(2).map(|m| m.range())))
                .collect();
            assert_eq!(a, b, "{text}");
        }
    }

    #[test]
    fn names_and_helpers() {
        assert_eq!(name_from_assignment("apiKey").as_deref(), Some("API_KEY"));
        assert_eq!(name_from_assignment("openai-api-key").as_deref(), Some("OPENAI_API_KEY"));
        assert_eq!(name_from_assignment("2fa.token").as_deref(), Some("K_2FA_TOKEN"));
        assert_eq!(name_from_assignment("密码").as_deref(), Some("PASSWORD"));
        assert_eq!(name_from_assignment("___"), None);
        assert_eq!(name_for_host("github.com", "https", true), "GITHUB_TOKEN");
        assert_eq!(name_for_host("db.example.com", "postgres", false), "EXAMPLE_PASSWORD");
        assert_eq!(name_for_host("api.example.co.uk", "https", false), "EXAMPLE_PASSWORD");
        assert_eq!(name_for_host("127.0.0.1", "postgres", false), "POSTGRES_PASSWORD");
        assert_eq!(name_for_host("localhost", "redis", false), "REDIS_PASSWORD");
        assert!(is_valid_name("API_KEY") && is_valid_name("_A") && is_valid_name(&"A".repeat(64)));
        assert!(!is_valid_name(&"A".repeat(65)) && !is_valid_name("A") && !is_valid_name("1ABC"));
        assert!(!is_valid_name("api_key") && !is_valid_name("API-KEY") && !is_valid_name("API_KEY\n"));
        assert_eq!(percent_encoded("a b/c@d!"), "a%20b%2Fc%40d!");
        assert_eq!(alphanumeric_classes("abcDEF"), 2);
    }
}
