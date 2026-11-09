//! 线程名通配，语义对齐 `fnmatch(pattern, text, FNM_NOESCAPE)`：
//! `*` 任意串，`?` 任意单字节，`[...]` 字符集（支持 `!`/`^` 取反、范围、`[:class:]`），`\` 为普通字符

pub fn matches(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0, 0);
    // 最近一个 `*` 之后的模式位置与其吞掉的文本位置
    let mut star: Option<(usize, usize)> = None;

    while ti < t.len() {
        let step = match p.get(pi) {
            Some(b'*') => {
                star = Some((pi + 1, ti));
                pi += 1;
                continue;
            }
            Some(b'?') => Some(pi + 1),
            Some(b'[') => match bracket(&p[pi..], t[ti]) {
                Some((hit, len)) => hit.then_some(pi + len),
                None => (t[ti] == b'[').then_some(pi + 1),
            },
            Some(&c) => (c == t[ti]).then_some(pi + 1),
            None => None,
        };
        match (step, star) {
            (Some(next), _) => {
                pi = next;
                ti += 1;
            }
            (None, Some((sp, st))) => {
                pi = sp;
                ti = st + 1;
                star = Some((sp, st + 1));
            }
            (None, None) => return false,
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

/// 解析 `p` 开头的 `[...]`，返回 (是否匹配 c, 括号占用字节数)；括号未闭合返回 None
fn bracket(p: &[u8], c: u8) -> Option<(bool, usize)> {
    let mut i = 1;
    let negate = matches!(p.get(i), Some(b'!' | b'^'));
    if negate {
        i += 1;
    }
    let mut hit = false;
    let mut first = true;
    loop {
        let b = *p.get(i)?;
        if b == b']' && !first {
            return Some((hit != negate, i + 1));
        }
        first = false;
        if b == b'['
            && p.get(i + 1) == Some(&b':')
            && let Some(end) = p[i + 2..].windows(2).position(|w| w == b":]")
        {
            hit |= class_has(&p[i + 2..i + 2 + end], c);
            i += end + 4;
            continue;
        }
        if p.get(i + 1) == Some(&b'-') && p.get(i + 2).is_some_and(|&e| e != b']') {
            hit |= (b..=p[i + 2]).contains(&c);
            i += 3;
        } else {
            hit |= b == c;
            i += 1;
        }
    }
}

fn class_has(name: &[u8], c: u8) -> bool {
    match name {
        b"alpha" => c.is_ascii_alphabetic(),
        b"digit" => c.is_ascii_digit(),
        b"alnum" => c.is_ascii_alphanumeric(),
        b"upper" => c.is_ascii_uppercase(),
        b"lower" => c.is_ascii_lowercase(),
        b"space" => c.is_ascii_whitespace() || c == 0x0b,
        b"punct" => c.is_ascii_punctuation(),
        b"xdigit" => c.is_ascii_hexdigit(),
        b"blank" => c == b' ' || c == b'\t',
        b"cntrl" => c.is_ascii_control(),
        b"print" => c.is_ascii_graphic() || c == b' ',
        b"graph" => c.is_ascii_graphic(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::matches;
    use std::ffi::CString;

    #[test]
    fn basics() {
        assert!(matches("RenderThread", "RenderThread"));
        assert!(!matches("RenderThread", "RenderThreads"));
        assert!(matches("Render*", "RenderThread"));
        assert!(matches("*Thread", "RenderThread"));
        assert!(matches("*", ""));
        assert!(matches("Thread-?", "Thread-1"));
        assert!(!matches("Thread-?", "Thread-12"));
        assert!(matches("Thread-[0-9]?", "Thread-12"));
        assert!(!matches("Thread-[0-9]?", "Thread-a2"));
        assert!(matches("a*b*c", "aXXbYYc"));
        assert!(!matches("a*b*c", "aXXbYY"));
    }

    #[test]
    fn brackets() {
        assert!(matches("[!a]x", "bx"));
        assert!(!matches("[!a]x", "ax"));
        assert!(matches("[^a]x", "bx"));
        assert!(matches("[]a]x", "]x"));
        assert!(matches("[a-]x", "-x"));
        assert!(matches("[[:digit:]]x", "7x"));
        assert!(!matches("[[:digit:]]x", "ax"));
        assert!(matches("[", "["));
        assert!(matches("a[b", "a[b"));
        assert!(!matches("[z-a]", "m"));
    }

    #[test]
    fn backslash_is_literal() {
        assert!(matches(r"a\*", r"a\x"));
        assert!(!matches(r"a\*", "a*"));
    }

    /// 与系统 fnmatch 对拍：良构模式的全部组合
    #[test]
    fn agrees_with_libc_fnmatch() {
        const TOKENS: &[&str] = &[
            "a",
            "b",
            "-",
            "*",
            "?",
            "[ab]",
            "[!a]",
            "[a-b]",
            "[^b]",
            "[[:digit:]]",
            "[]a]",
            "[a-]",
        ];
        let mut patterns = vec![String::new()];
        let mut layer = patterns.clone();
        for _ in 0..4 {
            layer = layer
                .iter()
                .flat_map(|p| TOKENS.iter().map(move |t| format!("{p}{t}")))
                .collect();
            patterns.extend(layer.iter().cloned());
        }
        let mut texts = vec![String::new()];
        let mut layer = texts.clone();
        for _ in 0..4 {
            layer = layer
                .iter()
                .flat_map(|s| ["a", "b", "-", "1", "]"].map(move |c| format!("{s}{c}")))
                .collect();
            texts.extend(layer.iter().cloned());
        }
        for p in &patterns {
            let cp = CString::new(p.as_str()).unwrap();
            for t in &texts {
                let ct = CString::new(t.as_str()).unwrap();
                let want =
                    unsafe { libc::fnmatch(cp.as_ptr(), ct.as_ptr(), libc::FNM_NOESCAPE) == 0 };
                assert_eq!(matches(p, t), want, "pattern={p:?} text={t:?}");
            }
        }
    }
}
