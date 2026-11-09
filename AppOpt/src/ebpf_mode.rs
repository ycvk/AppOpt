//! eBPF 事件驱动模式：内核态上报命中白名单的 fork/exec/rename/exit，用户态增量绑定

use std::collections::HashMap;
use std::fmt::Display;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use appopt_common::{
    COMM_KEY_LEN, COMM_LEN, EVENT_EXEC, EVENT_EXIT, EVENT_RENAME, ProcEvent, TracepointOffsets,
};
use aya::maps::{Array, HashMap as BpfHashMap, MapData, RingBuf};
use aya::programs::TracePoint;
use aya::{Ebpf as Bpf, EbpfLoader};

use crate::cache::{Outcome, ProcCache};
use crate::config::AppConfig;
use crate::procfs;

const BTF_PATH: &str = "/sys/kernel/btf/vmlinux";
const OBJECT_NAME: &str = "AppOpt-ebpf";

/// 内核是否提供 BTF（eBPF 模式的前提）
pub fn available() -> bool {
    Path::new(BTF_PATH).exists()
}

/// 白名单键：每个包名取前 15 字节，长于 15 字节时再取后 15 字节，与内核 comm 的截断方式对应
fn comm_keys<'a>(pkgs: impl Iterator<Item = &'a str>) -> Vec<[u8; COMM_KEY_LEN]> {
    let mut keys = Vec::new();
    for pkg in pkgs.map(str::as_bytes).filter(|b| !b.is_empty()) {
        let mut head = [0u8; COMM_KEY_LEN];
        let n = pkg.len().min(COMM_KEY_LEN);
        head[..n].copy_from_slice(&pkg[..n]);
        keys.push(head);
        if pkg.len() > COMM_KEY_LEN {
            let mut tail = [0u8; COMM_KEY_LEN];
            tail.copy_from_slice(&pkg[pkg.len() - COMM_KEY_LEN..]);
            keys.push(tail);
        }
    }
    keys.sort_unstable();
    keys.dedup();
    keys
}

fn comm_str(comm: &[u8; COMM_LEN]) -> &str {
    let end = comm.iter().position(|&b| b == 0).unwrap_or(COMM_LEN);
    std::str::from_utf8(&comm[..end]).unwrap_or("").trim()
}

/// 解析 tracefs format 文件，得到字段名到偏移的映射（字段名去掉数组下标）
fn parse_format(content: &str) -> HashMap<&str, u32> {
    content
        .lines()
        .filter_map(|line| line.trim().strip_prefix("field:"))
        .filter_map(|rest| {
            let mut parts = rest.split(';').map(str::trim);
            let name = parts.next()?.split_whitespace().last()?.split('[').next()?;
            let offset = parts
                .find_map(|p| p.strip_prefix("offset:"))?
                .trim()
                .parse()
                .ok()?;
            (!name.is_empty()).then_some((name, offset))
        })
        .collect()
}

fn ctx<T, E: Display>(r: Result<T, E>, what: &str) -> Result<T, String> {
    r.map_err(|e| format!("{what} ({e})"))
}

fn tracefs_root() -> Result<&'static str, String> {
    ["/sys/kernel/tracing", "/sys/kernel/debug/tracing"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .ok_or_else(|| "tracefs 不可用".to_string())
}

/// 读取本机 tracepoint 字段偏移，不同内核版本布局不同，不能硬编码
fn read_offsets() -> Result<TracepointOffsets, String> {
    let root = tracefs_root()?;
    let read = |category: &str, name: &str| {
        ctx(
            fs::read_to_string(format!("{root}/events/{category}/{name}/format")),
            &format!("读取 {name} format"),
        )
    };
    let fork = read("sched", "sched_process_fork")?;
    let rename = read("task", "task_rename")?;
    let (fork, rename) = (parse_format(&fork), parse_format(&rename));
    let field = |m: &HashMap<&str, u32>, name: &str| {
        m.get(name)
            .copied()
            .ok_or_else(|| format!("tracepoint format 缺少字段 {name}"))
    };
    let offsets = TracepointOffsets {
        fork_child_pid: field(&fork, "child_pid")?,
        fork_child_comm: field(&fork, "child_comm")?,
        rename_newcomm: field(&rename, "newcomm")?,
    };
    println!(
        "eBPF: tracepoint 偏移 [{root}] fork[child_pid={}, child_comm={}] rename[newcomm={}]",
        offsets.fork_child_pid, offsets.fork_child_comm, offsets.rename_newcomm
    );
    Ok(offsets)
}

fn attach(bpf: &mut Bpf, category: &str, name: &str) -> Result<(), String> {
    let prog = bpf
        .program_mut(name)
        .ok_or_else(|| format!("未找到 {name} 程序"))?;
    let tp: &mut TracePoint = ctx(prog.try_into(), &format!("{name} 类型转换"))?;
    ctx(tp.load(), &format!("{name} 加载"))?;
    ctx(tp.attach(category, name), &format!("{name} 附加"))?;
    Ok(())
}

/// 白名单写入结果
#[derive(Debug, PartialEq, Eq)]
pub enum Fit {
    Ready,
    /// 键数量超出内核 map 容量，需按新规模重建
    TooMany,
}

pub struct Ebpf {
    bpf: Bpf,
    pub events: mpsc::Receiver<ProcEvent>,
    cache: ProcCache,
    capacity: u32,
    /// 关闭写端即唤醒读取线程使其退出
    stop: Option<UnixStream>,
    reader: Option<JoinHandle<()>>,
}

impl Drop for Ebpf {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Ebpf {
    /// 加载与 AppOpt 同目录的内核程序并挂载 tracepoint，白名单容量按包数量留足
    pub fn start(pkg_count: usize) -> Result<Self, String> {
        if !available() {
            return Err("内核不支持（缺少 BTF）".into());
        }
        let exe = ctx(std::env::current_exe(), "定位可执行文件")?;
        let path = exe
            .parent()
            .ok_or("可执行文件没有所在目录")?
            .join(OBJECT_NAME);
        let object = ctx(fs::read(&path), &format!("读取 {}", path.display()))?;
        if object.is_empty() {
            return Err(format!("{} 为空", path.display()));
        }
        println!(
            "eBPF: 从 {} 加载程序 ({} bytes)",
            path.display(),
            object.len()
        );

        let capacity = (pkg_count * 2).max(512).next_power_of_two() as u32;
        let mut bpf = ctx(
            EbpfLoader::new()
                .map_max_entries("TARGET_COMM_MAP", capacity)
                .load(&object),
            "加载",
        )?;

        // 先注入偏移再 attach，避免首批事件读到空 map
        let offsets = read_offsets()?;
        let map = bpf.map_mut("OFFSETS_MAP").ok_or("未找到 OFFSETS_MAP")?;
        let mut offsets_map: Array<_, TracepointOffsets> =
            ctx(map.try_into(), "OFFSETS_MAP 类型转换")?;
        ctx(offsets_map.set(0, &offsets, 0), "OFFSETS_MAP 注入")?;

        for (category, name) in [
            ("sched", "sched_process_fork"),
            ("sched", "sched_process_exec"),
            ("sched", "sched_process_exit"),
            ("task", "task_rename"),
        ] {
            attach(&mut bpf, category, name)?;
        }

        let events_map = bpf.take_map("EVENTS").ok_or("未找到 EVENTS map")?;
        let ring = ctx(RingBuf::try_from(events_map), "EVENTS 类型转换")?;
        let (tx, rx) = mpsc::channel();
        let (stop_tx, stop_rx) = ctx(UnixStream::pair(), "创建唤醒通道")?;
        let reader = ctx(
            thread::Builder::new()
                .name("EbpfReader".into())
                .spawn(move || read_ring(ring, tx, stop_rx)),
            "启动读取线程",
        )?;

        let mut this = Self {
            bpf,
            events: rx,
            cache: ProcCache::default(),
            capacity,
            stop: Some(stop_tx),
            reader: Some(reader),
        };
        // 校验后续操作依赖的 map 存在且类型匹配
        this.targets()?;
        this.applied()?;
        println!("eBPF: 初始化成功");
        Ok(this)
    }

    fn targets(&mut self) -> Result<BpfHashMap<&mut MapData, [u8; COMM_KEY_LEN], u8>, String> {
        let map = self
            .bpf
            .map_mut("TARGET_COMM_MAP")
            .ok_or("未找到 TARGET_COMM_MAP")?;
        ctx(map.try_into(), "TARGET_COMM_MAP 类型转换")
    }

    fn applied(&mut self) -> Result<BpfHashMap<&mut MapData, u32, u8>, String> {
        let map = self
            .bpf
            .map_mut("APPLIED_MAP")
            .ok_or("未找到 APPLIED_MAP")?;
        ctx(map.try_into(), "APPLIED_MAP 类型转换")
    }

    fn applied_set(&mut self, tid: i32) {
        let mut map = self.applied().expect("APPLIED_MAP 已在启动时校验");
        if let Err(e) = map.insert(&(tid as u32), &1u8, 0) {
            eprintln!("eBPF: APPLIED_MAP 插入失败 tid={tid} ({e})");
        }
    }

    fn applied_del(&mut self, tid: i32) {
        // 键不存在属正常情况（线程从未登记或已被淘汰）
        let _ = self
            .applied()
            .expect("APPLIED_MAP 已在启动时校验")
            .remove(&(tid as u32));
    }

    fn applied_clear(&mut self) {
        let mut map = self.applied().expect("APPLIED_MAP 已在启动时校验");
        let keys: Vec<u32> = map.keys().flatten().collect();
        for k in keys {
            let _ = map.remove(&k);
        }
    }

    /// 以配置中的包名重写内核白名单
    pub fn set_whitelist(&mut self, cfg: &AppConfig) -> Result<Fit, String> {
        let keys = comm_keys(cfg.groups().iter().map(|g| &*g.pkg));
        if keys.len() > self.capacity as usize {
            return Ok(Fit::TooMany);
        }
        let capacity = self.capacity;
        let mut map = self.targets()?;
        let old: Vec<_> = map.keys().flatten().collect();
        for k in &old {
            let _ = map.remove(k);
        }
        for k in &keys {
            ctx(map.insert(k, &1u8, 0), "白名单插入")?;
        }
        println!(
            "eBPF: 白名单已配置，{} 个包名 > {} 条匹配键（容量 {capacity}）",
            cfg.groups().len(),
            keys.len()
        );
        Ok(Fit::Ready)
    }

    pub fn stats(&self) -> (usize, usize) {
        self.cache.stats()
    }

    /// 启动或配置变更后全量扫描 /proc 并绑定
    pub fn full_scan(&mut self, cfg: &AppConfig) -> std::io::Result<()> {
        self.cache.clear();
        self.applied_clear();
        let walk = procfs::walk(cfg, |_| true)?;
        for &(pid, group) in &walk.hits {
            let Some(tids) = procfs::tids(pid) else {
                continue;
            };
            for tid in tids {
                let comm = if group.has_thread_rules() {
                    procfs::comm(tid).unwrap_or_default()
                } else {
                    String::new()
                };
                match self.cache.apply(cfg, group, tid, pid, &comm) {
                    Outcome::Bound => self.applied_set(tid),
                    Outcome::Gone => self.applied_del(tid),
                    Outcome::Kept | Outcome::NoRule => {}
                }
            }
        }
        Ok(())
    }

    /// 周期性重新应用绑定并清理已退出线程
    pub fn resync(&mut self, cfg: &AppConfig) {
        for tid in self.cache.resync(cfg) {
            self.applied_del(tid);
        }
    }

    /// 处理一条内核事件
    pub fn handle(&mut self, ev: &ProcEvent, cfg: &AppConfig) {
        let (tid, comm) = (ev.tid, comm_str(&ev.comm));
        match ev.event_type {
            EVENT_EXIT => {
                self.cache.remove(tid);
                self.applied_del(tid);
            }
            EVENT_EXEC => {
                if !matches!(
                    self.bind(ev.pid, tid, comm, cfg),
                    Outcome::Bound | Outcome::Kept
                ) {
                    self.cache.remove(tid);
                    self.applied_del(tid);
                }
            }
            EVENT_RENAME => {
                self.bind(ev.pid, tid, comm, cfg);
            }
            // FORK：子线程继承父线程的亲和性与 cpuset，内核态已占位，改名时才做完整处理
            _ => {}
        }
    }

    /// comm 匹配包名，线程改名后回退到该进程主线程登记的包名
    fn bind(&mut self, pid: i32, tid: i32, comm: &str, cfg: &AppConfig) -> Outcome {
        let group = cfg
            .pkg_for_comm(comm, || procfs::cmdline(pid))
            .or_else(|| self.cache.pkg_of(pid).and_then(|pkg| cfg.group(pkg)));
        let Some(group) = group else {
            return Outcome::NoRule;
        };
        let outcome = self.cache.apply(cfg, group, tid, pid, comm);
        match outcome {
            Outcome::Bound => self.applied_set(tid),
            Outcome::Gone => self.applied_del(tid),
            Outcome::Kept | Outcome::NoRule => {}
        }
        outcome
    }
}

/// RingBuf 读取线程：poll 等待事件，stop 写端关闭时退出；通道断开表示主线程已丢弃本实例
fn read_ring(mut ring: RingBuf<MapData>, tx: mpsc::Sender<ProcEvent>, stop: UnixStream) {
    let mut fds = [
        libc::pollfd {
            fd: ring.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: stop.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        // SAFETY: fds 在调用期间有效
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            eprintln!("eBPF: poll 失败 ({e})，读取线程退出");
            return;
        }
        if fds[1].revents != 0 {
            return;
        }
        while let Some(item) = ring.next() {
            if let Some(ev) = ProcEvent::from_bytes(&item)
                && tx.send(ev).is_err()
            {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitelist_keys_cover_prefix_and_suffix() {
        let keys = comm_keys(["com.tencent.mobileqq", "surfaceflinger", "short", ""].into_iter());
        let key = |s: &str| {
            let mut k = [0u8; COMM_KEY_LEN];
            k[..s.len()].copy_from_slice(s.as_bytes());
            k
        };
        assert!(keys.contains(&key("com.tencent.mob")));
        assert!(keys.contains(&key("encent.mobileqq")));
        assert!(keys.contains(&key("surfaceflinger")[..].try_into().unwrap()));
        assert!(keys.contains(&key("short")));
        assert_eq!(keys.len(), 4);
    }

    #[test]
    fn comm_is_cut_at_nul() {
        let mut raw = [0u8; COMM_LEN];
        raw[..5].copy_from_slice(b"abc d");
        assert_eq!(comm_str(&raw), "abc d");
        assert_eq!(comm_str(&[b'x'; COMM_LEN]), "x".repeat(COMM_LEN));
    }

    #[test]
    fn tracepoint_format_offsets() {
        let format = "name: task_rename\nID: 1\nformat:\n\
            \tfield:unsigned short common_type;\toffset:0;\tsize:2;\tsigned:0;\n\
            \tfield:pid_t pid;\toffset:8;\tsize:4;\tsigned:1;\n\
            \tfield:char oldcomm[16];\toffset:12;\tsize:16;\tsigned:0;\n\
            \tfield:char newcomm[16];\toffset:28;\tsize:16;\tsigned:0;\n\
            \tfield:__data_loc char[] name;\toffset:44;\tsize:4;\tsigned:0;\n\
            \nprint fmt: \"x\"\n";
        let m = parse_format(format);
        assert_eq!(m["newcomm"], 28);
        assert_eq!(m["oldcomm"], 12);
        assert_eq!(m["pid"], 8);
        assert_eq!(m["name"], 44);
    }
}
