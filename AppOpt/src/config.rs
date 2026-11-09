//! 规则配置：由 applist.conf 文本解析出按包索引的规则集

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use crate::cpuset::{CpuSet, CpuTopology, CpusetFs};
use crate::grammar::{Node, scan};

pub const MAX_PKG_LEN: usize = 128;
pub const MAX_THREAD_LEN: usize = 32;

const NEW_FILE_HEADER: &str = "# 规则编写与使用说明请参考 http://AppOpt.suto.top\n\n";

/// 规则文件不存在时创建带说明头的空文件，返回是否新建
pub fn ensure_file(path: &Path) -> io::Result<bool> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut f) => f.write_all(NEW_FILE_HEADER.as_bytes()).map(|()| true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

/// 一条规则，thread 为空表示包级规则
pub struct RuleItem {
    pub thread: String,
    pub cpus: CpuSet,
}

/// 同一包名下的全部规则，保持文件中的出现顺序
pub struct PkgGroup {
    pub pkg: Arc<str>,
    pub items: Vec<RuleItem>,
    /// 全部包级规则的并集
    pkg_cpus: Option<CpuSet>,
    has_thread_rules: bool,
}

/// 线程应得的绑定
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Resolved {
    pub cpus: CpuSet,
    /// 来自线程规则（而非包级规则或兜底）
    pub thread_rule: bool,
}

impl PkgGroup {
    fn new(pkg: &str) -> Self {
        Self {
            pkg: pkg.into(),
            items: Vec::new(),
            pkg_cpus: None,
            has_thread_rules: false,
        }
    }

    fn push(&mut self, thread: &str, cpus: CpuSet) {
        if thread.is_empty() {
            self.pkg_cpus = Some(self.pkg_cpus.map_or(cpus, |acc| acc.union(&cpus)));
        } else {
            self.has_thread_rules = true;
        }
        self.items.push(RuleItem {
            thread: thread.to_string(),
            cpus,
        });
    }

    pub fn has_thread_rules(&self) -> bool {
        self.has_thread_rules
    }

    /// 线程规则按通配匹配并累加；无线程匹配时取包级并集；
    /// 仅有线程规则的包，未命中的线程恢复到全部核心；没有任何规则返回 None
    pub fn resolve(&self, thread: &str, present: &CpuSet) -> Option<Resolved> {
        if !thread.is_empty() {
            let hit = self
                .items
                .iter()
                .filter(|r| !r.thread.is_empty() && crate::glob::matches(&r.thread, thread))
                .fold(None::<CpuSet>, |acc, r| {
                    Some(acc.map_or(r.cpus, |a| a.union(&r.cpus)))
                });
            if let Some(cpus) = hit {
                return Some(Resolved {
                    cpus,
                    thread_rule: true,
                });
            }
        }
        match self.pkg_cpus {
            Some(cpus) => Some(Resolved {
                cpus,
                thread_rule: false,
            }),
            None if self.has_thread_rules => Some(Resolved {
                cpus: *present,
                thread_rule: false,
            }),
            None => None,
        }
    }
}

/// 解析后的规则集，连同其绑定的设备环境
pub struct AppConfig {
    groups: Vec<PkgGroup>,
    index: HashMap<Arc<str>, usize>,
    rule_count: usize,
    /// 被拒绝或无法解析的行数
    pub parse_fails: usize,
    /// 解析时逐行记录的问题说明
    pub issues: Vec<String>,
    pub topo: CpuTopology,
    pub cpusets: Option<Arc<CpusetFs>>,
}

fn name_error(pkg: &str, thread: &str) -> Option<&'static str> {
    let control = |s: &str| s.bytes().any(|b| b < 0x20 || b == 0x7f);
    if pkg.is_empty() || pkg.len() >= MAX_PKG_LEN {
        Some("包名为空或过长")
    } else if thread.len() >= MAX_THREAD_LEN {
        Some("线程名过长")
    } else if control(pkg) || control(thread) {
        Some("名称含控制字符")
    } else {
        None
    }
}

impl AppConfig {
    pub fn parse(text: &str, topo: CpuTopology, cpusets: Option<Arc<CpusetFs>>) -> Self {
        let lines: Vec<&str> = text.lines().collect();
        let parsed = scan(&lines);
        let mut cfg = Self {
            groups: Vec::new(),
            index: HashMap::new(),
            rule_count: 0,
            parse_fails: parsed.malformed,
            issues: Vec::new(),
            topo,
            cpusets,
        };

        for (i, node) in &parsed.nodes {
            let line = i + 1;
            let (pkg, thread, spec) = match *node {
                Node::Pkg { pkg, cpus, .. } => (pkg, "", cpus),
                Node::Single {
                    pkg, thread, cpus, ..
                }
                | Node::Member {
                    pkg, thread, cpus, ..
                } => (pkg, thread, cpus),
                Node::BadMember { .. } | Node::Junk => {
                    cfg.issues.push(format!("第 {line} 行无法解析"));
                    continue;
                }
                _ => continue,
            };
            let resolved = match name_error(pkg, thread) {
                Some(why) => Err(why.to_string()),
                None => cfg.topo.parse_spec(spec).map_err(|e| e.to_string()),
            };
            match resolved {
                Ok(cpus) => cfg.add(pkg, thread, cpus),
                Err(why) => {
                    cfg.parse_fails += 1;
                    cfg.issues.push(format!("第 {line} 行被忽略: {why}"));
                }
            }
        }
        cfg
    }

    fn add(&mut self, pkg: &str, thread: &str, cpus: CpuSet) {
        let gi = match self.index.get(pkg) {
            Some(&gi) => gi,
            None => {
                let group = PkgGroup::new(pkg);
                self.index.insert(group.pkg.clone(), self.groups.len());
                self.groups.push(group);
                self.groups.len() - 1
            }
        };
        self.groups[gi].push(thread, cpus);
        self.rule_count += 1;
    }

    pub fn groups(&self) -> &[PkgGroup] {
        &self.groups
    }

    pub fn group(&self, pkg: &str) -> Option<&PkgGroup> {
        self.index.get(pkg).map(|&gi| &self.groups[gi])
    }

    pub fn rule_count(&self) -> usize {
        self.rule_count
    }

    /// 内核 comm 最长 15 字节，长包名会被截成前缀或后缀；
    /// 恰好等于包名直接命中，否则 comm 写满 15 字节且可能是某包名的截断时，再用完整 cmdline 判定
    pub fn pkg_for_comm(
        &self,
        comm: &str,
        cmdline: impl FnOnce() -> Option<String>,
    ) -> Option<&PkgGroup> {
        if let Some(group) = self.group(comm) {
            return Some(group);
        }
        let truncated = comm.len() >= 15
            && self
                .groups
                .iter()
                .any(|g| g.pkg.starts_with(comm) || g.pkg.ends_with(comm));
        if truncated {
            cmdline().and_then(|name| self.group(&name))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpuset::Cluster;

    fn topo() -> CpuTopology {
        let set = |s| CpuSet::parse_list(s).unwrap();
        let cl = |max_freq, cpus| Cluster {
            max_freq,
            cpus: set(cpus),
            capacity: None,
        };
        CpuTopology::from_groups(set("0-7"), [cl(1, "0-3"), cl(2, "4-6"), cl(3, "7")])
    }

    fn cfg(text: &str) -> AppConfig {
        AppConfig::parse(text, topo(), None)
    }

    fn set(s: &str) -> CpuSet {
        CpuSet::parse_list(s).unwrap()
    }

    #[test]
    fn rules_group_by_package_in_file_order() {
        let c = cfg("b=e-core\na {\n RenderThread=hp-core\n}\nb{T}=0\na=p-core\n");
        let names: Vec<&str> = c.groups().iter().map(|g| &*g.pkg).collect();
        assert_eq!(names, ["b", "a"]);
        assert_eq!(c.rule_count(), 4);
        assert_eq!(c.parse_fails, 0);
        let b = c.group("b").unwrap();
        assert_eq!(b.items.len(), 2);
        assert!(b.has_thread_rules());
    }

    #[test]
    fn resolve_thread_then_package_then_fallback() {
        let c = cfg("a=e-core {\n Render*=hp-core\n Render?=p-core\n}\nonly {\n X=0\n}\nplain=1\n");
        let present = c.topo.present;
        let a = c.group("a").unwrap();
        // 线程规则累加
        let r = a.resolve("RenderT", &present).unwrap();
        assert_eq!((r.cpus, r.thread_rule), (set("4-7"), true));
        // 未命中线程回落到包级
        let r = a.resolve("other", &present).unwrap();
        assert_eq!((r.cpus, r.thread_rule), (set("0-3"), false));
        // 空线程名只看包级
        assert_eq!(a.resolve("", &present).unwrap().cpus, set("0-3"));
        // 仅线程规则的包：未命中线程恢复全部核心
        let only = c.group("only").unwrap();
        assert_eq!(only.resolve("Y", &present).unwrap().cpus, present);
        assert_eq!(only.resolve("X", &present).unwrap().cpus, set("0"));
        // 仅包级规则
        assert_eq!(
            c.group("plain")
                .unwrap()
                .resolve("Any", &present)
                .unwrap()
                .cpus,
            set("1")
        );
        assert!(c.group("absent").is_none());
    }

    #[test]
    fn duplicate_package_rules_union() {
        let c = cfg("a=0-1\na=4\n");
        assert_eq!(
            c.group("a")
                .unwrap()
                .resolve("", &c.topo.present)
                .unwrap()
                .cpus,
            set("0-1,4")
        );
    }

    #[test]
    fn bad_rules_are_counted_with_reason() {
        let c = cfg("ok=0-1\nbad=zzz\nnone=9\n=3\n");
        assert_eq!(c.rule_count(), 1);
        assert_eq!(c.parse_fails, 3);
        assert_eq!(c.issues.len(), 3);
        assert!(c.issues[0].contains("第 2 行"));
        let c = cfg(&format!("{}=0\n", "x".repeat(MAX_PKG_LEN)));
        assert_eq!((c.rule_count(), c.parse_fails), (0, 1));
        let c = cfg("a {\n a\u{7}b=0\n}\n");
        assert_eq!((c.rule_count(), c.parse_fails), (0, 1));
    }

    #[test]
    fn comm_lookup_handles_truncated_names() {
        let c = cfg("com.tencent.mobileqq=0\nsurfaceflinger=1\n");
        let full = || Some("com.tencent.mobileqq".to_string());
        assert!(c.pkg_for_comm("surfaceflinger", || None).is_some());
        // 后缀截断
        assert_eq!(
            &*c.pkg_for_comm("encent.mobileqq", full).unwrap().pkg,
            "com.tencent.mobileqq"
        );
        // 前缀截断
        assert!(c.pkg_for_comm("com.tencent.mob", full).is_some());
        // 短名不读取 cmdline
        assert!(
            c.pkg_for_comm("nothing", || panic!("不应读取 cmdline"))
                .is_none()
        );
        // cmdline 不在规则中
        assert!(
            c.pkg_for_comm("encent.mobileqq", || Some("other".into()))
                .is_none()
        );
    }

    #[test]
    fn shipped_sample_config_loads_cleanly() {
        let text = include_str!("../../module/applist.conf");
        let c = cfg(text);
        assert_eq!(c.parse_fails, 0, "{:?}", c.issues);
        assert!(c.group("com.tencent.mm").unwrap().has_thread_rules());
        assert!(c.group("surfaceflinger").is_some());
    }
}
