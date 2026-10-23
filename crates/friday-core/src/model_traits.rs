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
    fn hosts() {
        assert_eq!(url_host("https://user:pw@API.Example.com:443/v1?x").as_deref(), Some("api.example.com"));
        assert_eq!(url_host("http://[::1]:8080/v1").as_deref(), Some("::1"));
        assert_eq!(url_host("localhost:11434"), None);
        assert_eq!(url_host(" http://0.0.0.0 ").as_deref(), Some("0.0.0.0"));
    }
}
