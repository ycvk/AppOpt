//! applist.conf 语法：逐行分类与块结构扫描，规则加载与规则编辑共用同一份解析
//!
//! 行形态（`#`、`//` 开头为注释，行内注释需前置空白）：
//! - `pkg=cpus`                 包级规则
//! - `pkg{thread}=cpus`         单行线程规则
//! - `pkg=cpus {` / `pkg {`     开启块，`{` 也可单独成行接在 `pkg` / `pkg=` 之后
//! - `thread=cpus`              块内线程规则，行尾可带 `}` 收束
//! - `}`                        结束块

/// 注释起点（前置空白被裁掉后的行内长度）
pub fn comment_at(s: &str) -> Option<usize> {
    let mut prev_ws = false;
    for (i, c) in s.char_indices() {
        if prev_ws && (c == '#' || (c == '/' && s[i..].starts_with("//"))) {
            return Some(s[..i].trim_end().len());
        }
        prev_ws = c.is_whitespace();
    }
    None
}

pub fn strip_comment(s: &str) -> &str {
    &s[..comment_at(s).unwrap_or(s.len())]
}

/// 块内规则行 `thread=cpus`，返回 (thread, cpus, 行内是否以 `}` 收束)
pub fn split_rule_line(p: &str) -> Option<(&str, &str, bool)> {
    let p = strip_comment(p);
    fn kv(s: &str) -> Option<(&str, &str)> {
        s.rfind('=')
            .map(|eq| (s[..eq].trim(), s[eq + 1..].trim()))
            .filter(|(k, _)| !k.is_empty())
    }
    p.match_indices('}')
        .find_map(|(cb, _)| kv(&p[..cb]).map(|(k, v)| (k, v, true)))
        .or_else(|| kv(p).map(|(k, v)| (k, v, false)))
}

/// 以 `}` 开头且其后仅有空白或注释
pub fn close_like(p: &str) -> bool {
    p.strip_prefix('}').is_some_and(|r| {
        r.is_empty()
            || r.starts_with(char::is_whitespace)
            || r.starts_with('#')
            || r.starts_with("//")
    })
}

/// 单行线程规则主体 `pkg{thread}=cpus`
pub fn split_single_line(body: &str) -> Option<(&str, &str, &str)> {
    let eq = body.rfind('=')?;
    let cpus = body[eq + 1..].trim();
    let left = body[..eq].trim_end().strip_suffix('}')?.trim_end();
    let ob = left.find('{')?;
    let (pkg, thread) = (left[..ob].trim(), left[ob + 1..].trim());
    (!pkg.is_empty() && !thread.is_empty()).then_some((pkg, thread, cpus))
}

/// 块外一行的形态
pub enum OuterLine<'a> {
    Rule {
        pkg: &'a str,
        cpus: &'a str,
        open: bool,
    },
    BareOpen {
        pkg: &'a str,
    },
    Pending {
        pkg: &'a str,
    },
    Single {
        pkg: &'a str,
        thread: &'a str,
        cpus: &'a str,
        open: bool,
    },
    Junk,
}

pub fn parse_outer(p: &str) -> OuterLine<'_> {
    let p = strip_comment(p);
    let (open, body) = match p.strip_suffix('{') {
        Some(b) => (true, b.trim_end()),
        None => (false, p),
    };
    if let Some((pkg, thread, cpus)) = split_single_line(body) {
        return OuterLine::Single {
            pkg,
            thread,
            cpus,
            open,
        };
    }
    if !open && close_like(body) {
        return OuterLine::Junk;
    }
    match body.rfind('=') {
        Some(eq) => {
            let (pkg, cpus) = (body[..eq].trim(), body[eq + 1..].trim());
            if cpus.is_empty() {
                return if open {
                    OuterLine::BareOpen { pkg }
                } else {
                    OuterLine::Pending { pkg }
                };
            }
            OuterLine::Rule { pkg, cpus, open }
        }
        None if open => OuterLine::BareOpen { pkg: body },
        None => OuterLine::Pending { pkg: body },
    }
}

/// 一行在文件结构中的角色，pkg 为所属包名
#[derive(Debug, PartialEq, Eq)]
pub enum Node<'a> {
    /// `pkg=cpus`，open 表示行尾带 `{`
    Pkg {
        pkg: &'a str,
        cpus: &'a str,
        open: bool,
    },
    /// `pkg{thread}=cpus`
    Single {
        pkg: &'a str,
        thread: &'a str,
        cpus: &'a str,
        open: bool,
    },
    /// 没有 cpus 的块开头；`{` 单独成行时 pending 为上一行 `pkg` / `pkg=` 的行号
    BareOpen {
        pkg: &'a str,
        pending: Option<usize>,
    },
    /// 等待下一行 `{` 的 `pkg` / `pkg=`
    Pending { pkg: &'a str },
    /// 块内 `thread=cpus`
    Member {
        pkg: &'a str,
        thread: &'a str,
        cpus: &'a str,
        closed: bool,
    },
    /// 块内单独的 `}`
    Close { pkg: &'a str },
    /// 块内无法解析的行，closes 表示其中含 `}` 而结束了块
    BadMember { pkg: &'a str, closes: bool },
    /// 块外无法归类的行
    Junk,
}

/// 整个文件的结构化结果
pub struct Scan<'a> {
    /// (行号, 节点)，空行与注释不产生节点
    pub nodes: Vec<(usize, Node<'a>)>,
    /// 结构层面的异常数：悬空的 `pkg`、无法解析的行、无主的 `{`、文件结尾未闭合
    pub malformed: usize,
    /// 文件结尾仍在块内
    pub unterminated: bool,
}

pub fn scan<S: AsRef<str>>(lines: &[S]) -> Scan<'_> {
    let mut nodes = Vec::new();
    let mut malformed = 0;
    // 当前所在块的包名
    let mut owner: Option<&str> = None;
    // 等待 `{` 的包名行
    let mut pending: Option<(usize, &str)> = None;

    for (i, raw) in lines.iter().enumerate() {
        let p = raw.as_ref().trim();
        if p.is_empty() || p.starts_with('#') || p.starts_with("//") {
            continue;
        }

        if let Some(pkg) = owner {
            if close_like(p) {
                nodes.push((i, Node::Close { pkg }));
                owner = None;
            } else if let Some((thread, cpus, closed)) = split_rule_line(p) {
                nodes.push((
                    i,
                    Node::Member {
                        pkg,
                        thread,
                        cpus,
                        closed,
                    },
                ));
                if closed {
                    owner = None;
                }
            } else {
                let closes = p.contains('}');
                malformed += 1;
                nodes.push((i, Node::BadMember { pkg, closes }));
                if closes {
                    owner = None;
                }
            }
            continue;
        }

        let prev = pending.take();
        let dangling = prev.is_some();
        match parse_outer(p) {
            OuterLine::Single {
                pkg,
                thread,
                cpus,
                open,
            } => {
                malformed += dangling as usize;
                nodes.push((
                    i,
                    Node::Single {
                        pkg,
                        thread,
                        cpus,
                        open,
                    },
                ));
                if open {
                    owner = Some(pkg);
                }
            }
            OuterLine::Rule { pkg, cpus, open } => {
                malformed += dangling as usize;
                nodes.push((i, Node::Pkg { pkg, cpus, open }));
                if open {
                    owner = Some(pkg);
                }
            }
            OuterLine::BareOpen { pkg } if !pkg.is_empty() => {
                malformed += dangling as usize;
                nodes.push((i, Node::BareOpen { pkg, pending: None }));
                owner = Some(pkg);
            }
            OuterLine::BareOpen { .. } => match prev {
                Some((pi, pkg)) => {
                    nodes.push((
                        i,
                        Node::BareOpen {
                            pkg,
                            pending: Some(pi),
                        },
                    ));
                    owner = Some(pkg);
                }
                None => {
                    malformed += 1;
                    nodes.push((i, Node::Junk));
                }
            },
            OuterLine::Pending { pkg } => {
                malformed += dangling as usize;
                nodes.push((i, Node::Pending { pkg }));
                pending = (!pkg.is_empty()).then_some((i, pkg));
            }
            OuterLine::Junk => {
                malformed += 1;
                nodes.push((i, Node::Junk));
            }
        }
    }

    let unterminated = owner.is_some();
    malformed += (unterminated || pending.is_some()) as usize;
    Scan {
        nodes,
        malformed,
        unterminated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(String::from).collect()
    }

    fn kinds(text: &str) -> (Vec<(usize, String)>, usize, bool) {
        let ls = lines(text);
        let s = scan(&ls);
        let nodes = s
            .nodes
            .iter()
            .map(|(i, n)| (*i, format!("{n:?}")))
            .collect();
        (nodes, s.malformed, s.unterminated)
    }

    #[test]
    fn inline_comment_needs_leading_space() {
        assert_eq!(strip_comment("a=0-3 # c"), "a=0-3");
        assert_eq!(strip_comment("a=0-3 // c"), "a=0-3");
        assert_eq!(strip_comment("a#b=0"), "a#b=0");
        assert_eq!(strip_comment("http://x=1"), "http://x=1");
    }

    #[test]
    fn rule_line_with_close() {
        assert_eq!(split_rule_line("t=0-3"), Some(("t", "0-3", false)));
        assert_eq!(split_rule_line("t=0-3 }"), Some(("t", "0-3", true)));
        assert_eq!(split_rule_line("=0"), None);
        assert_eq!(split_rule_line("nothing"), None);
    }

    #[test]
    fn single_line_rule() {
        assert_eq!(
            split_single_line("pkg{RenderThread}=6-7"),
            Some(("pkg", "RenderThread", "6-7"))
        );
        assert_eq!(split_single_line("pkg{}=6"), None);
        assert_eq!(split_single_line("{t}=6"), None);
    }

    #[test]
    fn block_forms() {
        let (nodes, bad, open) = kinds("a=e-core {\n t=p-core\n}\nb {\n u=0 }\n");
        assert_eq!(bad, 0);
        assert!(!open);
        assert_eq!(nodes.len(), 5);
        assert!(nodes[0].1.starts_with("Pkg") && nodes[0].1.contains("open: true"));
        assert!(nodes[2].1.starts_with("Close"));
        assert!(nodes[3].1.starts_with("BareOpen"));
        assert!(nodes[4].1.contains("closed: true"));
    }

    #[test]
    fn brace_on_next_line() {
        let ls = lines("a\n{\n t=0\n}");
        let s = scan(&ls);
        assert_eq!(s.malformed, 0);
        assert_eq!(
            s.nodes[1],
            (
                1,
                Node::BareOpen {
                    pkg: "a",
                    pending: Some(0)
                }
            )
        );
        let ls = lines("a=\n{\n}");
        assert_eq!(
            scan(&ls).nodes[1],
            (
                1,
                Node::BareOpen {
                    pkg: "a",
                    pending: Some(0)
                }
            )
        );
    }

    #[test]
    fn malformed_counts() {
        // 悬空的 pkg 被下一条规则取代
        assert_eq!(kinds("a\nb=0\n").1, 1);
        // 孤立的 `{`
        assert_eq!(kinds("{\n").1, 1);
        // 块外的 `}`
        assert_eq!(kinds("}\n").1, 1);
        // 块内坏行
        assert_eq!(kinds("a {\n garbage\n}\n").1, 1);
        // 未闭合
        let (_, bad, open) = kinds("a {\n t=0\n");
        assert_eq!((bad, open), (1, true));
        // 文件末尾悬空 pkg
        assert_eq!(kinds("a\n").1, 1);
        assert_eq!(kinds("a=0\n# c\n\n").1, 0);
    }

    #[test]
    fn single_with_open_brace_starts_block() {
        let ls = lines("a{t}=0 {\n u=1\n}\n");
        let s = scan(&ls);
        assert_eq!(s.malformed, 0);
        assert!(matches!(s.nodes[1].1, Node::Member { pkg: "a", .. }));
    }
}
