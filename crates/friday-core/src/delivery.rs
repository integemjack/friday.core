//! 交付检查、写完没验证的检查、子智能体的运行记录（== Swift Agent/DeliveryCheck、TurnCheck（含 AgentLog）；Qt agent/DeliveryCheck、TurnCheck）。
//!
//! 交付检查：Friday 的目标是用户说一次就拿到结果。用户没要方案、建议、计划时，交出方案、打算或「要不要我…」都不算交付。
//! 框架只做便宜的「起疑」（像方案、却没真正动手、用户也没要方案），判断交给智能体自己（追一条提醒 / 原地打回 / 附验收提示）。
//! 写完没验证：一轮里写了代码或配置，写完之后却没有任何运行或读取，就追加一次提醒（文档类文件不提醒）。
//!
//! 什么时候查、追提醒、打回留在两端；工具的权限由两端给（{名字: readOnly | writeFiles | execute | network}）。

use std::collections::HashMap;

use serde_json::{Map, Value};
use unicode_segmentation::UnicodeSegmentation;

use crate::agents::{as_object, short_id, string_value};
use crate::chat_models::{BlockKind, MessageBlock, ToolEvent};
use crate::prompts::{self, Args};

/// 工具的权限（两端注册表给）
pub type Permissions = HashMap<String, String>;

fn graphemes(text: &str) -> Vec<&str> {
    text.graphemes(true).collect()
}

fn suffix(text: &str, count: usize) -> String {
    let parts = graphemes(text);
    parts[parts.len().saturating_sub(count)..].concat()
}

fn prefix(text: &str, count: usize) -> String {
    graphemes(text).into_iter().take(count).collect()
}

fn contains_any(text: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| text.contains(marker))
}

// MARK: - 用户 / 任务要的是什么

/// 明确在要方案、建议、计划、分析：这时交方案就是交结果
const PLAN_REQUEST: &[&str] = &[
    "方案",
    "建议",
    "计划",
    "思路",
    "规划",
    "步骤",
    "分析",
    "评估",
    "比较",
    "对比",
    "优缺点",
    "推荐",
    "调研",
    "研究一下",
    "是否应该",
    "该不该",
    "要不要",
    "可行性",
    "review",
    "plan",
    "suggest",
    "recommend",
    "advice",
    "proposal",
    "compare",
    "evaluate",
];
/// 在问问题、要解释：回答本身就是结果
const QUESTION: &[&str] = &[
    "怎么",
    "如何",
    "怎样",
    "为什么",
    "为何",
    "是什么",
    "什么是",
    "有哪些",
    "哪个",
    "哪些",
    "多少",
    "是不是",
    "能不能",
    "可不可以",
    "吗",
    "解释",
    "介绍",
    "讲讲",
    "说说",
    "告诉我",
    "how ",
    "what ",
    "why ",
    "which ",
    "explain",
    "?",
    "？",
];
/// 让 Friday 去做事：出现这些就按任务算（「帮我看看怎么回事」要的是修好，不是讲道理）
const TASK: &[&str] = &["帮我", "给我", "替我", "请你", "你来", "麻烦", "帮忙", "please ", "can you ", "could you "];
/// 做事的动词：用户的话里有这些才算交代了任务（「你好」「谢谢」这类闲聊不催着交付）
const ACTION: &[&str] = &[
    "写",
    "做",
    "改",
    "修",
    "建",
    "生成",
    "创建",
    "安装",
    "装",
    "配置",
    "部署",
    "运行",
    "跑",
    "下载",
    "整理",
    "翻译",
    "转换",
    "转成",
    "画",
    "查",
    "找",
    "搜",
    "删",
    "加",
    "实现",
    "开发",
    "搭",
    "优化",
    "测试",
    "发",
    "打开",
    "关闭",
    "设置",
    "清理",
    "备份",
    "统计",
    "计算",
    "导出",
    "合并",
    "汇总",
    "拆",
    "提取",
    "总结",
    "列出",
    "截图",
    "录",
    "上传",
    "迁移",
    "升级",
    "重命名",
    "移动",
    "复制",
    "压缩",
    "解压",
    "write",
    "make",
    "build",
    "create",
    "fix",
    "install",
    "run",
    "generate",
    "convert",
    "translate",
    "find",
    "search",
    "delete",
    "add",
    "implement",
    "set up",
    "deploy",
    "download",
    "rename",
    "move",
    "copy",
    "clean",
];
/// 子智能体的任务本身是调研、分析、出方案、审查（交回的分析就是成品）：在 PLAN_REQUEST 之外再认这些
const ANALYSIS: &[&str] = &[
    "审查",
    "审稿",
    "核对",
    "检查一下",
    "查一下",
    "搜索",
    "查找",
    "总结",
    "汇总",
    "列出",
    "解释",
    "说明",
    "research",
    "investigate",
    "analyze",
    "analyse",
    "summarize",
    "audit",
    "find out",
];

/// 用户这一轮是不是在要方案 / 建议 / 解释（是就不催着「直接做」）
pub fn asks_for_plan(request: &str) -> bool {
    let text = request.to_lowercase();
    if contains_any(&text, PLAN_REQUEST) {
        return true;
    }
    if contains_any(&text, TASK) {
        return false;
    }
    contains_any(&text, QUESTION)
}

/// 明确在要方案、建议、计划（单纯提问、「告诉我结果」不算）：这时把修不修交回去是对的
pub fn asks_for_plan_explicitly(request: &str) -> bool {
    contains_any(&request.to_lowercase(), PLAN_REQUEST)
}

/// 用户交代了要做的事（不是闲聊）
pub fn looks_like_task(request: &str) -> bool {
    let text = request.to_lowercase();
    contains_any(&text, TASK) || contains_any(&text, ACTION)
}

/// 子智能体的任务本身就是调研、分析、出方案、审查
pub fn task_wants_analysis(task: &str) -> bool {
    let text = task.to_lowercase();
    contains_any(&text, PLAN_REQUEST) || contains_any(&text, ANALYSIS)
}

// MARK: - 交出来的像不像方案

/// 把问题抛回给用户：「要不要我…」「需要我…吗」
const OFFER: &[&str] = &[
    "要不要我",
    "需要我",
    "是否需要",
    "是否要我",
    "要我帮你",
    "要我现在",
    "你希望我",
    "你想让我",
    "需要的话我",
    "如需我",
    "如果需要，我",
    "如果你愿意",
    "我可以帮你",
    "我可以继续",
    "我可以接着",
    "请确认",
    "确认后我",
    "等你确认",
    "would you like me",
    "shall i",
    "do you want me",
    "should i proceed",
    "let me know if you",
    "if you'd like, i",
];
/// 给方案、打算、让用户自己去做
const PLAN: &[&str] = &[
    "建议你",
    "建议您",
    "你可以",
    "您可以",
    "可以考虑",
    "推荐你",
    "你需要",
    "您需要",
    "请你",
    "请您",
    "方案如下",
    "计划如下",
    "步骤如下",
    "具体步骤",
    "以下步骤",
    "操作步骤",
    "思路如下",
    "做法如下",
    "我将",
    "我会先",
    "我会去",
    "我打算",
    "我准备",
    "接下来我",
    "下一步我",
    "下面我会",
    "you can ",
    "you could ",
    "you should ",
    "i will ",
    "i'll ",
    "i would ",
    "here's a plan",
    "here is a plan",
    "next steps",
];

/// 回复看起来是方案 / 建议 / 打算 / 询问，而不是做出来的结果
pub fn looks_like_plan(text: &str) -> bool {
    let lowered = text.to_lowercase();
    if lowered.trim().is_empty() {
        return false;
    }
    // 结尾在问要不要做：最后 160 字里有询问
    let tail = suffix(&lowered, 160);
    if contains_any(&tail, OFFER) {
        return true;
    }
    let hits =
        PLAN.iter().filter(|marker| lowered.contains(*marker)).count() + OFFER.iter().filter(|marker| lowered.contains(*marker)).count();
    hits >= 2
}

/// v2.11 说了已经做完、给了结论的不算
const DONE: &[&str] = &["已", "完成了", "做完", "无需", "不需要", "没有发现", "未发现", "结论", "通过", "成功", "失败"];
const ANNOUNCE: &[&str] = &[
    "继续",
    "接着",
    "接下来",
    "下面",
    "下一步",
    "现在",
    "然后",
    "让我",
    "我先",
    "我来",
    "我将",
    "我会",
    "我要",
    "准备",
    "开始",
    "实施",
    "执行",
    "正在",
    "马上",
    "先",
    "let me",
    "now ",
    "next",
    "i'll",
    "i will",
];

/// v2.11 最后只说了一句下一步就结束了（「实施修复 12」「接下来读 p2p.go」）：很短、不是结论、以「继续 / 接下来 / 现在 / 让我…」开头或以冒号结尾
pub fn looks_like_announcement(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains('\n') || graphemes(trimmed).len() > 80 {
        return false;
    }
    if contains_any(trimmed, DONE) {
        return false;
    }
    let lowered = trimmed.to_lowercase();
    if ANNOUNCE.iter().any(|start| lowered.starts_with(start)) {
        return true;
    }
    trimmed.ends_with('：') || trimmed.ends_with(':')
}

/// 把「修不修」交回给用户的说法
const HAND_BACK: &[&str] = &[
    "你说一声",
    "说一声",
    "我就安排",
    "我就去修",
    "我就修",
    "要修的话",
    "如需修复",
    "需要修复的话",
    "要不要修",
    "要我修",
    "等你决定",
    "由你决定",
    "你来决定",
    "你决定",
    "你拍板",
    "等你拍板",
    "等你发话",
    "你发话",
    "want me to fix",
    "should i fix",
    "shall i fix",
];
/// 回复里说到了问题：失败、报错、没通过
const PROBLEM: &[&str] =
    &["失败", "报错", "错误", "出错", "没通过", "未通过", "不通过", "编译不过", "挂了", "挂在", "崩溃", "bug", "fail", "error", "broken"];

/// 发现了问题，却把修不修交回给用户
pub fn hands_back_fix(text: &str) -> bool {
    let lowered = text.to_lowercase();
    let tail = suffix(&lowered, 240);
    if !contains_any(&tail, HAND_BACK) && !contains_any(&tail, OFFER) {
        return false;
    }
    contains_any(&lowered, PROBLEM)
}

/// 会跑子智能体、返回它结果的工具
pub const TEAM_TOOLS: [&str; 4] = ["delegate_task", "run_parallel_tasks", "rework_agent", "run_workflow"];

fn tool_of(block: &MessageBlock) -> Option<&ToolEvent> {
    match &block.kind {
        BlockKind::Tool(event) => Some(event),
        _ => None,
    }
}

/// 这些块里真正动手了没有：成功的写入 / 执行类工具，或交回了结果的子智能体
pub fn did_work(blocks: &[MessageBlock], permissions: &Permissions) -> bool {
    blocks.iter().filter_map(tool_of).filter(|event| event.status == "succeeded").any(|event| {
        TEAM_TOOLS.contains(&event.name.as_str())
            || matches!(permissions.get(&event.name).map(String::as_str), Some("writeFiles") | Some("execute"))
    })
}

/// 交给用户 / 父级的最后那段话：最后一个工具调用之后的文字
pub fn final_reply(blocks: &[MessageBlock]) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for block in blocks {
        match &block.kind {
            BlockKind::Tool(_) => parts.clear(),
            BlockKind::Text(text) => parts.push(text),
            _ => {}
        }
    }
    parts.join("\n\n")
}

/// 子智能体结果里的「未完成与风险」写了东西（不是「无」）
pub fn unfinished_items(text: &str) -> Option<String> {
    let start = text.find("未完成与风险")? + "未完成与风险".len();
    let mut rest = text[start..].trim_start_matches(|c: char| ":： \n*".contains(c));
    if let Some(end) = rest.find("\n\n") {
        rest = &rest[..end];
    }
    let value = rest.trim();
    const EMPTY: [&str; 10] = ["无", "无。", "没有", "没有。", "暂无", "none", "n/a", "-", "（无）", "(无)"];
    if value.is_empty() || EMPTY.contains(&value.to_lowercase().as_str()) {
        return None;
    }
    Some(if graphemes(value).len() > 200 { prefix(value, 200) + "…" } else { value.to_string() })
}

// MARK: - 提醒

fn args(pairs: &[(&str, String)]) -> Args {
    pairs.iter().map(|(key, value)| (key.to_string(), value.clone())).collect()
}

/// 主智能体：一轮快结束时交给用户的是方案 / 建议 / 询问，又没真正动手。stops、off_note：设置里哪些事要停下来交给用户（UserStops）
pub fn main_prompt(stops: &str, off_note: &str, override_id: Option<u64>) -> String {
    let clause = if stops.is_empty() {
        "。".to_string()
    } else {
        prompts::text("delivery.main.stops", &args(&[("stops", stops.into())]), override_id)
    };
    prompts::text("delivery.main", &args(&[("stopsClause", clause), ("offNote", off_note.into())]), override_id)
}

/// 主智能体：发现了问题，却把修不修交回给用户。delegates：这段对话能派子智能体（能派的才提「或你派的子 Agent」）
pub fn fix_prompt(delegates: bool, override_id: Option<u64>) -> String {
    let by_whom = prompts::text(if delegates { "working.by.team" } else { "working.by.self" }, &Args::new(), override_id);
    prompts::text("delivery.fix", &args(&[("byWhom", by_whom)]), override_id)
}

/// v2.11 只说了一句下一步就结束了（said：它说的那句）
pub fn announce_prompt(said: &str, override_id: Option<u64>) -> String {
    prompts::text("delivery.announce", &args(&[("said", said.trim().into())]), override_id)
}

/// 交给父级的结果后面附的验收提示；没什么可疑的是 None
pub fn review_hint(
    result: &str,
    unverified: &[String],
    still_plan: bool,
    stops: &str,
    off_note: &str,
    override_id: Option<u64>,
) -> Option<String> {
    let mut issues = Vec::new();
    if still_plan {
        issues.push(prompts::text("review.issue.plan", &Args::new(), override_id));
    }
    if !unverified.is_empty() {
        issues.push(prompts::text("review.issue.unverified", &args(&[("files", unverified.join("、"))]), override_id));
    }
    if let Some(open) = unfinished_items(result) {
        issues.push(prompts::text("review.issue.open", &args(&[("open", open)]), override_id));
    }
    if issues.is_empty() {
        return None;
    }
    let clause = if stops.is_empty() {
        prompts::text("review.stops.none", &Args::new(), override_id)
    } else {
        prompts::text("review.stops", &args(&[("stops", stops.into())]), override_id)
    };
    let hint = prompts::text(
        "review.hint",
        &args(&[("issues", issues.join("；")), ("stopsClause", clause), ("offNote", off_note.into())]),
        override_id,
    );
    Some(format!("\n\n{hint}"))
}

// MARK: - 写完没验证

/// 写完需要运行或读回确认的文件类型（Windows 上 run_shell 是 PowerShell，再认 PowerShell / cmd 的脚本）
const CHECKED_EXTENSIONS: &[&str] = &[
    "py", "sh", "zsh", "bash", "js", "mjs", "cjs", "ts", "tsx", "jsx", "swift", "c", "h", "cc", "cpp", "hpp", "m", "mm", "go", "rs", "rb",
    "java", "kt", "php", "lua", "pl", "r", "sql", "json", "yml", "yaml", "toml", "ini", "conf", "cfg", "plist", "xml", "env", "html",
    "css", "scss",
];
const WINDOWS_SCRIPTS: &[&str] = &["ps1", "psm1", "psd1", "bat", "cmd"];
const CHECKED_NAMES: &[&str] = &["Makefile", "Dockerfile", "Gemfile", "Podfile", "Package.swift"];
/// 命令、子智能体、技能的工具：写的是提示词（技能另看带的文件）
const COMMAND_TOOLS: &[&str] = &["use_command", "save_command", "delete_command", "save_subagent", "delete_subagent"];
const SKILL_TOOLS: &[&str] = &["use_skill", "save_skill", "delete_skill"];

/// == (path as NSString).lastPathComponent：连续的「/」算一个、去掉末尾的「/」，取最后一段；windows 时「\」也当分隔符
fn last_path_component(path: &str, windows: bool) -> String {
    let mut fixed = String::with_capacity(path.len());
    for c in path.chars() {
        let separator = c == '/' || (windows && c == '\\');
        if !separator {
            fixed.push(c);
        } else if !fixed.ends_with('/') {
            fixed.push('/');
        }
    }
    if fixed.chars().count() > 1 && fixed.ends_with('/') {
        fixed.pop();
    }
    if fixed.chars().count() <= 1 {
        return fixed;
    }
    fixed.rsplit('/').next().unwrap_or("").to_string()
}

/// == (name as NSString).pathExtension：最后一个「.」之后；「.」在开头的（.env）没有扩展名
fn path_extension(name: &str) -> String {
    if name.chars().count() <= 1 {
        return String::new();
    }
    match name.rfind('.') {
        Some(dot) if dot > 0 => name[dot + 1..].to_string(),
        _ => String::new(),
    }
}

fn checked(name: &str, windows: bool) -> bool {
    let extension = path_extension(name).to_lowercase();
    CHECKED_EXTENSIONS.contains(&extension.as_str())
        || (windows && WINDOWS_SCRIPTS.contains(&extension.as_str()))
        || CHECKED_NAMES.contains(&name)
}

/// 需要验证的写入目标；文档类文件是 None
fn target(event: &ToolEvent, windows: bool) -> Option<String> {
    let args = serde_json::from_str::<Value>(&event.arguments).ok().and_then(|value| value.as_object().cloned());
    // v2.61 命令、子智能体的文件是提示词，不用验证
    if COMMAND_TOOLS.contains(&event.name.as_str()) {
        return None;
    }
    // v2.14 技能：SKILL.md 是文档、删技能不用验证；save_skill 带的文件里有脚本、代码时才要（报第一个，按路径排）
    if SKILL_TOOLS.contains(&event.name.as_str()) {
        if event.name != "save_skill" {
            return None;
        }
        let files: Map<String, Value> = args.as_ref().and_then(|args| args.get("files")).and_then(as_object)?;
        let mut keys: Vec<&String> = files.keys().collect();
        keys.sort();
        return keys.into_iter().map(|key| last_path_component(key, windows)).find(|name| checked(name, windows));
    }
    // == (args["path"] ?? args["file"] ?? args["file_path"])?.stringValue：键在（哪怕值是 null）就用它
    let path = args
        .as_ref()
        .and_then(|args| ["path", "file", "file_path"].iter().find_map(|key| args.get(*key)))
        .and_then(|value| string_value(Some(value)));
    let Some(path) = path.filter(|path| !path.is_empty()) else { return Some(event.name.clone()) };
    let name = last_path_component(&path, windows);
    checked(&name, windows).then_some(name)
}

/// 这一轮写了、写完之后没有运行或读取过的文件（没有路径参数的写入类工具记工具名）
pub fn unverified_writes(blocks: &[MessageBlock], permissions: &Permissions, windows: bool) -> Vec<String> {
    let mut written: Vec<String> = Vec::new();
    for event in blocks.iter().filter_map(tool_of) {
        if event.status != "succeeded" || ["update_task_state", "read_agent_log", "get_current_time"].contains(&event.name.as_str()) {
            continue;
        }
        match permissions.get(&event.name).map(String::as_str) {
            Some("writeFiles") => {
                if let Some(file) = target(event, windows)
                    && !written.contains(&file)
                {
                    written.push(file);
                }
            }
            // 之后读过、运行过（包括派子智能体复核），算验证过
            Some("readOnly") | Some("execute") => written.clear(),
            _ => {}
        }
    }
    written
}

/// 用户明确说了不用运行 / 测试时不提醒
pub fn user_declined_verification(request: &str) -> bool {
    const PHRASES: [&str; 19] = [
        "不用运行",
        "不要运行",
        "别运行",
        "无需运行",
        "不用测试",
        "不要测试",
        "别测试",
        "不用验证",
        "不要验证",
        "无需验证",
        "不用跑",
        "不要跑",
        "别跑",
        "只写文件",
        "写好就行",
        "don't run",
        "do not run",
        "no need to run",
        "don't test",
    ];
    contains_any(&request.to_lowercase(), &PHRASES)
}

/// 写完没验证的提醒（delegates：能派下级的才提「派审查子 Agent 复核」）
pub fn turn_prompt(files: &[String], delegates: bool, override_id: Option<u64>) -> String {
    let review = if delegates { prompts::text("turn-check.review", &Args::new(), override_id) } else { String::new() };
    prompts::text("turn-check", &args(&[("files", files.join("、")), ("review", review)]), override_id)
}

// MARK: - 子智能体的运行记录

/// 在消息块里（含嵌套的运行记录）按 id 前缀（至少 4 位，不分大小写）找子智能体的事件
pub fn find_log<'a>(id: &str, blocks: &'a [MessageBlock]) -> Option<&'a ToolEvent> {
    let prefix = id.trim().to_lowercase();
    if graphemes(&prefix).len() < 4 {
        return None;
    }
    fn search<'a>(prefix: &str, blocks: &'a [MessageBlock]) -> Option<&'a ToolEvent> {
        for event in blocks.iter().filter_map(tool_of) {
            let Some(transcript) = &event.transcript else { continue };
            if event.id.to_lowercase().starts_with(prefix) {
                return Some(event);
            }
            if let Some(nested) = search(prefix, transcript) {
                return Some(nested);
            }
        }
        None
    }
    search(&prefix, blocks)
}

fn status_text(status: &str) -> &'static str {
    match status {
        "succeeded" => "成功",
        "failed" => "失败",
        "denied" => "被拒绝",
        "running" => "运行中",
        "awaitingApproval" => "等确认",
        _ => "",
    }
}

/// 超过 limit 个字只留开头，附「…（共 N 字）」
fn clip(text: &str, limit: usize) -> String {
    let parts = graphemes(text);
    if parts.len() <= limit { text.to_string() } else { format!("{}…（共 {} 字）", parts[..limit].concat(), parts.len()) }
}

/// 参数的可读 JSON 压成一行：按行切开（丢掉空行）、每行去掉首尾空格，用空格连起来
fn compact(json: &str) -> String {
    crate::task_state::split_lines(json).into_iter().map(|line| line.trim_matches([' ', '\t'])).collect::<Vec<_>>().join(" ")
}

/// 运行记录整理成给模型看的文字（不截断；read_agent_log 给了 offset 时按它分页读）
pub fn log_text(event: &ToolEvent) -> String {
    let name = event.extra.get("agentName").and_then(Value::as_str).unwrap_or("子 Agent");
    let mut lines = vec![format!("子 Agent「{name}」的运行记录（{}）：", status_text(&event.status))];
    for block in event.transcript.as_deref().unwrap_or(&[]) {
        match &block.kind {
            BlockKind::Text(text) => {
                let text = text.trim();
                if !text.is_empty() {
                    lines.push(format!("【输出】{}", clip(text, 1500)));
                }
            }
            BlockKind::Tool(call) if call.name == "agent_message" => {
                let title = call.extra.get("title").and_then(Value::as_str).unwrap_or("补充");
                lines.push(format!("【{title}】{}", clip(&call.arguments, 800)));
            }
            BlockKind::Tool(call) => {
                lines.push(format!("【调用】{} {}", call.name, clip(&compact(&call.arguments), 300)));
                if call.transcript.is_some() {
                    let nested = call.extra.get("agentName").and_then(Value::as_str).unwrap_or("");
                    lines.push(format!(
                        "【结果】（嵌套的子 Agent「{nested}」，运行记录 id：{}）{}",
                        short_id(&call.id),
                        clip(&call.result, 600)
                    ));
                } else {
                    lines.push(format!("【结果】{}：{}", status_text(&call.status), clip(&call.result, 800)));
                }
            }
            _ => {}
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn blocks(value: Value) -> Vec<MessageBlock> {
        serde_json::from_value(value).unwrap()
    }

    fn tool(id: &str, name: &str, arguments: &str, status: &str) -> Value {
        json!({ "id": format!("B{id}"), "kind": { "tool": { "_0": { "id": id, "name": name, "arguments": arguments, "status": status } } } })
    }

    #[test]
    fn requests() {
        assert!(asks_for_plan("给个方案"));
        assert!(!asks_for_plan("帮我看看怎么回事"));
        assert!(asks_for_plan("这是什么？"));
        assert!(asks_for_plan_explicitly("出个计划") && !asks_for_plan_explicitly("为什么"));
        assert!(looks_like_task("把这个文件翻译一下") && !looks_like_task("你好"));
        assert!(task_wants_analysis("审查这段代码") && !task_wants_analysis("改好它"));
    }

    #[test]
    fn replies() {
        assert!(looks_like_plan("我已经看完了。要不要我现在就改？"));
        assert!(looks_like_plan("建议你先备份，你可以再重装"));
        assert!(!looks_like_plan("改好了，测试都过了。"));
        assert!(!looks_like_plan("   "));
        assert!(looks_like_announcement("接下来读 p2p.go"));
        assert!(looks_like_announcement("修复内容如下："));
        assert!(!looks_like_announcement("已完成修复"));
        assert!(!looks_like_announcement("继续\n下一行"));
        assert!(hands_back_fix("编译失败了，你说一声我就修"));
        assert!(!hands_back_fix("你说一声我就修"));
        assert_eq!(unfinished_items("结果：好\n\n未完成与风险：\n- 没测 Windows\n\n其他"), Some("- 没测 Windows".into()));
        assert_eq!(unfinished_items("未完成与风险：无"), None);
        assert_eq!(unfinished_items("没有这一节"), None);
    }

    #[test]
    fn work_and_final_reply() {
        let permissions: Permissions =
            [("write_file".to_string(), "writeFiles".to_string()), ("read_file".to_string(), "readOnly".to_string())].into();
        let list = blocks(json!([
            { "id": "T0", "kind": { "text": { "_0": "先看看" } } },
            tool("1", "read_file", "{}", "succeeded"),
            { "id": "T1", "kind": { "text": { "_0": "结果一" } } },
            { "id": "T2", "kind": { "text": { "_0": "结果二" } } },
        ]));
        assert!(!did_work(&list, &permissions));
        assert_eq!(final_reply(&list), "结果一\n\n结果二");
        assert!(did_work(&blocks(json!([tool("2", "write_file", "{}", "succeeded")])), &permissions));
        assert!(did_work(&blocks(json!([tool("3", "delegate_task", "{}", "succeeded")])), &permissions));
        assert!(!did_work(&blocks(json!([tool("4", "write_file", "{}", "failed")])), &permissions));
    }

    #[test]
    fn unverified() {
        let permissions: Permissions = [
            ("write_file".to_string(), "writeFiles".to_string()),
            ("run_shell".to_string(), "execute".to_string()),
            ("save_skill".to_string(), "writeFiles".to_string()),
            ("save_command".to_string(), "writeFiles".to_string()),
            ("make_thing".to_string(), "writeFiles".to_string()),
        ]
        .into();
        let list = blocks(json!([
            tool("1", "write_file", "{\"path\":\"/a/b/run.py\"}", "succeeded"),
            tool("2", "run_shell", "{}", "succeeded"),
            tool("3", "write_file", "{\"path\":\"/a/README.md\"}", "succeeded"),
            tool("4", "write_file", "{\"file_path\":\"C:\\\\x\\\\go.ps1\"}", "succeeded"),
            tool("5", "save_skill", "{\"files\":{\"z/b.sh\":\"x\",\"a/SKILL.md\":\"y\"}}", "succeeded"),
            tool("6", "save_command", "{\"path\":\"x.py\"}", "succeeded"),
            tool("7", "make_thing", "{}", "succeeded"),
            tool("8", "write_file", "{\"path\":\"/etc/.env\"}", "succeeded"),
        ]));
        assert_eq!(unverified_writes(&list, &permissions, false), ["b.sh", "make_thing"]);
        assert_eq!(unverified_writes(&list, &permissions, true), ["go.ps1", "b.sh", "make_thing"]);
        assert_eq!(last_path_component("/a//b/", false), "b");
        assert_eq!(last_path_component("/", false), "/");
        assert_eq!(path_extension(".env"), "");
        assert!(user_declined_verification("写好就行，不用跑"));
    }

    #[test]
    fn agent_logs() {
        let list = blocks(json!([{ "id": "B1", "kind": { "tool": { "_0": {
            "id": "ABCD1234-0000", "name": "delegate_task", "arguments": "{}", "status": "succeeded", "agentName": "甲", "result": "ok",
            "transcript": [
                { "id": "B2", "kind": { "text": { "_0": " 我来查 " } } },
                { "id": "B3", "kind": { "tool": { "_0": { "id": "X", "name": "agent_message", "arguments": "改成只查文档", "status": "succeeded", "title": "用户补充" } } } },
                { "id": "B4", "kind": { "tool": { "_0": { "id": "EF567890-1", "name": "delegate_task", "arguments": "{\n  \"task\" : \"t\"\n}",
                    "status": "running", "agentName": "乙", "result": "", "transcript": [] } } } },
                { "id": "B5", "kind": { "tool": { "_0": { "id": "Y", "name": "read_file", "arguments": "{}", "status": "failed", "result": "没有" } } } },
            ] } } } }]));
        assert_eq!(find_log("abcd", &list).unwrap().id, "ABCD1234-0000");
        assert_eq!(find_log("EF56", &list).unwrap().id, "EF567890-1");
        assert!(find_log("abc", &list).is_none());
        let text = log_text(find_log("abcd1234", &list).unwrap());
        assert_eq!(
            text,
            "子 Agent「甲」的运行记录（成功）：\n【输出】我来查\n【用户补充】改成只查文档\n【调用】delegate_task { \"task\" : \"t\" }\n【结果】（嵌套的子 Agent「乙」，运行记录 id：ef567890）\n【调用】read_file {}\n【结果】失败：没有"
        );
    }
}
