//! 在线引擎配置（数据目录的 engines.json）的读写格式。引擎上那些按接口地址推断的规则（思考的方式、是不是本机、要不要带回思考）
//! 每次请求都要用，跟着在线引擎在第 3 阶段搬（docs/PORTING.md）。
//!
//! - 读：宽松，缺的键用默认值（== Qt；Swift 原来缺一个键整份读不出来，所有模型都不见了）；认不出的 thinking / tier / billing 去掉
//!   （两端都会按没设置推断）；不是对象的条目跳过
//! - 写：非可选字段全写（Swift 解码时它们是必填的）；可选字段为空时不写键：disabled 只在关掉时写 true，visionChecked 只在有值时写；
//!   认不出的字段原样保留。pretty + 键排序（[`crate::swift_json`]）
//!
//! 原来是 Swift `LLM/Engine/EngineStore.swift` 和 Qt `llm/engine/EngineStore.cpp` 各写一份。

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::swift_json;

fn default_base_url() -> String {
    "https://api.openai.com/v1".to_string()
}
fn default_secret() -> String {
    "OPENAI_API_KEY".to_string()
}
fn default_model() -> String {
    "gpt-4o-mini".to_string()
}
fn yes() -> bool {
    true
}
fn is_false(flag: &Option<bool>) -> bool {
    *flag != Some(true)
}
fn is_blank(text: &Option<String>) -> bool {
    text.as_deref().is_none_or(str::is_empty)
}

/// 一个在线大脑：OpenAI 兼容的 chat/completions 接口
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineConfig {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(rename = "baseURL", default = "default_base_url")]
    pub base_url: String,
    /// 密钥库里的名字；none 表示不需要
    #[serde(default = "default_secret")]
    pub api_key_secret: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "yes")]
    pub supports_vision: bool,
    #[serde(default = "yes")]
    pub supports_tools: bool,
    #[serde(default)]
    pub sends_enable_thinking: bool,
    #[serde(default)]
    pub extra_body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<i64>,
    /// none / toggle / effort / thinkingType；没有按 sendsEnableThinking 推断
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// 关掉了（这家服务关掉）；开着时不写这个键
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: Option<bool>,
    /// fast / balanced / strong；没有按模型名推断
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// plan / metered / free
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing: Option<String>,
    /// v2.42 能不能看图是怎么定的（实测过记「baseURL|model」，手动设的记「manual|baseURL|model」）
    #[serde(default, skip_serializing_if = "is_blank")]
    pub vision_checked: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

const THINKING: &[&str] = &["none", "toggle", "effort", "thinkingType"];
const TIERS: &[&str] = &["fast", "balanced", "strong"];
const BILLINGS: &[&str] = &["plan", "metered", "free"];

impl EngineConfig {
    /// 认不出的取值去掉（以后的版本新加的：两端按没设置推断，不让这个引擎读不出来）；开着的不留 disabled: false
    fn normalized(mut self) -> EngineConfig {
        let keep = |value: Option<String>, known: &[&str]| value.filter(|value| known.contains(&value.as_str()));
        self.thinking = keep(self.thinking, THINKING);
        self.tier = keep(self.tier, TIERS);
        self.billing = keep(self.billing, BILLINGS);
        if self.disabled != Some(true) {
            self.disabled = None;
        }
        if is_blank(&self.vision_checked) {
            self.vision_checked = None;
        }
        self
    }
}

/// 规整一组引擎（不是对象的跳过）
pub fn normalize(items: Vec<Value>) -> Vec<EngineConfig> {
    items
        .into_iter()
        .filter(Value::is_object)
        .filter_map(|item| serde_json::from_value::<EngineConfig>(item).ok())
        .map(EngineConfig::normalized)
        .collect()
}

/// 读 engines.json；文件不在、不是 JSON 数组返回空
pub fn load(path: &Path) -> Vec<EngineConfig> {
    match std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str::<Value>(&text).ok()) {
        Some(Value::Array(items)) => normalize(items),
        _ => Vec::new(),
    }
}

/// 写 engines.json（先写临时文件再换名）
pub fn save(path: &Path, engines: &[EngineConfig]) -> std::io::Result<()> {
    let value = serde_json::to_value(engines).map_err(std::io::Error::other)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("json.friday-core-{}", std::process::id()));
    std::fs::write(&temporary, swift_json::pretty(&value))?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_and_writing() {
        let engines = normalize(vec![
            json!({"id": "a", "title": "缺了好几个键"}),
            json!({"id": "b", "title": "全的", "baseURL": "https://x/v1", "apiKeySecret": "K", "model": "m", "supportsVision": false,
                   "supportsTools": true, "sendsEnableThinking": true, "extraBody": "{}", "maxConcurrency": 8, "thinking": "effort",
                   "disabled": false, "tier": "strong", "billing": "plan", "visionChecked": "https://x/v1|m", "later": [1]}),
            json!({"id": "c", "title": "以后的取值", "thinking": "quantum", "tier": "ultra", "billing": "?", "disabled": true, "visionChecked": ""}),
            json!("不是对象"),
        ]);
        assert_eq!(engines.len(), 3);
        let written = serde_json::to_value(&engines).unwrap();
        // 缺的键补默认值，非可选的全写
        assert_eq!(
            written[0],
            json!({"id": "a", "title": "缺了好几个键", "baseURL": "https://api.openai.com/v1", "apiKeySecret": "OPENAI_API_KEY",
                   "model": "gpt-4o-mini", "supportsVision": true, "supportsTools": true, "sendsEnableThinking": false, "extraBody": ""})
        );
        // 开着的不写 disabled，认不出的字段原样
        assert!(written[1].get("disabled").is_none());
        assert_eq!(written[1]["later"], json!([1]));
        assert_eq!(written[1]["thinking"], json!("effort"));
        // 认不出的取值去掉；关掉的写 true；空的 visionChecked 不写
        assert!(written[2].get("thinking").is_none() && written[2].get("tier").is_none() && written[2].get("billing").is_none());
        assert_eq!(written[2]["disabled"], json!(true));
        assert!(written[2].get("visionChecked").is_none());
    }

    #[test]
    fn files() {
        let directory = std::env::temp_dir().join(format!("friday-core-engines-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let path = directory.join("engines.json");
        assert!(load(&path).is_empty());
        // Swift 写的（「/」转义成「\/」、pretty）读得出来
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(&path, "[\n  {\n    \"baseURL\" : \"https:\\/\\/api.deepseek.com\\/v1\",\n    \"id\" : \"engine-1\",\n    \"title\" : \"DeepSeek\"\n  }\n]").unwrap();
        let engines = load(&path);
        assert_eq!(engines[0].base_url, "https://api.deepseek.com/v1");
        save(&path, &engines).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("[\n  {\n    \"apiKeySecret\" : \"OPENAI_API_KEY\",\n    \"baseURL\" : \"https://api.deepseek.com/v1\""),
            "{text}"
        );
        assert_eq!(load(&path), engines);
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
