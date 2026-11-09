//! /proc 轮询模式：以线程总数变化与已跟踪进程的存活作为重扫信号

use std::collections::HashSet;
use std::io;
use std::time::{Duration, Instant};

use crate::cache::ProcCache;
use crate::config::AppConfig;
use crate::procfs;

/// 线程总数一次性增长超过该值视为有新应用启动，触发重扫
const THREAD_BURST: u16 = 11;
/// 绑定重新应用的间隔（检查间隔的倍数）
const RESYNC_TICKS: u64 = 5;
/// 全量扫描 /proc 的最长间隔（检查间隔的倍数）。线程数增长未达阈值的小进程只能靠它被发现
const FULL_SCAN_TICKS: u64 = 5;

pub struct ProcMode {
    cache: ProcCache,
    last_threads: u16,
    tracked: HashSet<i32>,
    last_total: usize,
    /// 下一次重扫读取全部进程，否则只读已跟踪进程
    scan_all: bool,
    resync_now: bool,
    resync_at: Instant,
    full_scan_at: Instant,
}

impl Default for ProcMode {
    fn default() -> Self {
        Self {
            cache: ProcCache::default(),
            last_threads: 0,
            tracked: HashSet::new(),
            last_total: 0,
            scan_all: true,
            resync_now: true,
            resync_at: Instant::now(),
            full_scan_at: Instant::now(),
        }
    }
}

impl ProcMode {
    pub fn stats(&self) -> (usize, usize) {
        self.cache.stats()
    }

    /// 配置变更后下一轮全量重扫
    pub fn mark_dirty(&mut self) {
        self.scan_all = true;
        self.last_threads = 0;
    }

    /// 一轮：按需重建缓存，并按间隔重新应用绑定
    pub fn tick(&mut self, cfg: &AppConfig, interval: u64) -> io::Result<()> {
        if self.full_scan_at.elapsed() >= Duration::from_secs(FULL_SCAN_TICKS * interval) {
            self.scan_all = true;
        }
        let mut rescan = self.scan_all;
        let threads = procfs::thread_count()?;
        if threads > self.last_threads.saturating_add(THREAD_BURST) {
            rescan = true;
        } else if threads > self.last_threads {
            self.resync_now = true;
        }
        self.last_threads = threads;

        // SAFETY: kill(pid, 0) 只探测进程是否存在
        if !rescan
            && self
                .tracked
                .iter()
                .any(|&pid| unsafe { libc::kill(pid, 0) } != 0)
        {
            rescan = true;
        }
        if rescan {
            self.rescan(cfg)?;
        }

        if self.resync_now
            || self.resync_at.elapsed() >= Duration::from_secs(RESYNC_TICKS * interval)
        {
            self.cache.resync(cfg);
            self.resync_at = Instant::now();
            self.resync_now = false;
        }
        Ok(())
    }

    fn rescan(&mut self, cfg: &AppConfig) -> io::Result<()> {
        let (scan_all, tracked) = (self.scan_all, &self.tracked);
        let walk = procfs::walk(cfg, |pid| scan_all || tracked.contains(&pid))?;

        self.cache.clear();
        for (pid, group, tid, comm) in walk.threads() {
            self.cache.fill(cfg, group, tid, pid, &comm);
        }

        if scan_all {
            self.full_scan_at = Instant::now();
        }
        self.scan_all = walk.total > self.last_total;
        self.last_total = walk.total;
        self.tracked = self.cache.pids();
        self.resync_now = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpuset::{CpuSet, CpuTopology};
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    /// 新启动、线程数只增加一个的进程不会触发线程数突增，必须靠周期性全量扫描被发现
    #[test]
    fn small_new_process_is_found_by_periodic_scan() {
        let cpu0 = CpuSet::parse_list("0").unwrap();
        let topo = CpuTopology::from_groups(cpu0, []);
        let cfg = AppConfig::parse("appopt.test.sleeper=0\n", topo, None);

        let mut mode = ProcMode::default();
        // 让首轮扫描后的"进程数增长则再扫一次"标记回落，之后只剩周期性扫描这一条发现途径
        for _ in 0..20 {
            mode.tick(&cfg, 1).unwrap();
            if !mode.scan_all {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(mode.stats().0, 0);

        let mut child = Command::new("sleep")
            .arg0("appopt.test.sleeper")
            .arg("30")
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(FULL_SCAN_TICKS + 4);
        let mut found = false;
        while Instant::now() < deadline && !found {
            std::thread::sleep(Duration::from_millis(200));
            mode.tick(&cfg, 1).unwrap();
            found = mode.stats() == (1, 1);
        }
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(found, "周期性全量扫描应发现新进程");
    }
}
