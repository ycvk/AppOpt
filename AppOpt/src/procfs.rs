//! /proc 读取：进程名、线程名、线程列表与目标进程扫描

use std::fs;
use std::io;
use std::os::unix::fs::FileExt;

use crate::config::{AppConfig, MAX_PKG_LEN, MAX_THREAD_LEN, PkgGroup};

/// 读取文件开头至多 buf.len() 字节，空文件视为无内容
fn read_prefix(path: &str, buf: &mut [u8]) -> Option<usize> {
    let n = fs::File::open(path).ok()?.read_at(buf, 0).ok()?;
    (n > 0).then_some(n)
}

/// 进程名：cmdline 首段去掉路径前缀，Android 应用即包名（含 `:进程` 后缀）
pub fn cmdline(pid: i32) -> Option<String> {
    let mut buf = [0u8; MAX_PKG_LEN];
    let n = read_prefix(&format!("/proc/{pid}/cmdline"), &mut buf)?;
    let argv0 = std::str::from_utf8(buf[..n].split(|&b| b == 0).next()?).ok()?;
    let name = argv0.rsplit('/').next()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// 线程名（comm）
pub fn comm(tid: i32) -> Option<String> {
    let mut buf = [0u8; MAX_THREAD_LEN];
    let n = read_prefix(&format!("/proc/{tid}/comm"), &mut buf)?;
    let name = std::str::from_utf8(buf[..n].split(|&b| b == 0 || b == b'\n').next()?)
        .ok()?
        .trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// 进程的全部线程 id，进程已退出时返回 None
pub fn tids(pid: i32) -> Option<Vec<i32>> {
    let dir = fs::read_dir(format!("/proc/{pid}/task")).ok()?;
    Some(
        dir.flatten()
            .filter_map(|e| e.file_name().to_string_lossy().parse().ok())
            .collect(),
    )
}

/// /proc 下的全部进程 id
pub fn pids() -> io::Result<impl Iterator<Item = i32>> {
    Ok(fs::read_dir("/proc")?
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse().ok()))
}

/// 系统当前线程总数（sysinfo.procs），用作进程变动的廉价信号
pub fn thread_count() -> io::Result<u16> {
    // SAFETY: sysinfo 只写入传入的结构体
    let mut info: libc::sysinfo = unsafe { std::mem::zeroed() };
    if unsafe { libc::sysinfo(&mut info) } == 0 {
        Ok(info.procs)
    } else {
        Err(io::Error::last_os_error())
    }
}

pub struct Walk<'a> {
    /// 命中规则的进程
    pub hits: Vec<(i32, &'a PkgGroup)>,
    /// /proc 中进程总数
    pub total: usize,
}

impl<'a> Walk<'a> {
    /// 命中进程的全部线程 (pid, group, tid, comm)；只有带线程规则的包才读取线程名
    pub fn threads(&self) -> impl Iterator<Item = (i32, &'a PkgGroup, i32, String)> + '_ {
        self.hits.iter().flat_map(|&(pid, group)| {
            tids(pid).into_iter().flatten().map(move |tid| {
                let name = if group.has_thread_rules() {
                    comm(tid).unwrap_or_default()
                } else {
                    String::new()
                };
                (pid, group, tid, name)
            })
        })
    }
}

/// 遍历 /proc，对 want 为真的进程读取进程名并匹配规则
pub fn walk<'a>(cfg: &'a AppConfig, want: impl Fn(i32) -> bool) -> io::Result<Walk<'a>> {
    let mut hits = Vec::new();
    let mut total = 0;
    for pid in pids()? {
        total += 1;
        if !want(pid) {
            continue;
        }
        // 内核线程没有 cmdline，退回线程名
        let Some(name) = cmdline(pid).or_else(|| comm(pid)) else {
            continue;
        };
        if let Some(group) = cfg.group(&name) {
            hits.push((pid, group));
        }
    }
    Ok(Walk { hits, total })
}
