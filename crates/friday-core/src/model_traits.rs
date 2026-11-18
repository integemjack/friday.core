//! v2.5 模型的档位（快 / 均衡 / 强）、计费（套餐内 / 按量 / 免费）和「是不是对话模型」：从模型名、服务名、接口地址推断，
//! 用户可以在模型的高级设置里改（engines.json 的 tier / billing，没有 = 按推断）。用在：子智能体按活自动选模型、额度用完时换一个同档的接着做、
//! 小活用的模型、派发工具的说明、导入模型时过滤掉向量、重排、生图、语音这些不能对话的模型。
//!
//! 原来是 Swift `LLM/Engine/ModelTraits.swift` 和 Qt `llm/engine/ModelTraits.cpp` 各写一份。

use std::sync::LazyLock;

use regex::Regex;

const STRONG_WORDS: &[&str] = &["max", "pro", "ultra", "opus", "large", "reasoner", "r1"];
const FAST_WORDS: &[&str] =
    &["flash", "flashx", "lite", "mini", "nano", "small", "tiny", "micro", "turbo", "lightning", "haiku", "air", "airx", "instant", "xs"];
/// 名字里任何位置出现就不是对话模型
const ANYWHERE: &[&str] = &[
    "embed",
    "rerank",
    "retriever",
    "guard",
    "safety",
    "reward",
    "moderation",
    "whisper",
    "deplot",
    "detector",
    "calibration",
    "cogview",
    "cogvideo",
    "dall-e",
    "stable-diffusion",
    "seedream",
    "seedance",
    "kolors",
    "nvclip",
    "realtime",
    "cosyvoice",
    "sambert",
    "paraformer",
    "sensevoice",
    "transcribe",
];
/// 单独成段才算（避免误伤：sparse 里的 parse、eclipse 里的 clip）
const WHOLE: &[&str] = &["tts", "asr", "image", "video", "clip", "parse", "sora", "flux", "sdxl", "wanx"];

/// Swift 的 `.whitespaces`：Unicode Zs 类 + 制表符
fn is_space(c: char) -> bool {
    matches!(c, '\t' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}')
}

/// 模型名的最后一段（openai/gpt-4o-mini → gpt-4o-mini，去掉 :free 这类后缀），小写
fn base_name(model: &str) -> String {
    let lower = model.to_lowercase();
    let last = lower.rsplit('/').next().unwrap_or("");
    last.split(':').next().unwrap_or("").trim_matches(is_space).to_string()
}

fn tokens(name: &str) -> Vec<&str> {
    name.split(['-', '_', '.', ' ']).filter(|token| !token.is_empty()).collect()
}

/// 名字里写的参数量（十亿）：gemma-3-12b-it → 12；nemotron-3-super-120b-a12b → 120（a12b 是激活参数，不算）
fn parameter_billions(name: &str) -> Option<f64> {
    static PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:^|[-_.])([0-9]+(?:\.[0-9]+)?)b(?:$|[-_.])").unwrap());
    PATTERN.captures(name).and_then(|captures| captures[1].parse().ok())
}

/// 档位：fast / balanced / strong。名字里有 max、pro、ultra、opus、large、reasoner、r1 的算强；flash、lite、mini、nano、small、
/// turbo、lightning、haiku、air 这类算快；都没有时看参数量（≤ 32B 快，≥ 400B 强），其余均衡
pub fn infer_tier(model: &str) -> &'static str {
    let name = base_name(model);
    let words = tokens(&name);
    if words.iter().any(|word| STRONG_WORDS.contains(word)) {
        return "strong";
    }
    if words.iter().any(|word| FAST_WORDS.contains(word)) {
        return "fast";
    }
    match parameter_billions(&name) {
        Some(billions) if billions <= 32.0 => "fast",
        Some(billions) if billions >= 400.0 => "strong",
        _ => "balanced",
    }
}

/// 计费：plan / metered / free。Token Plan / Coding Plan 的地址算套餐内；:free、「免费」、本机地址算免费；其余按量
pub fn infer_billing(base_url: &str, title: &str, model: &str) -> &'static str {
    let url = base_url.to_lowercase();
    let lower_model = model.to_lowercase();
    if lower_model.ends_with(":free") || lower_model == "openrouter/free" || title.contains("免费") || is_local_address(base_url) {
        return "free";
    }
    // 百炼 Token Plan（token-plan.…qianwenaiapi.com）、智谱 / Kimi / 百炼的 Coding Plan（地址里带 coding）
    if url.contains("token-plan") || url.contains("coding") || title.contains("套餐") || title.to_lowercase().contains("plan") {
        return "plan";
    }
    "metered"
}

/// 能不能拿来对话：向量（embed）、重排（rerank）、检索、安全审核（guard、safety）、奖励模型、生图生视频（image、video、
/// cogview、wan2.x）、语音（tts、asr、realtime）、文档解析（parse）这些不是对话模型
pub fn is_chat_model(model: &str) -> bool {
    static WAN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^wan[0-9].*$").unwrap());
    let name = base_name(model);
    if name.is_empty() {
        return true;
    }
    if ANYWHERE.iter().any(|word| name.contains(word)) {
        return false;
    }
    !tokens(&name).iter().any(|token| WHOLE.contains(token) || WAN.is_match(token))
}

/// 接口地址里的主机（小写；IPv6 去掉方括号）；不是「协议://主机」的写法返回 None
pub fn url_host(url: &str) -> Option<String> {
    let rest = url.trim_matches(is_space).split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or("");
    let host = if let Some(inner) = host_port.strip_prefix('[') {
        inner.split(']').next().unwrap_or("")
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    (!host.is_empty()).then(|| host.to_lowercase())
}

fn is_local_address(base_url: &str) -> bool {
    let host = url_host(base_url).unwrap_or_default();
    ["localhost", "127.0.0.1", "::1", "0.0.0.0"].contains(&host.as_str()) || host.ends_with(".local")
}

/// 模型属于哪一系列、第几版：系列 = 名字里的字母段（去掉版本前缀 v），版本 = 数字段（qwen3.8-max → qwen-max / [3, 8]；
/// Opus 5.5 → opus / [5, 5]；deepseek-v4.1-flash-1017 → deepseek-flash / [4, 1]，1017 这类 4 位以上的数是日期，只在版本一样时比）。
/// 方括号、圆括号里的（opus[1m]、Opus 5 (1M)）和参数量（235b、a22b、128k）不算
#[derive(Debug, PartialEq, Eq)]
pub struct Lineage {
    pub family: String,
    pub version: Vec<u64>,
    pub date: u64,
}

pub fn lineage(name: &str) -> Lineage {
    static SIZE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:[0-9]+[bkm]|a[0-9]+b)$").unwrap());
    let mut plain = String::new();
    let mut depth = 0usize;
    for c in base_name(name).chars() {
        match c {
            '[' | '(' | '（' => depth += 1,
            ']' | ')' | '）' => depth = depth.saturating_sub(1),
            _ if depth == 0 => plain.push(c),
            _ => {}
        }
    }
    let mut family: Vec<String> = Vec::new();
    let mut version = Vec::new();
    let mut date = 0;
    for token in plain.split(|c: char| !c.is_alphanumeric()).filter(|token| !token.is_empty() && !SIZE.is_match(token)) {
        // 字母和数字分开：qwen3 → qwen 3、v4 → v 4、k2 → k 2
        let mut pieces: Vec<String> = Vec::new();
        for c in token.chars() {
            match pieces.last_mut() {
                Some(last) if last.chars().last().is_some_and(|prev| prev.is_ascii_digit() == c.is_ascii_digit()) => last.push(c),
                _ => pieces.push(c.to_string()),
            }
        }
        for piece in pieces {
            if piece.chars().all(|c| c.is_ascii_digit()) {
                let number = piece.parse().unwrap_or(u64::MAX);
                if piece.len() >= 4 { date = number } else { version.push(number) }
            } else if piece != "v" {
                family.push(piece);
            }
        }
    }
    Lineage { family: family.join("-"), version, date }
}

/// 换成哪一个：candidates 里和 from 同一系列、版本最新的那个（newer：还要比 from 新）；系列认不出、没有同系列的返回 None。
/// 用在订阅的模型列表刷新后：新对话默认的模型没有了换成它的后继，同系列出了新版本默认换成新版本
pub fn successor(from: &str, candidates: &[&str], newer: bool) -> Option<usize> {
    let origin = lineage(from);
    if origin.family.is_empty() {
        return None;
    }
    let mut best: Option<(usize, Lineage)> = None;
    for (index, name) in candidates.iter().enumerate() {
        let found = lineage(name);
        if found.family != origin.family || (newer && (&found.version, found.date) <= (&origin.version, origin.date)) {
            continue;
        }
        if best.as_ref().is_none_or(|(_, top)| (&found.version, found.date) > (&top.version, top.date)) {
            best = Some((index, found));
        }
    }
    best.map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers() {
        for (model, tier) in [
            ("qwen3.7-max", "strong"),
            ("openai/gpt-4o-mini", "fast"),
            ("glm-4.5-air", "fast"),
            ("deepseek-reasoner", "strong"),
            ("gemma-3-12b-it", "fast"),
            ("nemotron-3-super-120b-a12b", "balanced"),
            ("llama-3.1-405b-instruct", "strong"),
            ("kimi-k2", "balanced"),
            ("claude-opus-4", "strong"),
            ("qwen3-coder-480b-a35b", "strong"),
            ("Qwen/QwQ-32B:free", "fast"),
        ] {
            assert_eq!(infer_tier(model), tier, "{model}");
        }
    }

    #[test]
    fn billings() {
        assert_eq!(infer_billing("https://token-plan.cn-beijing.maas.qianwenaiapi.com/v1", "百炼", "qwen3.7-max"), "plan");
        assert_eq!(infer_billing("https://open.bigmodel.cn/api/coding/paas/v4", "智谱", "glm-5"), "plan");
        assert_eq!(infer_billing("https://api.x.com", "Kimi Coding PLAN", "k2"), "plan");
        assert_eq!(infer_billing("https://openrouter.ai/api/v1", "OpenRouter", "qwen/qwen3:free"), "free");
        assert_eq!(infer_billing("http://localhost:11434/v1", "Ollama", "qwen3"), "free");
        assert_eq!(infer_billing("http://mac.local:1234/v1", "LM Studio", "x"), "free");
        assert_eq!(infer_billing("https://api.deepseek.com", "DeepSeek", "deepseek-chat"), "metered");
    }

    #[test]
    fn chat_models() {
        for model in ["text-embedding-v4", "gte-rerank", "qwen-tts", "cogview-4", "wan2.1-t2v", "flux-dev", "paraformer-v2", "qwen3-asr"] {
            assert!(!is_chat_model(model), "{model}");
        }
        for model in ["qwen3-max", "deepseek-chat", "sparse-chat", "eclipse-7b", "", "wanx2"] {
            // wanx2 不是单独的「wanx」，也不是 wan 加数字
            assert!(is_chat_model(model), "{model}");
        }
        assert!(!is_chat_model("wanx"));
    }

    #[test]
    fn lineages() {
        let of = |name: &str| {
            let found = lineage(name);
            (found.family, found.version, found.date)
        };
        assert_eq!(of("qwen3.8-max"), ("qwen-max".into(), vec![3, 8], 0));
        assert_eq!(of("Opus 5.5"), ("opus".into(), vec![5, 5], 0));
        assert_eq!(of("Opus 5 (1M)"), ("opus".into(), vec![5], 0));
        assert_eq!(of("opus[1m]"), ("opus".into(), vec![], 0));
        assert_eq!(of("deepseek-v4.1-flash-1017"), ("deepseek-flash".into(), vec![4, 1], 1017));
        assert_eq!(of("claude-haiku-4-5-20251001"), ("claude-haiku".into(), vec![4, 5], 20251001));
        assert_eq!(of("qwen/qwen3-235b-a22b-2507:free"), ("qwen".into(), vec![3], 2507));
        assert_eq!(of("glm-4.5-air"), ("glm-air".into(), vec![4, 5], 0));
        assert_eq!(of("deepseek-r1"), ("deepseek-r".into(), vec![1], 0));
        assert_eq!(of("默认"), ("默认".into(), vec![], 0));
        assert_eq!(of("1.5"), ("".into(), vec![1, 5], 0));
    }

    #[test]
    fn successors() {
        // Claude Code 换了一批：原来的 Opus 5 (1M) 没了，换成同系列最新的
        let claude = ["默认", "Opus 5.5", "Fable 5.2", "Sonnet 5.5", "Haiku 5.5", "Opus 5", "Opus 4.8", "Haiku 4.5"];
        assert_eq!(successor("Opus 5 (1M)", &claude, false), Some(1));
        assert_eq!(successor("Haiku 4.5", &claude, true), Some(4));
        // 已经是最新的：没有更新的
        assert_eq!(successor("Opus 5.5", &claude, true), None);
        assert_eq!(successor("Gemini 3", &claude, false), None);
        // 套餐里出了新版本
        let plan = ["qwen3.9-flash", "qwen3.9-max", "glm-5.4"];
        assert_eq!(successor("qwen3.8-max", &plan, true), Some(1));
        assert_eq!(successor("glm-5.3", &plan, true), Some(2));
        assert_eq!(successor("glm-5.3-flash", &plan, true), None);
        // 小版本号大过日期：v4.1 比 v4 新；版本一样时比日期
        assert_eq!(successor("deepseek-v4-flash-0731", &["deepseek-v4.1-flash-1017", "deepseek-v4-flash-0613"], true), Some(0));
        assert_eq!(successor("deepseek-v4-pro", &["deepseek-v4-pro-0813"], true), Some(0));
        // 一样新的取前面那个；认不出系列的不换
        assert_eq!(successor("x-1", &["x-2", "x-2"], true), Some(0));
        assert_eq!(successor("1.5", &["2.0"], true), None);
    }

    #[test]
    fn hosts() {
        assert_eq!(url_host("https://user:pw@API.Example.com:443/v1?x").as_deref(), Some("api.example.com"));
        assert_eq!(url_host("http://[::1]:8080/v1").as_deref(), Some("::1"));
        assert_eq!(url_host("localhost:11434"), None);
        assert_eq!(url_host(" http://0.0.0.0 ").as_deref(), Some("0.0.0.0"));
    }
}
