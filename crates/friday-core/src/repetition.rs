//! 模型复读（== Swift `LLM/Engine/Repetition.swift`、Qt `llm/engine/Repetition.cpp`）：一次回复里同一段话一遍遍重复
//! （思考或正文），或一轮里每一步都说同一句「现在去做…」却一直没做到。流式解析（[`crate::chat_stream`]）用它截断复读，
//! 智能体循环用相似度提醒模型换做法。长度、下标按 UTF-16 算（同 Swift 的 `utf16`、Qt 的 QString），两端截在同一个地方。

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

/// 思考：一段 ≥ 60 字连着出现 3 遍就算
pub const REASONING_PERIOD: usize = 60;
pub const REASONING_COPIES: usize = 3;
/// 正文：要 8 遍（用户可能就是要它重复：「从 0 数到 9，重复很多遍」「这句话抄十遍」；表格、代码里也偶尔有整行重复）
pub const CONTENT_PERIOD: usize = 60;
pub const CONTENT_COPIES: usize = 8;
/// 流式时每多这么多字（UTF-16）看一次
pub const CHECK_EVERY: usize = 64;
/// 一次请求里截断复读后提醒重来的次数；再复读就收尾
pub const MAX_RETRIES: i64 = 2;
/// 和之前几步的正文相似度达到这么多算「说差不多的话」
pub const SAME_SAYING: f64 = 0.6;
/// 去掉空白标点后至少这么多字才比
pub const SAYING_MINIMUM: usize = 12;

/// 一次回复里的复读：复读开始的位置（第二遍的开头，UTF-16 下标：第一遍留着）和那一段
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loop {
    pub start: usize,
    pub unit: String,
}

/// 一段（≥ min_period 个字、不是分隔线这类）在末尾连着出现了 copies 遍
pub fn trailing_loop(all: &[u16], min_period: usize, copies: usize) -> Option<Loop> {
    // 末尾一小截在前面出现过的位置就是候选周期；最近的几处挨个试，直到周期够长、末尾连着 copies 遍都一样
    const PROBE: usize = 40;
    let n = all.len();
    if copies < 2 || n <= PROBE || n < min_period * copies {
        return None;
    }
    let tail = &all[n - PROBE..];
    let same = |a: usize, b: usize, length: usize| all[a..a + length] == all[b..b + length];
    // 周期 × 遍数超过全文就不用再往前找了（== 两端找到了再 break）：只搜到这里，长文不用每次从尾扫到头
    let lowest = (n - PROBE).saturating_sub(n / copies);
    let mut from = (n - PROBE) as isize - 1;
    let mut tries = 0;
    while tries < 12 && from >= lowest as isize {
        tries += 1;
        let Some(at) = last_index(tail, all, from as usize, lowest) else { break };
        from = at as isize - 1;
        let period = n - PROBE - at;
        if period < min_period {
            continue;
        }
        if period * copies > n {
            break;
        }
        let repeated = (2..=copies).all(|k| same(n - k * period, n - period, period));
        if !repeated || !meaningful(&all[n - period..]) {
            continue;
        }
        // 往前看还有几遍，再一个字一个字往前对齐到复读真正开始的地方：从那里起的第二遍截（第一遍留着）
        let mut first = n - copies * period;
        while first >= period && same(first - period, n - period, period) {
            first -= period;
        }
        while first > 0 && all[first - 1] == all[first - 1 + period] {
            first -= 1;
        }
        return Some(Loop { start: first + period, unit: String::from_utf16_lossy(&all[first..first + period]) });
    }
    None
}

/// 同 [`trailing_loop`]，传字符串
pub fn trailing_loop_in(text: &str, min_period: usize, copies: usize) -> Option<Loop> {
    trailing_loop(&text.encode_utf16().collect::<Vec<_>>(), min_period, copies)
}

/// needle 在 lowest 到 from 之间最后一次出现的位置
fn last_index(needle: &[u16], haystack: &[u16], from: usize, lowest: usize) -> Option<usize> {
    let length = needle.len();
    if haystack.len() < length {
        return None;
    }
    let first = needle[0];
    (lowest..=from.min(haystack.len() - length)).rev().find(|&at| haystack[at] == first && haystack[at..at + length] == *needle)
}

/// 是像样的一段话：去掉空白标点后至少 20 个字、至少 12 种字（分隔线、「哈哈哈」、数字串、表格边框不算）
fn meaningful(unit: &[u16]) -> bool {
    let chars = letters(&String::from_utf16_lossy(unit), false);
    chars.len() >= 20 && chars.iter().collect::<HashSet<_>>().len() >= 12
}

/// 去掉空白、标点（Unicode P*）、符号（S*）后剩下的字（== Swift 的 whitespacesAndNewlines / punctuationCharacters / symbols）
fn letters(text: &str, lowercased: bool) -> Vec<char> {
    static SKIPPED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\s\p{P}\p{S}]+").unwrap());
    let text = if lowercased { text.to_lowercase() } else { text.to_string() };
    SKIPPED.replace_all(&text, "").chars().collect()
}

/// 一轮里每一步都说差不多的话：和之前几步的正文相似（≥ SAME_SAYING、去掉空白标点后至少 SAYING_MINIMUM 个字）的次数
pub fn similar_count(text: &str, earlier: &[String]) -> usize {
    if letters(text, true).len() < SAYING_MINIMUM {
        return 0;
    }
    earlier.iter().filter(|other| similarity(text, other) >= SAME_SAYING).count()
}

/// 两段话的相似度（0–1）：去掉空白和标点后按相邻两字的重合算（Dice）
pub fn similarity(a: &str, b: &str) -> f64 {
    let (x, y) = (bigrams(a), bigrams(b));
    let total: usize = x.values().sum::<usize>() + y.values().sum::<usize>();
    if total == 0 {
        return 0.0;
    }
    let shared: usize = x.iter().map(|(key, count)| (*count).min(y.get(key).copied().unwrap_or(0))).sum();
    2.0 * shared as f64 / total as f64
}

fn bigrams(text: &str) -> HashMap<String, usize> {
    let chars = letters(text, true);
    let mut counts = HashMap::new();
    if chars.len() <= 1 {
        if let Some(only) = chars.first() {
            counts.insert(only.to_string(), 1);
        }
        return counts;
    }
    for pair in chars.windows(2) {
        *counts.entry(pair.iter().collect::<String>()).or_insert(0) += 1;
    }
    counts
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// == Swift 自检「复读」、Qt TestEngines::repetition（2026-10-14 #0f43 的原文）
    pub const UNIT: &str = "让我派 3 个子 Agent：Android 客户端安全审查、Apple iOS 客户端安全审查、Desktop Qt 客户端安全审查。然后我继续审查服务器端的剩余部分（bots, scsh, push）。\n\n";
    const HEAD: &str = "我已经读了 readHello。";

    #[test]
    fn trailing_loops() {
        let u16len = |text: &str| text.encode_utf16().count();
        assert!(trailing_loop_in(&format!("{HEAD}{UNIT}{UNIT}"), REASONING_PERIOD, REASONING_COPIES).is_none());
        let prefix: String = UNIT.chars().take(10).collect();
        let looping = format!("{HEAD}{UNIT}{UNIT}{UNIT}{prefix}");
        let found = trailing_loop_in(&looping, REASONING_PERIOD, REASONING_COPIES).unwrap();
        assert_eq!(u16len(&found.unit), u16len(UNIT));
        assert_eq!(found.start, u16len(HEAD) + u16len(UNIT));
        assert!(trailing_loop_in(&format!("{HEAD}{UNIT}{UNIT}{UNIT}"), CONTENT_PERIOD, CONTENT_COPIES).is_none());
        assert!(trailing_loop_in(&"-".repeat(400), 60, 3).is_none());
        assert!(trailing_loop_in(&"哈哈".repeat(200), 60, 3).is_none());
        assert!(trailing_loop_in(&"| --- | --- |\n".repeat(40), 60, 3).is_none());
        assert!(trailing_loop_in(&"0123456789".repeat(80), 60, 3).is_none());
    }

    #[test]
    fn sayings() {
        let said = [
            "服务器核心质量很高。现在派子 Agent 审查三个客户端，我继续审查服务器其余关键".to_string(),
            "服务器核心质量很高。派子 Agent 审查三个客户端，我继续审查服务器其余安全关键路径".to_string(),
        ];
        assert_eq!(similar_count("服务器核心质量很高。现在派子 Agent 审查三个客户端，我继续审查 WS 握手与调试日志", &said), 2);
        assert_eq!(similar_count("好的", &said), 0);
        assert_eq!(similar_count("读一下 E2E 方案文档和 iOS 的加密实现", &said), 0);
        assert_eq!(similarity("", ""), 0.0);
        assert_eq!(similarity("a", "a"), 1.0);
        assert!((similarity("abcd", "abce") - 2.0 * 2.0 / 6.0).abs() < 1e-9);
    }

    /// 两端原来的写法（没有下界，找到了再 break），对照用
    fn reference(all: &[u16], min_period: usize, copies: usize) -> Option<Loop> {
        let probe = 40;
        let n = all.len();
        if copies < 2 || n <= probe || n < min_period * copies {
            return None;
        }
        let tail = &all[n - probe..];
        let same = |a: usize, b: usize, length: usize| all[a..a + length] == all[b..b + length];
        let mut from = (n - probe) as isize - 1;
        let mut tries = 0;
        while tries < 12 && from >= 0 {
            tries += 1;
            let start = (from as usize).min(n - probe);
            let Some(at) = (0..=start).rev().find(|&at| all[at..at + probe] == *tail) else { break };
            from = at as isize - 1;
            let period = n - probe - at;
            if period < min_period {
                continue;
            }
            if period * copies > n {
                break;
            }
            if !(2..=copies).all(|k| same(n - k * period, n - period, period)) || !meaningful(&all[n - period..]) {
                continue;
            }
            let mut first = n - copies * period;
            while first >= period && same(first - period, n - period, period) {
                first -= period;
            }
            while first > 0 && all[first - 1] == all[first - 1 + period] {
                first -= 1;
            }
            return Some(Loop { start: first + period, unit: String::from_utf16_lossy(&all[first..first + period]) });
        }
        None
    }

    #[test]
    fn same_as_reference() {
        // 伪随机拼：一段开头 + 几遍（有时差一点的）单元 + 半截，单元长短、遍数、字表都变
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound as u64) as usize
        };
        let alphabet: Vec<char> = "的一是了我不人在他有这个上们来到时大地为子中你说生国年着就那和要她出也得里后自以会家可下而过天去能对小多然于心学么之都好看起发当没成只如事把还用第样道想作种开美总从无情己面最女但现前些所同日手又行意动方期它头经长儿回位分爱老因很给名法间斯知世什两次使身者被高已亲其进此话常与活正感abcdefgh 。，\n".chars().collect();
        let mut checked = 0;
        for _ in 0..3000 {
            let mut text = String::new();
            let piece = |length: usize, next: &mut dyn FnMut(usize) -> usize| -> String {
                let kinds = 3 + next(alphabet.len() - 3);
                (0..length).map(|_| alphabet[next(kinds)]).collect()
            };
            let head = piece(next(300), &mut next);
            let unit = piece(1 + next(160), &mut next);
            text += &head;
            for _ in 0..next(12) {
                if next(5) == 0 {
                    text += &piece(1 + next(4), &mut next);
                }
                text += &unit;
            }
            let cut: String = unit.chars().take(next(unit.chars().count() + 1)).collect();
            text += &cut;
            let all: Vec<u16> = text.encode_utf16().collect();
            for (min_period, copies) in [(REASONING_PERIOD, REASONING_COPIES), (CONTENT_PERIOD, CONTENT_COPIES), (20, 2), (1, 3)] {
                assert_eq!(trailing_loop(&all, min_period, copies), reference(&all, min_period, copies), "{text:?} {min_period} {copies}");
                checked += usize::from(reference(&all, min_period, copies).is_some());
            }
        }
        assert!(checked > 300, "复读的样本太少：{checked}");
    }
}
