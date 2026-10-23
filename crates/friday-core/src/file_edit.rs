//! edit_file：把文件里的一段原文（old_text）换成新内容（new_text）（== Swift `Agent/FileEdit.swift`、Qt `agent/FileEdit.cpp`）。
//! 按 UTF-16 逐字比较（== NSString 的 .literal、QString::indexOf），位置、长度都按 UTF-16 算；文件是 \r\n 换行、而 old_text 用的是 \n 时，
//! 两段都换成 \r\n 再找。old_text 必须只出现一次，不然报出现在哪几行、让它多带上下文或设 replace_all。几处一起改用 apply_all
//! （有一处对不上就一处都不改）。v2.5 一字不差找不到时按行宽松地再找一次（先忽略行尾空白，再忽略缩进：缩进差得一致才算，
//! new_text 跟着换缩进），恰好对上一处才改，结果里说明；还是找不到时给出文件里最像的那几行（带行号）。

use std::collections::HashMap;

use unicode_segmentation::UnicodeSegmentation;

/// 一处修改的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edited {
    /// 改完的全文
    pub text: String,
    /// 替换了几处
    pub count: usize,
    /// 第一处改完后在第几行到第几行（1 起）
    pub first_line: usize,
    pub last_line: usize,
    /// 实际换进去的内容（\r\n 文件里换成了 \r\n）
    pub inserted: String,
    /// 换掉的每处在改之前的全文里从哪开始（UTF-16），每处原来多长
    pub replaced_at: Vec<usize>,
    pub removed_length: usize,
    /// 宽松对上的：怎么对上的；一字不差的为空
    pub note: String,
}

/// edits 数组里的一项
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
    pub replace_all: bool,
}

/// 几处一起改的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiEdited {
    pub text: String,
    /// 一共替换了几处（replace_all 的按实际次数算）
    pub count: usize,
    /// 每项实际换进去的内容
    pub inserted: Vec<String>,
    /// 每项（第一处）改完后在最终全文里从哪开始（UTF-16）
    pub starts: Vec<usize>,
    /// 宽松对上的几处的说明
    pub notes: Vec<String>,
}

fn u16s(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

const NEWLINE: u16 = b'\n' as u16;
const RETURN: u16 = b'\r' as u16;

fn find(haystack: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    (from..=haystack.len() - needle.len()).find(|&at| haystack[at..at + needle.len()] == *needle)
}

/// 不重叠地找出每处的位置
fn occurrences(text: &[u16], needle: &[u16]) -> Vec<usize> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = find(text, needle, from) {
        found.push(at);
        from = at + needle.len();
    }
    found
}

fn replace_all(text: &[u16], from: &[u16], to: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(text.len());
    let mut last = 0;
    for at in occurrences(text, from) {
        out.extend_from_slice(&text[last..at]);
        out.extend_from_slice(to);
        last = at + from.len();
    }
    out.extend_from_slice(&text[last..]);
    out
}

fn line_at(text: &[u16], index: usize) -> usize {
    text[..index.min(text.len())].iter().filter(|&&unit| unit == NEWLINE).count() + 1
}

/// 换进去的内容占了几行（结尾的换行不多算）
fn spanned_lines(inserted: &str) -> usize {
    let newlines = inserted.matches('\n').count();
    newlines.saturating_sub(usize::from(inserted.ends_with('\n')))
}

/// 按行拆开（末尾的换行不多算一行），每行去掉结尾的 \r
fn display_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines.into_iter().map(|line| line.strip_suffix('\r').unwrap_or(line).to_string()).collect()
}

/// 第 start–end 行，每行前面带行号；太长的行截到 200 字
fn numbered(lines: &[String], start: usize, end: usize) -> Vec<String> {
    let last = end.min(lines.len());
    if start > last || start == 0 {
        return Vec::new();
    }
    (start..=last)
        .map(|number| {
            let line = &lines[number - 1];
            let graphemes: Vec<&str> = line.graphemes(true).collect();
            let shown = if graphemes.len() > 200 { graphemes[..200].concat() + "…" } else { line.clone() };
            format!("{number}\t{shown}")
        })
        .collect()
}

fn leading_space(line: &str) -> &str {
    let end = line.find(|c| c != ' ' && c != '\t').unwrap_or(line.len());
    &line[..end]
}

fn without_trailing_space(line: &str) -> &str {
    line.trim_end()
}

fn trimmed(line: &str) -> &str {
    line.trim()
}

/// 文件的每一行：从哪开始（UTF-16）、内容（不含 \r\n）
fn file_lines(text: &[u16]) -> Vec<(usize, String)> {
    let mut lines = Vec::new();
    let mut start = 0;
    loop {
        let end = find(text, &[NEWLINE], start);
        let stop = end.unwrap_or(text.len());
        let mut line = &text[start..stop];
        if line.last() == Some(&RETURN) {
            line = &line[..line.len() - 1];
        }
        lines.push((start, String::from_utf16_lossy(line)));
        match end {
            Some(end) => start = end + 1,
            None => break,
        }
    }
    lines
}

/// 按 \n 拆开，每行去掉结尾的 \r
fn split_lines_keep_last(text: &str) -> Vec<String> {
    text.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line).to_string()).collect()
}

struct Loose {
    start: usize,
    length: usize,
    replacement: String,
    how: &'static str,
}

fn loose_match(text: &str, old_text: &str, new_text: &str) -> Option<Loose> {
    let mut old = split_lines_keep_last(old_text);
    let mut fresh = split_lines_keep_last(new_text);
    // old_text 以换行结尾：最后那个空行不参与比较，换进去的也去掉对应的换行（行尾的换行留在原文里）
    if old_text.ends_with('\n') {
        old.pop();
        if new_text.ends_with('\n') {
            fresh.pop();
        }
    }
    if old.is_empty() || old.iter().all(|line| trimmed(line).is_empty()) {
        return None;
    }
    let source = u16s(text);
    let lines = file_lines(&source);
    let n = old.len();
    if lines.len() < n {
        return None;
    }
    let newline = if find(&source, &u16s("\r\n"), 0).is_some() { "\r\n" } else { "\n" };
    for level in 1..=2 {
        let windows: Vec<usize> = (0..=lines.len() - n)
            .filter(|&j| {
                (0..n).all(|i| {
                    let file = &lines[j + i].1;
                    if level == 1 {
                        without_trailing_space(file) == without_trailing_space(&old[i])
                    } else {
                        trimmed(file) == trimmed(&old[i])
                    }
                })
            })
            .collect();
        let [j] = windows[..] else { continue };
        let mut replacement = fresh.clone();
        let mut how = "忽略了行尾空白";
        if level == 2 {
            // 缩进：old_text 第一行非空行的缩进 X 对应文件里的 Y，每一行都得是「Y + 多出来的」，new_text 里以 X 开头的换成 Y
            let Some(first) = old.iter().position(|line| !trimmed(line).is_empty()) else { continue };
            let from = leading_space(&old[first]).to_string();
            let to = leading_space(&lines[j + first].1).to_string();
            let consistent = (0..n).filter(|&i| !trimmed(&old[i]).is_empty()).all(|i| {
                let lead = leading_space(&old[i]);
                lead.starts_with(&from) && leading_space(&lines[j + i].1) == format!("{to}{}", &lead[from.len()..])
            });
            if !consistent {
                continue;
            }
            replacement = replacement
                .into_iter()
                .map(
                    |line| {
                        if !trimmed(&line).is_empty() && line.starts_with(&from) { format!("{to}{}", &line[from.len()..]) } else { line }
                    },
                )
                .collect();
            how = "old_text 的缩进和文件不一样，按内容对上了，new_text 跟着换了缩进";
        }
        let start = lines[j].0;
        let last = &lines[j + n - 1];
        return Some(Loose { start, length: last.0 + last.1.encode_utf16().count() - start, replacement: replacement.join(newline), how });
    }
    None
}

/// 相邻两个字一组的 Dice 系数（去掉空白后按 UTF-16 比）
fn similarity(a: &str, b: &str) -> f64 {
    let x: Vec<u16> = a.chars().filter(|c| !c.is_whitespace()).collect::<String>().encode_utf16().collect();
    let y: Vec<u16> = b.chars().filter(|c| !c.is_whitespace()).collect::<String>().encode_utf16().collect();
    if x.len() < 2 || y.len() < 2 {
        return if !x.is_empty() && x == y { 1.0 } else { 0.0 };
    }
    let mut grams: HashMap<(u16, u16), usize> = HashMap::new();
    for pair in x.windows(2) {
        *grams.entry((pair[0], pair[1])).or_insert(0) += 1;
    }
    let mut common = 0;
    for pair in y.windows(2) {
        if let Some(count) = grams.get_mut(&(pair[0], pair[1]))
            && *count > 0
        {
            *count -= 1;
            common += 1;
        }
    }
    2.0 * common as f64 / (x.len() - 1 + y.len() - 1) as f64
}

/// 找不到 old_text 时：文件里最像的那几行（按 old_text 第一行非空行找），带行号；不够像为空
fn closest_lines(text: &str, old_text: &str) -> String {
    let old: Vec<&str> = old_text.split('\n').collect();
    let Some(probe) = old.iter().map(|line| trimmed(line)).find(|line| !line.is_empty()) else { return String::new() };
    let lines = display_lines(text);
    let mut best = 0.0;
    let mut best_line = None;
    for (index, line) in lines.iter().enumerate() {
        let score = similarity(probe, trimmed(line));
        if score > best {
            best = score;
            best_line = Some(index);
        }
    }
    let Some(best_line) = best_line.filter(|_| best >= 0.6) else { return String::new() };
    let start = best_line.max(1);
    let end = lines.len().min(best_line + 1 + old.len()).min(start + 14);
    format!("最像的是第 {} 行附近（照着这几行的原文改 old_text，包括缩进）：\n{}", best_line + 1, numbered(&lines, start, end).join("\n"))
}

/// 找不到、不唯一（且没设 replace_all）、old_text 为空或和 new_text 一样时返回 Err（给模型看的说明）
pub fn apply(text: &str, old_text: &str, new_text: &str, replace_every: bool) -> Result<Edited, String> {
    if old_text.is_empty() {
        return Err("old_text 不能为空：新建文件或整份重写用 write_file".into());
    }
    if old_text == new_text {
        return Err("old_text 和 new_text 一样，不用改".into());
    }
    let source = u16s(text);
    let (mut from, mut to) = (old_text.to_string(), new_text.to_string());
    let mut found = occurrences(&source, &u16s(&from));
    // 文件是 \r\n 换行、模型给的是 \n：两段都换成 \r\n 再找
    if found.is_empty() && text.contains("\r\n") && from.contains('\n') && !from.contains("\r\n") {
        from = from.replace('\n', "\r\n");
        to = to.replace("\r\n", "\n").replace('\n', "\r\n");
        found = occurrences(&source, &u16s(&from));
    }
    let Some(&first) = found.first() else {
        // v2.5 一字不差找不到：按行宽松地再找一次（行尾空白、缩进），恰好对上一处就改
        if let Some(loose) = loose_match(text, old_text, new_text) {
            let mut edited = source[..loose.start].to_vec();
            edited.extend(loose.replacement.encode_utf16());
            edited.extend_from_slice(&source[loose.start + loose.length..]);
            let first_line = line_at(&source, loose.start);
            return Ok(Edited {
                text: String::from_utf16_lossy(&edited),
                count: 1,
                first_line,
                last_line: first_line + spanned_lines(&loose.replacement),
                inserted: loose.replacement,
                replaced_at: vec![loose.start],
                removed_length: loose.length,
                note: loose.how.to_string(),
            });
        }
        let closest = closest_lines(text, old_text);
        return Err(format!(
            "没找到 old_text：它要和文件里的原文一字不差（包括缩进、空格和换行）。{}",
            if closest.is_empty() {
                "先用 read_file（带 offset / limit）或 search_files 看准原文再改。".to_string()
            } else {
                closest
            }
        ));
    };
    if found.len() > 1 && !replace_every {
        let lines: Vec<String> = found.iter().take(10).map(|&at| line_at(&source, at).to_string()).collect();
        return Err(format!(
            "old_text 在文件里出现了 {} 次（第 {} 行），不知道改哪一处：多带几行上下文让它只出现一次，或者设 replace_all: true 全部替换。",
            found.len(),
            lines.join("、")
        ));
    }
    let (from16, to16) = (u16s(&from), u16s(&to));
    let edited = if replace_every {
        replace_all(&source, &from16, &to16)
    } else {
        let mut edited = source[..first].to_vec();
        edited.extend_from_slice(&to16);
        edited.extend_from_slice(&source[first + from16.len()..]);
        edited
    };
    let first_line = line_at(&source, first);
    Ok(Edited {
        text: String::from_utf16_lossy(&edited),
        count: if replace_every { found.len() } else { 1 },
        first_line,
        last_line: first_line + spanned_lines(&to),
        inserted: to,
        replaced_at: if replace_every { found } else { vec![first] },
        removed_length: from16.len(),
        note: String::new(),
    })
}

/// 给模型看的结果：「已修改 <path>：替换了 N 处（第一处在第 a–b 行）」+ 改动前后各两行（带行号）
pub fn summary(path: &str, result: &Edited) -> String {
    let lines = display_lines(&result.text);
    let start = result.first_line.saturating_sub(2).max(1);
    let end = lines.len().min(result.last_line + 2).min(start + 29);
    let range = if result.first_line == result.last_line {
        format!("第 {} 行", result.first_line)
    } else {
        format!("第 {}–{} 行", result.first_line, result.last_line)
    };
    let how = if result.note.is_empty() { String::new() } else { format!("；{}", result.note) };
    let mut out = vec![format!("已修改 {path}：替换了 {} 处（第一处在{range}{how}），改完是：", result.count)];
    out.extend(numbered(&lines, start, end));
    out.join("\n")
}

/// 按顺序改几处：有一处找不到、不唯一就返回 Err（写明第几处），一处都不改
pub fn apply_all(text: &str, edits: &[Edit]) -> Result<MultiEdited, String> {
    if edits.is_empty() {
        return Err("edits 是空的：至少给一处 {old_text, new_text}".into());
    }
    let mut result = MultiEdited { text: text.to_string(), count: 0, inserted: Vec::new(), starts: Vec::new(), notes: Vec::new() };
    for (index, edit) in edits.iter().enumerate() {
        let step = apply(&result.text, &edit.old_text, &edit.new_text, edit.replace_all)
            .map_err(|error| format!("第 {} 处（共 {} 处）：{error} 这次一处都没改。", index + 1, edits.len()))?;
        // 前面几项的位置：这一步在它们前面换掉的每处都让它们挪 delta；落在被换掉的那段里的挪到那段的开头
        let delta = step.inserted.encode_utf16().count() as isize - step.removed_length as isize;
        result.starts = result
            .starts
            .iter()
            .map(|&start| {
                let mut shift = 0isize;
                let mut moved = start;
                for &at in &step.replaced_at {
                    if at + step.removed_length <= start {
                        shift += delta;
                        continue;
                    }
                    if at <= start {
                        moved = at;
                    }
                    break;
                }
                (moved as isize + shift).max(0) as usize
            })
            .collect();
        result.text = step.text;
        result.count += step.count;
        result.inserted.push(step.inserted);
        result.starts.push(step.replaced_at.first().copied().unwrap_or(0));
        if !step.note.is_empty() {
            result.notes.push(format!("第 {} 处{}", index + 1, step.note));
        }
    }
    Ok(result)
}

/// 「已修改 <path>：N 处修改都改好了」+ 每处改完在第几行、那几行（带行号）
pub fn summary_all(path: &str, result: &MultiEdited) -> String {
    let lines = display_lines(&result.text);
    let source = u16s(&result.text);
    let notes = if result.notes.is_empty() { String::new() } else { format!("；{}", result.notes.join("；")) };
    let mut out = vec![format!("已修改 {path}：{} 处修改都改好了（共替换 {} 处）{notes}，改完是：", result.inserted.len(), result.count)];
    for (index, inserted) in result.inserted.iter().enumerate() {
        let label = format!("— 第 {} 处", index + 1);
        if inserted.is_empty() {
            out.push(format!("{label}：删掉了这段"));
            continue;
        }
        let first = line_at(&source, result.starts[index].min(source.len()));
        let last = first + spanned_lines(inserted);
        out.push(if first == last {
            format!("{label}（第 {first} 行）：")
        } else {
            format!("{label}（第 {first}–{last} 行）：")
        });
        out.extend(numbered(&lines, first.saturating_sub(1).max(1), lines.len().min(last + 1).min(first + 9)));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// == Swift 自检「edit_file」、Qt TestTools::fileEdit
    #[test]
    fn exact_and_errors() {
        let text = "a\nb\nc\nb\n";
        let edited = apply(text, "c", "C", false).unwrap();
        assert_eq!((edited.text.as_str(), edited.count, edited.first_line, edited.last_line), ("a\nb\nC\nb\n", 1, 3, 3));
        assert!(apply(text, "b", "B", false).unwrap_err().contains("出现了 2 次（第 2、4 行）"));
        let all = apply(text, "b", "B\nB", true).unwrap();
        assert_eq!((all.text.as_str(), all.count, all.replaced_at.clone()), ("a\nB\nB\nc\nB\nB\n", 2, vec![2, 6]));
        assert!(apply(text, "", "x", false).unwrap_err().starts_with("old_text 不能为空"));
        assert!(apply(text, "a", "a", false).unwrap_err().starts_with("old_text 和 new_text 一样"));
        assert!(apply(text, "zzz", "x", false).unwrap_err().contains("先用 read_file"));
        // \r\n 文件、\n 的 old_text：两段都换成 \r\n
        let crlf = apply("x\r\ny\r\nz\r\n", "x\ny", "1\n2", false).unwrap();
        assert_eq!((crlf.text.as_str(), crlf.inserted.as_str()), ("1\r\n2\r\nz\r\n", "1\r\n2"));
        // UTF-16 位置
        let emoji = apply("😀前\n后", "后", "後", false).unwrap();
        assert_eq!((emoji.replaced_at.clone(), emoji.first_line), (vec![4], 2));
    }

    #[test]
    fn loose_and_closest() {
        let code = "fn main() {\n    let x = 1;   \n    let y = 2;\n}\n";
        // 缩进不一样（两格对四格）、第一行行尾还有空白：先忽略行尾空白找不到，再忽略缩进找到，new_text 跟着换缩进
        let shifted = apply(code, "  let x = 1;\n  let y = 2;", "  let x = 3;\n  if y {\n    y = 4;\n  }", false).unwrap();
        assert_eq!(shifted.text, "fn main() {\n    let x = 3;\n    if y {\n      y = 4;\n    }\n}\n");
        assert!(shifted.note.contains("缩进"));
        // 缩进差得不一致：不算对上
        assert!(apply(code, "let x = 1;\n    let y = 2;", "x", false).is_err());
        let spaces = apply(code, "    let x = 1;\n", "    let x = 9;\n", false).unwrap();
        assert_eq!(spaces.note, "忽略了行尾空白");
        assert_eq!(spaces.text, "fn main() {\n    let x = 9;\n    let y = 2;\n}\n");
        let indented = apply(code, "\tlet y = 2;\n", "\tlet y = 5;\n\tlet z = 6;\n", false).unwrap();
        assert_eq!(indented.text, "fn main() {\n    let x = 1;   \n    let y = 5;\n    let z = 6;\n}\n");
        let missing = apply(code, "let w = 2;", "x", false).unwrap_err();
        assert!(missing.contains("最像的是第 3 行附近") && missing.contains("3\t    let y = 2;"), "{missing}");
    }

    #[test]
    fn several_and_summaries() {
        let text = "one\ntwo\nthree\nfour\n";
        let edits = [
            Edit { old_text: "three".into(), new_text: "3\n3".into(), replace_all: false },
            Edit { old_text: "one".into(), new_text: "".into(), replace_all: false },
        ];
        let result = apply_all(text, &edits).unwrap();
        assert_eq!(result.text, "\ntwo\n3\n3\nfour\n");
        assert_eq!(result.starts, vec![5, 0]);
        let text_all = summary_all("a.txt", &result);
        assert!(text_all.starts_with("已修改 a.txt：2 处修改都改好了（共替换 2 处），改完是："));
        assert!(text_all.contains("— 第 1 处（第 3–4 行）：") && text_all.contains("— 第 2 处：删掉了这段"), "{text_all}");
        let failed = apply_all(text, &[edits[0].clone(), Edit { old_text: "nope".into(), new_text: "x".into(), replace_all: false }]);
        assert!(failed.unwrap_err().starts_with("第 2 处（共 2 处）：没找到 old_text"));
        let single = apply(text, "two", "2", false).unwrap();
        assert_eq!(summary("a.txt", &single), "已修改 a.txt：替换了 1 处（第一处在第 2 行），改完是：\n1\tone\n2\t2\n3\tthree\n4\tfour");
        assert!(apply_all(text, &[]).unwrap_err().starts_with("edits 是空的"));
    }
}
