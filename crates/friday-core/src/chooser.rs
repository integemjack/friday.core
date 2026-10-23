//! 按活挑模型（== Swift Agent/ModelChooser、SubAgentRunner 里选大脑的那段；Qt agent/ModelChooser、SubAgentRunner）：
//! - 从一池模型里按档位挑（同一档里套餐内 > 按量 > 免费；这档没有用指定的，再按相邻的档找）、小活用的快模型
//! - 子智能体的深度思考：按活定（查看低、动手中、难题高）或主智能体点的，不超过这段对话设的档位，再按模型支持的档位落地
//! - 子智能体用哪个大脑：设置里固定的 / 跟对话 / 按活自动挑，主智能体点名的要在允许的里面
//!
//! 模型的档位、计费（用户改过的按用户的）、能不能用（歇着、带不带工具）由两端算好交过来。

use serde_json::{Value, json};

/// 池子里的一个模型
#[derive(Debug, Clone, Default)]
pub struct Candidate {
    pub id: String,
    pub title: String,
    pub model: String,
    /// fast / balanced / strong
    pub tier: String,
    /// plan / metered / free
    pub billing: String,
    pub is_chat: bool,
    pub vision: bool,
    /// 两端说能用（没歇着、带工具……，按调用的场合）
    pub usable: bool,
    /// 所属服务关掉了
    pub disabled: bool,
    /// 本机的 Power（friday-llm）
    pub power: bool,
}

impl Candidate {
    pub fn from_json(value: &Value) -> Candidate {
        let text = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or("").to_string();
        let flag = |key: &str, default: bool| value.get(key).and_then(Value::as_bool).unwrap_or(default);
        Candidate {
            id: text("id"),
            title: text("title"),
            model: text("model"),
            tier: text("tier"),
            billing: text("billing"),
            is_chat: flag("isChat", true),
            vision: flag("vision", false),
            usable: flag("usable", true),
            disabled: flag("disabled", false),
            power: flag("power", false),
        }
    }

    fn label(&self) -> String {
        format!("{}（{} · {}）", self.id, self.title, self.model)
    }
}

fn billing_order(billing: &str) -> i64 {
    match billing {
        "plan" => 0,
        "free" => 2,
        _ => 1,
    }
}

fn neighbors(tier: &str) -> [&'static str; 2] {
    match tier {
        "fast" => ["balanced", "strong"],
        "strong" => ["balanced", "fast"],
        _ => ["strong", "fast"],
    }
}

/// 从 pool 里挑一个（返回下标）：先找 tier 这一档（preferred 在这档就用它；否则套餐内 > 按量 > 免费，一样的按 pool 的顺序），
/// 这档没有就用 preferred，再没有按相邻的档找。只挑对话模型和能用的；prefer_vision 时同档里先挑能看图的
pub fn pick(pool: &[Candidate], tier: &str, preferred: Option<&str>, prefer_vision: bool) -> Option<usize> {
    let candidates: Vec<usize> = (0..pool.len()).filter(|&index| pool[index].is_chat && pool[index].usable).collect();
    let best = |wanted: &str| {
        let mut chosen: Option<(usize, i64)> = None;
        for &index in &candidates {
            let engine = &pool[index];
            if engine.tier != wanted {
                continue;
            }
            // 看图优先于计费：这一轮带着图时换成看不了图的，图要先转成文字
            let score = if prefer_vision && !engine.vision { 10 } else { 0 } + billing_order(&engine.billing);
            if chosen.is_none_or(|(_, best)| score < best) {
                chosen = Some((index, score));
            }
        }
        chosen.map(|(index, _)| index)
    };
    let preferred = preferred.and_then(|id| candidates.iter().copied().find(|&index| pool[index].id == id));
    if let Some(index) = preferred
        && pool[index].tier == tier
    {
        return Some(index);
    }
    if let Some(index) = best(tier) {
        return Some(index);
    }
    if preferred.is_some() {
        return preferred;
    }
    neighbors(tier).into_iter().find_map(best)
}

/// 小活用的模型（自动）：快的一档里按计费挑；一个快的都没有是 None
pub fn pick_light(pool: &[Candidate]) -> Option<usize> {
    let mut chosen: Option<usize> = None;
    for (index, engine) in pool.iter().enumerate() {
        if engine.is_chat
            && engine.tier == "fast"
            && engine.usable
            && chosen.is_none_or(|best| billing_order(&engine.billing) < billing_order(&pool[best].billing))
        {
            chosen = Some(index);
        }
    }
    chosen
}

/// 子智能体挑哪一档：主智能体说了 work 按它；没说跟随主智能体的大脑那一档（没有大脑按均衡）
pub fn sub_agent_tier(work: Option<&str>, main_tier: Option<&str>) -> String {
    match work {
        Some("look") => "fast".into(),
        Some("change") => "balanced".into(),
        Some("hard") => "strong".into(),
        _ => main_tier.unwrap_or("balanced").to_string(),
    }
}

fn rank(level: &str) -> i64 {
    match level {
        "off" => 0,
        "low" => 1,
        "medium" => 2,
        _ => 3,
    }
}

/// 子智能体的深度思考：wanted（主智能体点的）或按活定（查看低、动手中、难题高；没说 work 按动手），不超过 cap（超级模式按高算），
/// 再按模型支持的档位落地（levels 为空 = 不限；只有开 / 关的，低档按关、更高的按开（中））
pub fn thinking(wanted: Option<&str>, work: Option<&str>, cap: &str, levels: &[String]) -> String {
    let wanted = wanted.unwrap_or(match work.unwrap_or("change") {
        "look" => "low",
        "hard" => "high",
        _ => "medium",
    });
    let level = if rank(wanted) <= rank(cap) {
        wanted
    } else if cap == "ultra" {
        "high"
    } else {
        cap
    };
    if level == "off" || levels.is_empty() || levels.iter().any(|known| known == level) {
        return level.to_string();
    }
    if level == "low" && !levels.iter().any(|known| known == "low") {
        return "off".into();
    }
    if levels.iter().any(|known| known == "medium") { "medium".into() } else { level.to_string() }
}

/// 子智能体用哪个大脑（== runSubAgent 里选大脑的那段）：
/// - mode：设置里子智能体用的模型（空 = 跟对话、auto = 按活自动挑、其余 = 固定那个引擎）
/// - engines：全部引擎；allowed：设置里允许子智能体用的（按设置的顺序）；current：这段对话的大脑；requested：主智能体点名的
/// - 返回 {engine} 或 {failure, kind: invalid | failed}
pub fn choose_engine(
    mode: &str,
    engines: &[Candidate],
    allowed: &[Candidate],
    current: Option<&str>,
    requested: Option<&str>,
    work: Option<&str>,
    has_enabled: bool,
) -> Value {
    let choices: Vec<String> = allowed.iter().map(Candidate::label).collect();
    if allowed.is_empty() {
        let message = if has_enabled {
            "用户没有允许子 Agent 使用任何大脑（设置 › 服务与密钥）。请自己完成任务。".to_string()
        } else {
            "还没有接入在线大模型：在工具栏的模型菜单里点「去设置里添加在线服务…」，或在 设置 › 服务与密钥 里添加在线服务。请自己完成任务。"
                .to_string()
        };
        return json!({ "failure": message, "kind": "failed" });
    }
    let chooses = mode == "auto";
    let find = |id: &str| engines.iter().find(|engine| engine.id == id);
    let allows = |id: &str| allowed.iter().any(|engine| engine.id == id);
    // 固定的那个删掉了、所属服务关掉了：当成跟对话
    let fixed = if chooses || mode.is_empty() { None } else { find(mode).filter(|engine| !engine.disabled && engine.is_chat) };
    // v2.0 之前的 "local"（本机模型）不再有：当成没指定
    let requested = if chooses { requested.filter(|id| *id != "local") } else { None };
    let current = current.and_then(find);
    let chosen: String = if let Some(fixed) = fixed {
        fixed.id.clone()
    } else if !chooses && let Some(current) = current {
        // 跟对话：就用这段对话的大脑；不在允许的里面时换成里面同一档的（没有再找相邻的档），一个能用的都挑不出来用第一个允许的
        if allows(&current.id) {
            current.id.clone()
        } else {
            let pool: Vec<Candidate> =
                allowed.iter().map(|engine| Candidate { usable: engine.usable && !engine.power, ..engine.clone() }).collect();
            pick(&pool, &current.tier, None, current.vision).map_or_else(|| allowed[0].id.clone(), |index| pool[index].id.clone())
        }
    } else if let Some(requested) = requested {
        let Some(found) = find(requested) else {
            return json!({ "failure": format!("没有 id 为 {requested} 的在线引擎。可用：{}", choices.join("、")), "kind": "invalid" });
        };
        if found.disabled {
            return json!({ "failure": format!("用户关掉了 {} 所属的服务。可用：{}", found.label(), choices.join("、")), "kind": "invalid" });
        }
        if !allows(&found.id) {
            return json!({ "failure": format!("用户不允许子 Agent 使用 {}。可用：{}", found.label(), choices.join("、")), "kind": "invalid" });
        }
        found.id.clone()
    } else if let Some(current) = current.filter(|current| current.power && allows(&current.id)) {
        // v2.7 主智能体用本地大模型：子智能体跟它用同一个，不按活换档（friday-llm 一次只装一个模型）
        current.id.clone()
    } else {
        // v2.6 按活挑档，没说就是主智能体那一档、先用主智能体的；同一档里再按套餐内、按量、免费；歇着的、不支持工具的、本地大模型不挑
        let main = current.filter(|current| allows(&current.id)).map(|current| current.id.clone());
        let pool: Vec<Candidate> =
            allowed.iter().map(|engine| Candidate { usable: engine.usable && !engine.power, ..engine.clone() }).collect();
        let tier = sub_agent_tier(work, current.map(|current| current.tier.as_str()));
        match pick(&pool, &tier, main.as_deref(), false) {
            Some(index) => pool[index].id.clone(),
            None => main.unwrap_or_else(|| allowed[0].id.clone()),
        }
    };
    json!({ "engine": chosen })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(id: &str, tier: &str, billing: &str) -> Candidate {
        Candidate {
            id: id.into(),
            title: id.to_uppercase(),
            model: format!("{id}-m"),
            tier: tier.into(),
            billing: billing.into(),
            is_chat: true,
            usable: true,
            ..Candidate::default()
        }
    }

    #[test]
    fn picks_by_tier_and_billing() {
        let pool = vec![
            engine("a", "strong", "metered"),
            engine("b", "strong", "plan"),
            engine("c", "fast", "free"),
            engine("d", "balanced", "metered"),
        ];
        assert_eq!(pick(&pool, "strong", None, false), Some(1));
        assert_eq!(pick(&pool, "strong", Some("a"), false), Some(0));
        // 指定的不在这档：先这档最好的
        assert_eq!(pick(&pool, "fast", Some("a"), false), Some(2));
        let mut only = vec![engine("x", "balanced", "metered")];
        only[0].usable = false;
        only.push(engine("y", "fast", "metered"));
        assert_eq!(pick(&only, "balanced", None, false), Some(1));
        let mut vision = vec![engine("p", "strong", "plan"), engine("q", "strong", "metered")];
        vision[1].vision = true;
        assert_eq!(pick(&vision, "strong", None, true), Some(1));
        assert_eq!(pick_light(&pool), Some(2));
        assert_eq!(pick_light(&[engine("z", "strong", "plan")]), None);
    }

    #[test]
    fn thinking_levels() {
        assert_eq!(thinking(None, Some("look"), "high", &[]), "low");
        assert_eq!(thinking(None, None, "high", &[]), "medium");
        assert_eq!(thinking(None, Some("hard"), "medium", &[]), "medium");
        assert_eq!(thinking(Some("high"), None, "ultra", &[]), "high");
        let switch = vec!["off".to_string(), "medium".to_string()];
        assert_eq!(thinking(None, Some("look"), "high", &switch), "off");
        assert_eq!(thinking(None, Some("hard"), "high", &switch), "medium");
        assert_eq!(sub_agent_tier(None, Some("strong")), "strong");
        assert_eq!(sub_agent_tier(Some("look"), Some("strong")), "fast");
    }

    #[test]
    fn engine_choice() {
        let engines = vec![engine("main", "strong", "metered"), engine("cheap", "fast", "plan"), engine("other", "balanced", "metered")];
        let allowed = vec![engines[1].clone(), engines[2].clone()];
        let id = |value: Value| value["engine"].as_str().unwrap_or("").to_string();
        // 跟对话：对话用的不在允许里 → 同档里挑（没有强的，按相邻的档：均衡）
        assert_eq!(id(choose_engine("", &engines, &allowed, Some("main"), None, None, true)), "other");
        // 固定的
        assert_eq!(id(choose_engine("cheap", &engines, &allowed, Some("main"), None, None, true)), "cheap");
        // 按活自动挑
        assert_eq!(id(choose_engine("auto", &engines, &allowed, Some("main"), None, Some("look"), true)), "cheap");
        assert_eq!(id(choose_engine("auto", &engines, &allowed, Some("main"), Some("other"), Some("look"), true)), "other");
        let refused = choose_engine("auto", &engines, &allowed, Some("main"), Some("main"), None, true);
        assert_eq!(refused["kind"], "invalid");
        assert!(
            refused["failure"]
                .as_str()
                .unwrap()
                .starts_with("用户不允许子 Agent 使用 main（MAIN · main-m）。可用：cheap（CHEAP · cheap-m）")
        );
        assert!(
            choose_engine("auto", &engines, &allowed, None, Some("nope"), None, true)["failure"]
                .as_str()
                .unwrap()
                .starts_with("没有 id 为 nope")
        );
        assert_eq!(choose_engine("", &engines, &[], None, None, None, false)["kind"], "failed");
        // 本地大模型：跟主智能体用同一个
        let mut local = engines.clone();
        local[0].power = true;
        let both = vec![local[0].clone(), local[1].clone()];
        assert_eq!(id(choose_engine("auto", &local, &both, Some("main"), None, Some("look"), true)), "main");
    }
}
