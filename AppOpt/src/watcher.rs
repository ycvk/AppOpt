//! 规则文件监听：inotify 监视其所在目录以即时感知变更，并按检查间隔兜底比对文件指纹

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::shared::Shared;

struct Inotify {
    fd: OwnedFd,
    wd: Option<i32>,
}

impl Inotify {
    fn new() -> io::Result<Self> {
        // SAFETY: inotify_init1 返回新建 fd 的所有权
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            wd: None,
        })
    }

    /// 改为监视 dir；目录内文件的新建、改名覆盖、写完关闭与删除都会唤醒
    fn watch(&mut self, dir: &Path) -> io::Result<()> {
        if let Some(wd) = self.wd.take() {
            // SAFETY: wd 由本 fd 的 inotify_add_watch 返回
            unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), wd as _) };
        }
        let path = CString::new(dir.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "路径含 NUL"))?;
        let mask = libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_CREATE | libc::IN_DELETE;
        // SAFETY: path 是有效的 C 字符串
        let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), path.as_ptr(), mask) };
        if wd < 0 {
            return Err(io::Error::last_os_error());
        }
        self.wd = Some(wd);
        Ok(())
    }

    /// 等待事件至多 timeout，并取走已排队的事件
    fn wait(&self, timeout: Duration) -> io::Result<()> {
        let mut pfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: pfd 在调用期间有效
        let n = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as libc::c_int) };
        if n < 0 {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(libc::EINTR) {
                Ok(())
            } else {
                Err(e)
            };
        }
        if n == 0 {
            return Ok(());
        }
        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: buf 可写且长度正确；事件内容无需解析
            let len =
                unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if len < 0 {
                let e = io::Error::last_os_error();
                return if e.raw_os_error() == Some(libc::EAGAIN) {
                    Ok(())
                } else {
                    Err(e)
                };
            }
        }
    }
}

fn watch_dir(config_file: &str) -> PathBuf {
    match Path::new(config_file).parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

pub fn spawn(shared: Arc<Shared>) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("ConfigWatcher".into())
        .spawn(move || run(&shared))
}

fn run(shared: &Shared) {
    let mut inotify = match Inotify::new() {
        Ok(i) => Some(i),
        Err(e) => {
            eprintln!("inotify 初始化失败 ({e})，改为按检查间隔轮询规则文件");
            None
        }
    };
    let mut watched: Option<PathBuf> = None;
    let mut last_err = String::new();

    loop {
        let settings = shared.settings();
        let interval = Duration::from_secs(settings.check_interval);

        if let Some(ino) = inotify.as_mut() {
            let dir = watch_dir(&settings.config_file);
            if watched.as_ref() != Some(&dir) {
                match ino.watch(&dir) {
                    Ok(()) => {
                        println!("监控规则文件目录: {}", dir.display());
                        watched = Some(dir);
                    }
                    Err(e) => {
                        eprintln!(
                            "监控 {} 失败 ({e})，改为按检查间隔轮询规则文件",
                            dir.display()
                        );
                        inotify = None;
                    }
                }
            }
        }

        let waited = match &inotify {
            Some(ino) => ino.wait(interval),
            None => {
                thread::sleep(interval);
                Ok(())
            }
        };
        if let Err(e) = waited {
            eprintln!("inotify 读取失败 ({e})，改为按检查间隔轮询规则文件");
            inotify = None;
        }

        // 无论是否收到事件都比对指纹：事件只是缩短延迟，不是唯一来源
        match shared.reload_config(false) {
            Ok(_) => last_err.clear(),
            Err(e) if e.to_string() != last_err => {
                eprintln!("重载规则文件失败 ({e})，保持当前规则");
                last_err = e.to_string();
            }
            Err(_) => {}
        }
    }
}
