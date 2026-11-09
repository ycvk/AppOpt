//! 规则文件的行级编辑：新增/更新、删除、重命名，保留注释与原有书写形态

use std::collections::HashMap;

use crate::grammar::{Node, OuterLine, comment_at, parse_outer, scan, strip_comment};

#[derive(Debug, PartialEq, Eq)]
pub enum EditError {
    NotFound,
    /// 重命名的目标包名已有规则
    Conflict,
    /// 文件存在未闭合的块，无法确定插入位置
    Malformed,
}

enum PkgLine {
    /// `pkg=cpus`
    Standalone(usize),
    /// `pkg=cpus {`
    OpenInline(usize),
    /// `pkg {`
    BareOpen(usize),
    /// `pkg` / `pkg=` 单独成行，`{` 在后续行
    BarePending(usize),
}

#[derive(Clone, Copy)]
struct ThreadLoc {
    idx: usize,
    /// 单行形态 `pkg{thread}=cpus`
    single: bool,
    /// 块内行同时以 `}` 收束
    closed: bool,
    /// 单行形态同时开启块
    open: bool,
}

/// 目标包在文件中的位置信息；块只记录该包的第一个块
struct Target {
    pkg_line: Option<PkgLine>,
    block_open: Option<usize>,
    block_close: Option<usize>,
    threads: HashMap<String, Vec<ThreadLoc>>,
    unterminated: bool,
}

impl Target {
    fn singles(&self) -> impl Iterator<Item = &ThreadLoc> {
        self.threads.values().flatten().filter(|l| l.single)
    }

    fn any_line(&self) -> bool {
        self.pkg_line.is_some() || self.singles().next().is_some()
    }

    fn note_open(&mut self, i: usize) {
        if self.block_close.is_none() {
            self.block_open = Some(i);
        }
    }

    fn note_close(&mut self, i: usize) {
        if self.block_close.is_none() {
            self.block_close = Some(i);
        }
    }

    fn note_pkg_line(&mut self, line: PkgLine) {
        self.pkg_line.get_or_insert(line);
    }

    fn note_thread(&mut self, thread: &str, loc: ThreadLoc) {
        self.threads
            .entry(thread.to_string())
            .or_default()
            .push(loc);
    }
}

fn target_scan<S: AsRef<str>>(lines: &[S], pkg: &str) -> Target {
    let parsed = scan(lines);
    let mut t = Target {
        pkg_line: None,
        block_open: None,
        block_close: None,
        threads: HashMap::new(),
        unterminated: parsed.unterminated,
    };
    for &(i, ref node) in &parsed.nodes {
        match *node {
            Node::Single {
                pkg: p,
                thread,
                open,
                ..
            } if p == pkg => {
                t.note_thread(
                    thread,
                    ThreadLoc {
                        idx: i,
                        single: true,
                        closed: false,
                        open,
                    },
                );
                if open {
                    t.note_open(i);
                }
            }
            Node::Pkg { pkg: p, open, .. } if p == pkg => {
                if open {
                    t.note_open(i);
                    t.note_pkg_line(PkgLine::OpenInline(i));
                } else {
                    t.note_pkg_line(PkgLine::Standalone(i));
                }
            }
            Node::BareOpen { pkg: p, pending } if p == pkg => {
                t.note_open(i);
                t.note_pkg_line(match pending {
                    Some(pi) => PkgLine::BarePending(pi),
                    None => PkgLine::BareOpen(i),
                });
            }
            Node::Member {
                pkg: p,
                thread,
                closed,
                ..
            } if p == pkg => {
                t.note_thread(
                    thread,
                    ThreadLoc {
                        idx: i,
                        single: false,
                        closed,
                        open: false,
                    },
                );
                if closed {
                    t.note_close(i);
                }
            }
            Node::Close { pkg: p }
            | Node::BadMember {
                pkg: p,
                closes: true,
            } if p == pkg => {
                t.note_close(i);
            }
            _ => {}
        }
    }
    t
}

/// 把 pkg 的单行线程规则并入其块：块内已有则追加到块尾，否则把包级行扩成块或新开一个块
fn normalize_singles(lines: &mut Vec<String>, pkg: &str) {
    let t = target_scan(lines, pkg);
    let mut items: Vec<(ThreadLoc, String)> = Vec::new();
    for loc in t.singles() {
        let raw_line = lines[loc.idx].trim();
        if let OuterLine::Single { thread, cpus, .. } = parse_outer(raw_line) {
            let line = with_comment(&format!("\t{thread}={cpus}"), raw_line);
            items.push((*loc, line));
        }
    }
    if items.is_empty() {
        return;
    }
    items.sort_unstable_by_key(|(l, _)| l.idx);
    if t.block_close.is_none()
        && (items.iter().any(|(l, _)| l.open)
            || !matches!(t.pkg_line, None | Some(PkgLine::Standalone(_))))
    {
        return;
    }

    let at = items[0].0.idx;
    for (loc, _) in items.iter().rev() {
        line_remove(lines, pkg, loc);
    }
    let items: Vec<String> = items.into_iter().map(|(_, line)| line).collect();

    let t2 = target_scan(lines, pkg);
    if let Some(close) = t2.block_close {
        for (off, line) in items.into_iter().enumerate() {
            lines.insert(close + off, line);
        }
    } else if let Some(PkgLine::Standalone(i)) = t2.pkg_line {
        expand_block(lines, i, items);
    } else {
        let chunk: Vec<String> = std::iter::once(bare_open_line(pkg))
            .chain(items)
            .chain(std::iter::once("}".to_string()))
            .collect();
        let at = at.min(lines.len());
        lines.splice(at..at, chunk);
    }
}

/// 把独立的包级行 i 扩成块，块内放入 items
fn expand_block(lines: &mut Vec<String>, i: usize, items: impl IntoIterator<Item = String>) {
    let open = format!("{} {{", strip_comment(&lines[i]).trim_end());
    lines[i] = with_comment(&open, &lines[i]);
    lines.splice(i + 1..i + 1, items.into_iter().chain(["}".to_string()]));
}

fn bare_open_line(pkg: &str) -> String {
    if pkg.contains('=') {
        format!("{pkg}= {{")
    } else {
        format!("{pkg} {{")
    }
}

fn with_comment(new_line: &str, old: &str) -> String {
    match comment_at(old) {
        Some(at) => format!("{}{}", new_line, &old[at..]),
        None => new_line.to_string(),
    }
}

/// 替换行内 `=` 之后的 cpus，保留其后的 `{` `}` 与注释
fn spec_swap(raw: &str, cpus: &str) -> String {
    let cut = comment_at(raw).unwrap_or(raw.len());
    let Some(eq) = raw[..cut].rfind('=') else {
        return raw.into();
    };
    let rhs = &raw[eq + 1..cut];
    let val = rhs.trim_start();
    let lead = rhs.len() - val.len();
    let v_end = val
        .find(|c: char| c.is_whitespace() || c == '{' || c == '}')
        .unwrap_or(val.len());
    let tail: String = val[v_end..]
        .chars()
        .filter(|c| c.is_whitespace() || *c == '{' || *c == '}')
        .collect();
    format!("{}{}{}{}", &raw[..eq + 1 + lead], cpus, tail, &raw[cut..])
}

/// 删除一条线程规则所在行；该行兼具块开/闭作用时只降级为裸 `pkg {` / `}`
fn line_remove(lines: &mut Vec<String>, pkg: &str, loc: &ThreadLoc) {
    if loc.open {
        lines[loc.idx] = with_comment(&bare_open_line(pkg), &lines[loc.idx]);
    } else if loc.closed {
        lines[loc.idx] = with_comment("}", &lines[loc.idx]);
    } else {
        lines.remove(loc.idx);
    }
}

/// 设置 pkg 的包级规则（thread 为空）或线程规则
pub fn upsert(
    lines: &mut Vec<String>,
    pkg: &str,
    thread: &str,
    cpus: &str,
) -> Result<(), EditError> {
    normalize_singles(lines, pkg);
    let t = target_scan(lines, pkg);

    if thread.is_empty() {
        match t.pkg_line {
            Some(PkgLine::Standalone(i) | PkgLine::OpenInline(i)) => {
                lines[i] = spec_swap(&lines[i], cpus);
            }
            Some(PkgLine::BarePending(i)) => {
                let mut line = with_comment(&format!("{pkg}={cpus} {{"), &lines[i]);
                // 单独成行的 `{` 并入包名行，其注释接在后面
                if let Some(open) = t.block_open
                    && matches!(
                        parse_outer(lines[open].trim()),
                        OuterLine::BareOpen { pkg: "" }
                    )
                {
                    let brace = lines.remove(open);
                    if let Some(at) = comment_at(&brace) {
                        line.push_str(&brace[at..]);
                    }
                }
                lines[i] = line;
            }
            Some(PkgLine::BareOpen(i)) => {
                lines[i] = with_comment(&format!("{pkg}={cpus} {{"), &lines[i]);
            }
            None if t.unterminated => return Err(EditError::Malformed),
            None => lines.push(format!("{pkg}={cpus}")),
        }
    } else if let Some(locs) = t.threads.get(thread) {
        let last = locs.last().copied().expect("线程位置表不含空项");
        lines[last.idx] = spec_swap(&lines[last.idx], cpus);
        for loc in locs[..locs.len() - 1].iter().rev() {
            line_remove(lines, pkg, loc);
        }
    } else if let Some(close) = t.block_close {
        lines.insert(close, format!("\t{thread}={cpus}"));
    } else if let Some(PkgLine::Standalone(i)) = t.pkg_line {
        expand_block(lines, i, [format!("\t{thread}={cpus}")]);
    } else if t.unterminated {
        return Err(EditError::Malformed);
    } else {
        lines.push(bare_open_line(pkg));
        lines.push(format!("\t{thread}={cpus}"));
        lines.push("}".to_string());
    }
    Ok(())
}

/// 删除包级规则（thread 为空）或指定线程规则
pub fn delete(lines: &mut Vec<String>, pkg: &str, thread: &str) -> Result<(), EditError> {
    normalize_singles(lines, pkg);
    let t = target_scan(lines, pkg);

    if thread.is_empty() {
        match t.pkg_line {
            Some(PkgLine::Standalone(i)) => {
                lines.remove(i);
            }
            Some(PkgLine::OpenInline(i)) => {
                lines[i] = with_comment(&bare_open_line(pkg), &lines[i])
            }
            _ => return Err(EditError::NotFound),
        }
    } else if let Some(locs) = t.threads.get(thread) {
        for loc in locs.iter().rev() {
            line_remove(lines, pkg, loc);
        }
    } else {
        return Err(EditError::NotFound);
    }
    Ok(())
}

/// 删除 pkg 的全部规则及其块
pub fn delete_pkg(lines: &mut Vec<String>, pkg: &str) -> Result<(), EditError> {
    normalize_singles(lines, pkg);

    let mut removed = false;
    loop {
        let t = target_scan(lines, pkg);
        let mut del: Vec<usize> = Vec::new();
        match t.pkg_line {
            Some(
                PkgLine::Standalone(i)
                | PkgLine::OpenInline(i)
                | PkgLine::BareOpen(i)
                | PkgLine::BarePending(i),
            ) => del.push(i),
            None if !t.any_line() => break,
            None => {}
        }
        if let Some(open) = t.block_open {
            let end = t
                .block_close
                .or_else(|| {
                    t.threads
                        .values()
                        .flatten()
                        .filter(|l| !l.single)
                        .map(|l| l.idx)
                        .max()
                })
                .unwrap_or(open);
            del.extend(open..=end);
        }
        del.extend(t.singles().map(|l| l.idx));

        del.sort_unstable();
        del.dedup();
        for i in del.into_iter().rev() {
            lines.remove(i);
        }
        removed = true;
    }

    if removed {
        Ok(())
    } else {
        Err(EditError::NotFound)
    }
}

/// 把 old 的全部规则行改名为 new
pub fn rename(lines: &mut Vec<String>, old: &str, new: &str) -> Result<(), EditError> {
    normalize_singles(lines, old);
    if !target_scan(lines, old).any_line() {
        return Err(EditError::NotFound);
    }
    if target_scan(lines, new).any_line() {
        return Err(EditError::Conflict);
    }

    loop {
        let t = target_scan(lines, old);
        let mut idxs: Vec<usize> = t.singles().map(|l| l.idx).collect();
        if let Some(
            PkgLine::Standalone(i)
            | PkgLine::OpenInline(i)
            | PkgLine::BareOpen(i)
            | PkgLine::BarePending(i),
        ) = t.pkg_line
        {
            idxs.push(i);
        }
        if idxs.is_empty() {
            return Ok(());
        }
        for i in idxs {
            let line = lines[i].trim();
            let rest = line.strip_prefix(old).expect("规则行以包名开头");
            // 含 `=` 的包名在 `pkg` / `pkg {` 形态下要补写 `=`，否则会被读成 `pkg=cpus`
            let tail = match (new.contains('='), strip_comment(rest).trim()) {
                (true, "" | "=") => with_comment("=", rest),
                (true, "{") => with_comment("= {", rest),
                _ => rest.to_string(),
            };
            lines[i] = format!("{new}{tail}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(String::from).collect()
    }

    fn text(lines: &[String]) -> String {
        lines.join("\n")
    }

    fn edit(src: &str, f: impl FnOnce(&mut Vec<String>) -> Result<(), EditError>) -> String {
        let mut l = lines(src);
        f(&mut l).unwrap();
        text(&l)
    }

    #[test]
    fn upsert_package_rule() {
        assert_eq!(edit("", |l| upsert(l, "a", "", "0-3")), "a=0-3");
        assert_eq!(
            edit("a=0 # keep\nb=1", |l| upsert(l, "a", "", "e-core")),
            "a=e-core # keep\nb=1"
        );
        assert_eq!(
            edit("a=0 {\n t=1\n}", |l| upsert(l, "a", "", "2")),
            "a=2 {\n t=1\n}"
        );
        assert_eq!(
            edit("a {\n t=1\n}", |l| upsert(l, "a", "", "2")),
            "a=2 {\n t=1\n}"
        );
    }

    #[test]
    fn upsert_thread_rule() {
        assert_eq!(edit("a=0", |l| upsert(l, "a", "T", "1")), "a=0 {\n\tT=1\n}");
        assert_eq!(
            edit("a=0 {\n X=2\n}", |l| upsert(l, "a", "T", "1")),
            "a=0 {\n X=2\n\tT=1\n}"
        );
        assert_eq!(
            edit("a=0 {\n T=2 # c\n}", |l| upsert(l, "a", "T", "1")),
            "a=0 {\n T=1 # c\n}"
        );
        assert_eq!(
            edit("x=0", |l| upsert(l, "a", "T", "1")),
            "x=0\na {\n\tT=1\n}"
        );
    }

    #[test]
    fn single_line_rules_fold_into_block() {
        assert_eq!(
            edit("a{T}=1\na=0", |l| upsert(l, "a", "U", "2")),
            "a=0 {\n\tT=1\n\tU=2\n}"
        );
        assert_eq!(
            edit("a{T}=1", |l| upsert(l, "a", "T", "3")),
            "a {\n\tT=3\n}"
        );
    }

    #[test]
    fn unterminated_block_is_reported() {
        let mut l = lines("x {\n T=1\n");
        assert_eq!(upsert(&mut l, "a", "", "0"), Err(EditError::Malformed));
        let mut l = lines("a {\n T=1\n");
        assert_eq!(upsert(&mut l, "a", "U", "2"), Err(EditError::Malformed));
        // 已存在的线程规则无需定位块尾，可直接更新
        assert!(upsert(&mut l, "a", "T", "2").is_ok());
    }

    #[test]
    fn delete_rules() {
        assert_eq!(edit("a=0\nb=1", |l| delete(l, "a", "")), "b=1");
        assert_eq!(
            edit("a=0 {\n T=1\n}", |l| delete(l, "a", "")),
            "a {\n T=1\n}"
        );
        assert_eq!(
            edit("a {\n T=1\n U=2\n}", |l| delete(l, "a", "T")),
            "a {\n U=2\n}"
        );
        let mut l = lines("a=0");
        assert_eq!(delete(&mut l, "a", "T"), Err(EditError::NotFound));
        assert_eq!(delete(&mut l, "b", ""), Err(EditError::NotFound));
    }

    #[test]
    fn delete_whole_package() {
        assert_eq!(edit("a=0 {\n T=1\n}\nb=2", |l| delete_pkg(l, "a")), "b=2");
        assert_eq!(edit("a=0\nb=1\na{T}=2", |l| delete_pkg(l, "a")), "b=1");
        let mut l = lines("b=1");
        assert_eq!(delete_pkg(&mut l, "a"), Err(EditError::NotFound));
    }

    #[test]
    fn rename_package() {
        assert_eq!(
            edit("a=0 {\n T=1\n}", |l| rename(l, "a", "z")),
            "z=0 {\n T=1\n}"
        );
        assert_eq!(
            edit("a=0\na{T}=1", |l| rename(l, "a", "z")),
            "z=0 {\n\tT=1\n}"
        );
        let mut l = lines("a=0\nz=1");
        assert_eq!(rename(&mut l, "a", "z"), Err(EditError::Conflict));
        assert_eq!(rename(&mut l, "q", "w"), Err(EditError::NotFound));
    }

    #[test]
    fn edits_keep_end_of_line_comments() {
        type Edit = fn(&mut Vec<String>) -> Result<(), EditError>;
        let cases: [(&str, Edit, &str); 6] = [
            // 带注释的包级行扩成块时，`{` 写在注释之前
            (
                "a=0 # 说明",
                |l| upsert(l, "a", "T", "1"),
                "a=0 { # 说明\n\tT=1\n}",
            ),
            (
                "a=0 # 说明\na{T}=1",
                |l| upsert(l, "a", "U", "2"),
                "a=0 { # 说明\n\tT=1\n\tU=2\n}",
            ),
            // 删包级规则时保留块开头行的注释
            (
                "a=0 { # 说明\n T=1\n}",
                |l| delete(l, "a", ""),
                "a { # 说明\n T=1\n}",
            ),
            // 合并单独成行的 `{` 时带上它的注释
            (
                "a # x\n{ # y\n T=1\n}",
                |l| upsert(l, "a", "", "2"),
                "a=2 { # x # y\n T=1\n}",
            ),
            // 改名为含 `=` 的包名时，注释不影响补写的 `=`
            (
                "a # c\n{\n T=1\n}",
                |l| rename(l, "a", "p=q"),
                "p=q= # c\n{\n T=1\n}",
            ),
            (
                "a { # c\n T=1\n}",
                |l| rename(l, "a", "p=q"),
                "p=q= { # c\n T=1\n}",
            ),
        ];
        for (src, f, want) in cases {
            let out = edit(src, f);
            assert_eq!(out, want, "输入:\n{src}");
            let l = lines(&out);
            assert_eq!(scan(&l).malformed, 0, "结果无法完整解析:\n{out}");
        }
    }
}
