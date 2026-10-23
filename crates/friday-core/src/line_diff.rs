//! v2.24 改文件的工具（write_file / edit_file）、v2.29 命令改的文件这一步加了几行、删了几行（卡片上的「+N −M」），
//! v2.31 文件卡片的改动视图（每一行的去向）。
//!
//! 按 \n 分行，行尾的 \r 不算；结尾的换行不多算一行；空文本 0 行。先去掉相同的开头和结尾，中间用 Myers 算最少增删；
//! 中间两边合计超过 [`MAX_LINES`] 行、或最少要改的行数超过 [`MAX_EDITS`] 时不细算，按整段换掉计。
//!
//! 每行的指纹是 FNV-1a 64 位、按 UTF-16 算（[`line_hashes`]）。分行和算指纹两端也各有一份（命令执行前要扫整个工作目录，
//! 每个文件的全文都过 C 接口太慢），三边按同一个算法、有交叉测试；指纹过 C 接口时写成 16 位十六进制（JSON 的数字装不下 64 位）。
//!
//! 原来是 Swift `Models/LineDiff.swift` 和 Qt `models/LineDiff.cpp` 各写一份。

pub const MAX_LINES: usize = 20_000;
pub const MAX_EDITS: usize = 2_000;

/// 加了几行、删了几行
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    pub added: usize,
    pub removed: usize,
}

/// 每一行的去向
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Same,
    Removed,
    Added,
}

impl Op {
    pub fn name(self) -> &'static str {
        match self {
            Op::Same => "same",
            Op::Removed => "removed",
            Op::Added => "added",
        }
    }
}

/// 改动视图的一行：old_line / new_line 从 1 起，这一边没有为 0
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub op: Op,
    pub old_line: usize,
    pub new_line: usize,
    pub text: String,
}

/// 按 \n 分行，去掉行尾的 \r；结尾的换行不多算一行
pub fn lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines.into_iter().map(|line| line.strip_suffix('\r').unwrap_or(line)).collect()
}

/// 每行的指纹（FNV-1a 64 位，按 UTF-16 算；== 两端的 lineHashes）
pub fn line_hashes(text: &str) -> Vec<u64> {
    lines(text)
        .into_iter()
        .map(|line| {
            line.encode_utf16().fold(14_695_981_039_346_656_037u64, |hash, unit| (hash ^ u64::from(unit)).wrapping_mul(1_099_511_628_211))
        })
        .collect()
}

/// 相同的开头、结尾各有几行
fn trim(a: &[u64], b: &[u64]) -> (usize, usize) {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..].iter().rev().zip(b[prefix..].iter().rev()).take_while(|(x, y)| x == y).count();
    (prefix, suffix)
}

/// 按全文算
pub fn count_text(old: &str, new: &str) -> Counts {
    count(&line_hashes(old), &line_hashes(new))
}

/// 按每行的指纹算（v2.29 命令改的文件执行前只留指纹、不留全文）
pub fn count(a: &[u64], b: &[u64]) -> Counts {
    let (prefix, suffix) = trim(a, b);
    let n = a.len() - prefix - suffix;
    let m = b.len() - prefix - suffix;
    let whole = Counts { added: m, removed: n };
    if n == 0 || m == 0 || n + m > MAX_LINES {
        return whole;
    }
    let Some(edits) = myers(&a[prefix..prefix + n], &b[prefix..prefix + m]) else { return whole };
    let common = (n + m - edits) / 2;
    Counts { added: m - common, removed: n - common }
}

/// 最少要增删几行（超过 MAX_EDITS 返回 None）
fn myers(a: &[u64], b: &[u64]) -> Option<usize> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let limit = MAX_EDITS.min(a.len() + b.len()) as isize;
    let offset = limit + 1;
    let mut v = vec![0isize; (2 * limit + 3) as usize];
    for d in 0..=limit {
        for k in (-d..=d).step_by(2) {
            let index = |k: isize| (offset + k) as usize;
            let mut x = if k == -d || (k != d && v[index(k - 1)] < v[index(k + 1)]) { v[index(k + 1)] } else { v[index(k - 1)] + 1 };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[index(k)] = x;
            if x >= n && y >= m {
                return Some(d as usize);
            }
        }
    }
    None
}

/// 每一行的去向：同 count 先去掉相同的开头结尾，中间用 Myers 回溯出最少增删；太大按整段换掉。连着的一段改动里先列删的、再列加的
pub fn script(a: &[u64], b: &[u64]) -> Vec<Op> {
    let (prefix, suffix) = trim(a, b);
    let n = a.len() - prefix - suffix;
    let m = b.len() - prefix - suffix;
    let middle = if n > 0 && m > 0 && n + m <= MAX_LINES { myers_script(&a[prefix..prefix + n], &b[prefix..prefix + m]) } else { None };
    let raw = middle.unwrap_or_else(|| [vec![Op::Removed; n], vec![Op::Added; m]].concat());
    let mut ops = vec![Op::Same; prefix];
    ops.reserve(a.len() + b.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == Op::Same {
            ops.push(Op::Same);
            i += 1;
            continue;
        }
        let mut end = i;
        let mut removed = 0;
        while end < raw.len() && raw[end] != Op::Same {
            if raw[end] == Op::Removed {
                removed += 1;
            }
            end += 1;
        }
        ops.extend(std::iter::repeat_n(Op::Removed, removed));
        ops.extend(std::iter::repeat_n(Op::Added, end - i - removed));
        i = end;
    }
    ops.extend(std::iter::repeat_n(Op::Same, suffix));
    ops
}

/// 同 myers，但记下每一步的 v（只留 [-d-1, d+1] 这一段），回溯出每行的去向；超过 MAX_EDITS 返回 None
fn myers_script(a: &[u64], b: &[u64]) -> Option<Vec<Op>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let limit = MAX_EDITS.min(a.len() + b.len()) as isize;
    let offset = limit + 1;
    let mut v = vec![0isize; (2 * limit + 3) as usize];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut done = false;
    let mut d = 0;
    while d <= limit && !done {
        trace.push(v[(offset - d - 1) as usize..=(offset + d + 1) as usize].to_vec());
        for k in (-d..=d).step_by(2) {
            let index = |k: isize| (offset + k) as usize;
            let mut x = if k == -d || (k != d && v[index(k - 1)] < v[index(k + 1)]) { v[index(k + 1)] } else { v[index(k - 1)] + 1 };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[index(k)] = x;
            if x >= n && y >= m {
                done = true;
                break;
            }
        }
        d += 1;
    }
    if !done {
        return None;
    }
    // 从终点往回走：每一步先是一段相同的（斜着走），再是一次删（往右）或加（往下）
    let mut reversed = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (0..trace.len() as isize).rev() {
        let vd = &trace[d as usize];
        let at = |k: isize| vd[(k + d + 1) as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) { k + 1 } else { k - 1 };
        let prev_x = at(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            reversed.push(Op::Same);
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            reversed.push(if x == prev_x { Op::Added } else { Op::Removed });
        }
        x = prev_x;
        y = prev_y;
    }
    reversed.reverse();
    Some(reversed)
}

/// 改之前、改之后的全文逐行对出来（没改的行也在里面，折叠交给界面）
pub fn diff_lines(old: &str, new: &str) -> Vec<Line> {
    let (before, after) = (lines(old), lines(new));
    let (mut i, mut j) = (0, 0);
    script(&line_hashes(old), &line_hashes(new))
        .into_iter()
        .map(|op| {
            let line = match op {
                Op::Same => Line { op, old_line: i + 1, new_line: j + 1, text: after.get(j).copied().unwrap_or("").to_string() },
                Op::Removed => Line { op, old_line: i + 1, new_line: 0, text: before.get(i).copied().unwrap_or("").to_string() },
                Op::Added => Line { op, old_line: 0, new_line: j + 1, text: after.get(j).copied().unwrap_or("").to_string() },
            };
            if op != Op::Added {
                i += 1;
            }
            if op != Op::Removed {
                j += 1;
            }
            line
        })
        .collect()
}

/// 指纹写成 16 位十六进制（C 接口用）
pub fn hex(hash: u64) -> String {
    format!("{hash:016x}")
}

pub fn parse_hex(text: &str) -> Option<u64> {
    u64::from_str_radix(text, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// == Swift 自检 lineDiff 和 Qt TestTools::lineDiff 的同一组
    #[test]
    fn counting() {
        let cases = [
            ("", "", 0, 0),
            ("", "a\nb\n", 2, 0),
            ("a\nb\nc\n", "", 0, 3),
            ("a\nb\nc\n", "a\nb\nc", 0, 0),
            ("a\r\nb\r\n", "a\nb\n", 0, 0),
            ("a\nb\nc\n", "a\nB\nc\n", 1, 1),
            ("a\nb\nc\nd\n", "a\nc\nd\ne\n", 1, 1),
            ("x\na\ny\nb\nz\n", "a\nq\nb\nr\n", 2, 3),
            ("1\n2\n3\n4\n5\n6\n", "1\n2\nx\n3\n4\n6\n", 1, 1),
        ];
        for (old, new, added, removed) in cases {
            assert_eq!(count_text(old, new), Counts { added, removed }, "{old:?} → {new:?}");
        }
        // 太长不细算：按中间整段换掉计
        let big: Vec<String> = (0..12_000).map(|i| format!("line {i}")).collect();
        let reversed: Vec<String> = big.iter().rev().cloned().collect();
        assert_eq!(count_text(&big.join("\n"), &reversed.join("\n")), Counts { added: 12_000, removed: 12_000 });
    }

    /// == Swift 自检 diffScript 和 Qt TestTools::diffScript
    #[test]
    fn scripts() {
        let line = |op, old_line, new_line, text: &str| Line { op, old_line, new_line, text: text.to_string() };
        assert_eq!(
            diff_lines("a\nb\nc\n", "a\nB\nc\n"),
            [line(Op::Same, 1, 1, "a"), line(Op::Removed, 2, 0, "b"), line(Op::Added, 0, 2, "B"), line(Op::Same, 3, 3, "c")]
        );
        assert_eq!(diff_lines("", "x\n"), [line(Op::Added, 0, 1, "x")]);
        assert_eq!(diff_lines("x\r\n", ""), [line(Op::Removed, 1, 0, "x")]);
        // 随机的：两边按去向拼回去就是原文，删的排在加的前面，和 count 一致
        let mut seed: u64 = 31;
        let mut next = |bound: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) % bound
        };
        for _ in 0..300 {
            let a: Vec<String> = (0..next(40)).map(|_| ((b'a' + next(5) as u8) as char).to_string()).collect();
            let b: Vec<String> = (0..next(40)).map(|_| ((b'a' + next(5) as u8) as char).to_string()).collect();
            let (a, b) = (a.join("\n"), b.join("\n"));
            let result = diff_lines(&a, &b);
            let left: Vec<&str> = result
                .iter()
                .filter(|l| l.op != Op::Added)
                .map(|l| if l.op == Op::Same { lines(&a)[l.old_line - 1] } else { &l.text })
                .collect();
            let right: Vec<&str> = result.iter().filter(|l| l.op != Op::Removed).map(|l| l.text.as_str()).collect();
            assert_eq!(left, lines(&a));
            assert_eq!(right, lines(&b));
            assert!(result.windows(2).all(|pair| !(pair[0].op == Op::Added && pair[1].op == Op::Removed)));
            let added = result.iter().filter(|l| l.op == Op::Added).count();
            let removed = result.iter().filter(|l| l.op == Op::Removed).count();
            assert_eq!(Counts { added, removed }, count_text(&a, &b));
        }
        // 太长不细算：中间整段先删后加
        let big: Vec<String> = (0..12_000).map(|i| format!("line {i}")).collect();
        let reversed: Vec<String> = big.iter().rev().cloned().collect();
        let whole = script(&line_hashes(&big.join("\n")), &line_hashes(&reversed.join("\n")));
        assert_eq!(whole.len(), 24_000);
        assert!(whole[0] == Op::Removed && whole[11_999] == Op::Removed && whole[12_000] == Op::Added);
    }

    /// 指纹和两端一样（两端的测试用同一组值对照）
    #[test]
    fn hashes() {
        assert_eq!(line_hashes(""), Vec::<u64>::new());
        assert_eq!(line_hashes("a\r\n中\n").iter().map(|hash| hex(*hash)).collect::<Vec<_>>(), ["af63dc4c8601ec8c", "afada04c867f4498"]);
        assert_eq!(parse_hex(&hex(u64::MAX)), Some(u64::MAX));
    }
}
