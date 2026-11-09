//! 主循环：在 eBPF 与 /proc 两种后端之间选择与切换，并推进绑定

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::{Duration, Instant};

use crate::config::AppConfig;
use crate::ebpf_mode::{Ebpf, Fit};
use crate::proc_mode::ProcMode;
use crate::settings::Mode;
use crate::shared::{Shared, Stats};

/// eBPF 不可用时的重试间隔
const EBPF_RETRY: Duration = Duration::from_secs(30);
/// eBPF 模式下重新应用绑定的间隔（检查间隔的倍数）
const EBPF_RESYNC_TICKS: u64 = 3;
const STATS_EVERY: Duration = Duration::from_secs(2);
const EVENT_WAIT: Duration = Duration::from_secs(1);

enum Backend {
    Proc(ProcMode),
    Ebpf(Ebpf),
}

struct Daemon {
    shared: Arc<Shared>,
    backend: Backend,
    seen_gen: u64,
    ebpf_retry_at: Instant,
    resync_at: Instant,
    stats_at: Instant,
}

pub fn run(shared: Arc<Shared>) -> ! {
    shared.register_main_thread();
    let mut daemon = Daemon {
        shared,
        backend: Backend::Proc(ProcMode::default()),
        seen_gen: 0,
        // 预支一个重试间隔，使首次尝试立即进行
        ebpf_retry_at: Instant::now()
            .checked_sub(EBPF_RETRY)
            .unwrap_or_else(Instant::now),
        resync_at: Instant::now(),
        stats_at: Instant::now(),
    };
    loop {
        daemon.step();
    }
}

impl Daemon {
    fn step(&mut self) {
        let settings = self.shared.settings();
        // 先读代数再取规则集：期间若有更新，下一轮仍会看到变化
        let generation = self.shared.config_gen();
        let cfg = self.shared.config();
        let changed = std::mem::replace(&mut self.seen_gen, generation) != generation;
        let interval = settings.check_interval;

        self.select_backend(settings.mode, &cfg, changed);

        match &mut self.backend {
            Backend::Ebpf(es) => {
                let mut disconnected = false;
                match es.events.recv_timeout(EVENT_WAIT) {
                    Ok(ev) => {
                        es.handle(&ev, &cfg);
                        while let Ok(ev) = es.events.try_recv() {
                            es.handle(&ev, &cfg);
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => disconnected = true,
                }
                if self.resync_at.elapsed() >= Duration::from_secs(EBPF_RESYNC_TICKS * interval) {
                    es.resync(&cfg);
                    self.resync_at = Instant::now();
                }
                if disconnected {
                    eprintln!("eBPF: 事件通道断开，回退到 /proc 轮询");
                    self.backend = Backend::Proc(ProcMode::default());
                }
            }
            Backend::Proc(ps) => {
                if changed {
                    ps.mark_dirty();
                }
                if let Err(e) = ps.tick(&cfg, interval) {
                    eprintln!("警告: 扫描 /proc 失败 ({e})，下一轮重试");
                }
                // 配置重载与设置变更会唤醒本线程
                thread::park_timeout(Duration::from_secs(interval));
            }
        }

        self.publish_stats();
    }

    fn select_backend(&mut self, mode: Mode, cfg: &AppConfig, changed: bool) {
        match (&mut self.backend, mode) {
            (Backend::Ebpf(_), Mode::Proc) => {
                println!("工作模式切换: /proc 轮询");
                self.backend = Backend::Proc(ProcMode::default());
            }
            (Backend::Ebpf(es), _) if changed => {
                let refreshed = es.set_whitelist(cfg).and_then(|fit| match fit {
                    Fit::Ready => es
                        .full_scan(cfg)
                        .map(|()| true)
                        .map_err(|e| format!("全量扫描失败 ({e})")),
                    Fit::TooMany => Ok(false),
                });
                match refreshed {
                    Ok(true) => {}
                    // 白名单容量不足：先释放旧实例，再按新规模重建
                    Ok(false) => self.rebuild_ebpf(cfg),
                    Err(why) => {
                        eprintln!("eBPF: {why}，重建");
                        self.rebuild_ebpf(cfg);
                    }
                }
            }
            (Backend::Proc(_), Mode::Auto | Mode::Ebpf) => {
                let gave_up = self.shared.ebpf_gave_up.load(Ordering::Relaxed);
                if (mode == Mode::Ebpf || !gave_up) && self.ebpf_retry_at.elapsed() >= EBPF_RETRY {
                    self.ebpf_retry_at = Instant::now();
                    if !self.try_ebpf(cfg) && mode == Mode::Auto {
                        self.shared.ebpf_gave_up.store(true, Ordering::Relaxed);
                    }
                }
            }
            _ => {}
        }
    }

    /// 释放当前 eBPF 实例后重新启动，失败则回到 /proc 并按重试间隔再试
    fn rebuild_ebpf(&mut self, cfg: &AppConfig) {
        self.backend = Backend::Proc(ProcMode::default());
        self.ebpf_retry_at = Instant::now();
        self.try_ebpf(cfg);
    }

    fn try_ebpf(&mut self, cfg: &AppConfig) -> bool {
        match start_ebpf(cfg) {
            Ok(es) => {
                println!("工作模式切换: eBPF 事件驱动");
                self.backend = Backend::Ebpf(es);
                self.resync_at = Instant::now();
                true
            }
            Err(why) => {
                eprintln!("eBPF: {why}，使用 /proc 轮询");
                false
            }
        }
    }

    fn publish_stats(&mut self) {
        if !self.shared.web_running() || self.stats_at.elapsed() < STATS_EVERY {
            return;
        }
        self.stats_at = Instant::now();
        let ((threads, hit_pkgs), ebpf) = match &self.backend {
            Backend::Ebpf(es) => (es.stats(), true),
            Backend::Proc(ps) => (ps.stats(), false),
        };
        self.shared.publish_stats(Stats {
            threads,
            hit_pkgs,
            ebpf,
        });
    }
}

fn start_ebpf(cfg: &AppConfig) -> Result<Ebpf, String> {
    let mut es = Ebpf::start(cfg.groups().len())?;
    match es.set_whitelist(cfg)? {
        Fit::Ready => {}
        Fit::TooMany => return Err("白名单容量不足".into()),
    }
    es.full_scan(cfg)
        .map_err(|e| format!("全量扫描失败 ({e})"))?;
    Ok(es)
}
