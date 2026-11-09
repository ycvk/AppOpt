//! 受管线程缓存，eBPF 与 /proc 两种模式共用

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::config::{AppConfig, PkgGroup, Resolved};
use crate::cpuset::{Bind, CpuSet, bind_thread};

pub struct TaskEntry {
    pub pid: i32,
    pub pkg: Arc<str>,
    pub cpus: CpuSet,
    /// 来自线程规则，之后的包级结果不再覆盖它
    pub thread_rule: bool,
    /// 上次绑定失败（已告警），之后成功时复位
    failed: bool,
}

pub enum Plan {
    NoRule,
    /// 线程已由线程规则绑定，保持不变
    Keep,
    Bind(Resolved),
}

/// 一次绑定的结果
#[derive(PartialEq, Eq, Debug)]
pub enum Outcome {
    NoRule,
    Kept,
    Bound,
    /// 线程已退出
    Gone,
}

#[derive(Default)]
pub struct ProcCache {
    tasks: HashMap<i32, TaskEntry>,
}

impl ProcCache {
    pub fn clear(&mut self) {
        self.tasks.clear();
    }

    pub fn remove(&mut self, tid: i32) {
        self.tasks.remove(&tid);
    }

    /// 进程主线程的包名，用于线程改名后仍能归属到包
    pub fn pkg_of(&self, pid: i32) -> Option<&str> {
        self.tasks.get(&pid).map(|e| &*e.pkg)
    }

    pub fn pids(&self) -> HashSet<i32> {
        self.tasks.values().map(|e| e.pid).collect()
    }

    /// (受管线程数, 命中的包数)
    pub fn stats(&self) -> (usize, usize) {
        let pkgs: HashSet<&str> = self.tasks.values().map(|e| &*e.pkg).collect();
        (self.tasks.len(), pkgs.len())
    }

    fn plan(&self, cfg: &AppConfig, group: &PkgGroup, tid: i32, comm: &str) -> Plan {
        let thread = if group.has_thread_rules() { comm } else { "" };
        match group.resolve(thread, &cfg.topo.present) {
            None => Plan::NoRule,
            Some(r)
                if !r.thread_rule && self.tasks.get(&tid).is_some_and(|old| old.thread_rule) =>
            {
                Plan::Keep
            }
            Some(r) => Plan::Bind(r),
        }
    }

    fn record(&mut self, group: &PkgGroup, tid: i32, pid: i32, r: Resolved, failed: bool) {
        self.tasks.insert(
            tid,
            TaskEntry {
                pid,
                pkg: group.pkg.clone(),
                cpus: r.cpus,
                thread_rule: r.thread_rule,
                failed,
            },
        );
    }

    /// 只登记不绑定，/proc 全量重建后由 resync 统一应用
    pub fn fill(&mut self, cfg: &AppConfig, group: &PkgGroup, tid: i32, pid: i32, comm: &str) {
        if let Plan::Bind(r) = self.plan(cfg, group, tid, comm) {
            self.record(group, tid, pid, r, false);
        }
    }

    /// 计算并立即绑定；sticky 为假时结果不当作线程规则保留（名字继承自父线程，改名后要按新名字重算）
    pub fn apply(
        &mut self,
        cfg: &AppConfig,
        group: &PkgGroup,
        tid: i32,
        pid: i32,
        comm: &str,
        sticky: bool,
    ) -> Outcome {
        let mut r = match self.plan(cfg, group, tid, comm) {
            Plan::NoRule => return Outcome::NoRule,
            Plan::Keep => return Outcome::Kept,
            Plan::Bind(r) => r,
        };
        r.thread_rule &= sticky;
        match bind_thread(tid, &r.cpus, cfg.cpusets.as_deref()) {
            Bind::Gone => {
                self.tasks.remove(&tid);
                Outcome::Gone
            }
            Bind::Done => {
                self.record(group, tid, pid, r, false);
                Outcome::Bound
            }
            Bind::Failed(e) => {
                eprintln!(
                    "警告: 线程 {tid} 绑定到 {} 失败 ({e})，包 {}",
                    r.cpus, group.pkg
                );
                self.record(group, tid, pid, r, true);
                Outcome::Bound
            }
        }
    }

    /// 重新应用全部受管线程，返回已退出的线程
    pub fn resync(&mut self, cfg: &AppConfig) -> Vec<i32> {
        let mut gone = Vec::new();
        for (&tid, e) in self.tasks.iter_mut() {
            match bind_thread(tid, &e.cpus, cfg.cpusets.as_deref()) {
                Bind::Done => e.failed = false,
                Bind::Gone => gone.push(tid),
                Bind::Failed(err) => {
                    if !e.failed {
                        eprintln!(
                            "警告: 线程 {tid} 绑定到 {} 失败 ({err})，包 {}，之后每轮重试",
                            e.cpus, e.pkg
                        );
                    }
                    e.failed = true;
                }
            }
        }
        for tid in &gone {
            self.tasks.remove(tid);
        }
        gone
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpuset::CpuTopology;
    use std::sync::mpsc;
    use std::thread;

    /// 起一个空闲线程作为绑定对象，返回其 tid；线程在 done 被丢弃后退出
    fn idle_thread() -> (i32, mpsc::Sender<()>) {
        let (tid_tx, tid_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        thread::spawn(move || {
            // SAFETY: gettid 无参数，只返回调用线程的 id
            tid_tx.send(unsafe { libc::gettid() }).unwrap();
            let _ = done_rx.recv();
        });
        (tid_rx.recv().unwrap(), done_tx)
    }

    #[test]
    fn inherited_name_does_not_pin_thread_rule() {
        let present = CpuSet::parse_list("0-1").unwrap();
        // 包级规则 0，名为 a 的线程（即主线程名）走线程规则 1
        let cfg = AppConfig::parse(
            "a=0 {\n\ta=1\n}\n",
            CpuTopology::from_groups(present, []),
            None,
        );
        let group = cfg.group("a").unwrap();
        let pid = std::process::id() as i32;
        let cpus = |s| CpuSet::parse_list(s).unwrap();

        // 新线程沿用父线程名 a：先按线程规则绑定，改名后按新名字落到包级规则
        let (tid, _done) = idle_thread();
        let mut cache = ProcCache::default();
        assert_eq!(
            cache.apply(&cfg, group, tid, pid, "a", false),
            Outcome::Bound
        );
        assert_eq!(CpuSet::of_thread(tid).unwrap(), cpus("1"));
        assert_eq!(
            cache.apply(&cfg, group, tid, pid, "Worker", true),
            Outcome::Bound
        );
        assert_eq!(CpuSet::of_thread(tid).unwrap(), cpus("0"));

        // 真实名字命中的线程规则仍然保留
        let (tid, _done) = idle_thread();
        assert_eq!(
            cache.apply(&cfg, group, tid, pid, "a", true),
            Outcome::Bound
        );
        assert_eq!(
            cache.apply(&cfg, group, tid, pid, "Worker", true),
            Outcome::Kept
        );
        assert_eq!(CpuSet::of_thread(tid).unwrap(), cpus("1"));
    }
}
