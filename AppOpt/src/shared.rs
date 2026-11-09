//! 跨线程共享的运行状态：设置、当前规则集、统计，以及规则文件的重载与编辑

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread::{self, Thread};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::{AppConfig, ensure_file};
use crate::cpuset::{CPUSET_ROOT, CpuTopology, CpusetFs};
use crate::fsutil::write_atomic;
use crate::rule_edit::EditError;
use crate::settings::{Mode, Settings};

/// 主循环发布的运行统计
#[derive(Clone, Copy, Default)]
pub struct Stats {
    pub threads: usize,
    pub hit_pkgs: usize,
    pub ebpf: bool,
}

/// 文件内容变更的判据：mtime、大小与 inode（rename 替换会换 inode）
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    mtime: (i64, i64),
    size: u64,
    ino: u64,
}

/// 修改时间距今不足该值时，文件系统的时间戳粒度可能让两次不同内容共用同一指纹
const RACY_WINDOW: Duration = Duration::from_secs(2);

impl FileStamp {
    fn of(path: &Path) -> io::Result<Self> {
        let m = fs::metadata(path)?;
        Ok(Self {
            mtime: (m.mtime(), m.mtime_nsec()),
            size: m.size(),
            ino: m.ino(),
        })
    }

    /// 指纹不足以证明内容未变：修改时间太新，或在未来
    fn is_racy(&self) -> bool {
        let mtime =
            UNIX_EPOCH + Duration::new(self.mtime.0.max(0) as u64, self.mtime.1.max(0) as u32);
        SystemTime::now()
            .duration_since(mtime)
            .is_ok_and(|age| age < RACY_WINDOW)
            || mtime > SystemTime::now()
    }
}

/// 最近一次载入的规则文件状态
#[derive(Default)]
struct Loaded {
    stamp: Option<FileStamp>,
    text: String,
}

/// 一次设置变更，None 表示不改
#[derive(Default)]
pub struct ConfigPatch {
    pub mode: Option<Mode>,
    pub interval: Option<u64>,
    pub cpuset_name: Option<String>,
    pub config_file: Option<String>,
}

#[derive(Debug)]
pub enum RuleEditError {
    Edit(EditError),
    Io(io::Error),
}

impl From<io::Error> for RuleEditError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub struct Shared {
    settings: Mutex<Settings>,
    settings_path: PathBuf,
    topo: CpuTopology,
    cpusets: Mutex<Option<Arc<CpusetFs>>>,
    config: RwLock<Arc<AppConfig>>,
    config_gen: AtomicU64,
    /// 持锁期间串行化重载
    loaded: Mutex<Loaded>,
    /// 串行化规则文件的读-改-写
    edit_lock: Mutex<()>,
    stats: Mutex<Stats>,
    web_running: AtomicBool,
    /// 自动模式下 eBPF 初始化失败后不再重试，用户切换模式时清除
    pub ebpf_gave_up: AtomicBool,
    main_thread: OnceLock<Thread>,
    pub started: Instant,
}

impl Shared {
    /// 载入初始规则集；规则文件必须可读
    pub fn new(
        settings: Settings,
        settings_path: PathBuf,
        topo: CpuTopology,
        cpusets: Option<Arc<CpusetFs>>,
    ) -> io::Result<Self> {
        let this = Self {
            settings: Mutex::new(settings),
            settings_path,
            config: RwLock::new(Arc::new(AppConfig::parse(
                "",
                topo.clone(),
                cpusets.clone(),
            ))),
            topo,
            cpusets: Mutex::new(cpusets),
            config_gen: AtomicU64::new(0),
            loaded: Mutex::new(Loaded::default()),
            edit_lock: Mutex::new(()),
            stats: Mutex::new(Stats::default()),
            web_running: AtomicBool::new(false),
            ebpf_gave_up: AtomicBool::new(false),
            main_thread: OnceLock::new(),
            started: Instant::now(),
        };
        this.reload_config(true)?;
        Ok(this)
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().expect("设置锁中毒").clone()
    }

    /// 修改设置并写回设置文件；落盘失败时内存中的设置保持不变
    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) -> io::Result<()> {
        let mut guard = self.settings.lock().expect("设置锁中毒");
        let mut next = guard.clone();
        f(&mut next);
        next.save(&self.settings_path)?;
        *guard = next;
        Ok(())
    }

    pub fn save_settings(&self) -> io::Result<()> {
        self.update_settings(|_| {})
    }

    pub fn config(&self) -> Arc<AppConfig> {
        self.config.read().expect("配置锁中毒").clone()
    }

    /// 规则集每替换一次加一，主循环据此感知变更
    pub fn config_gen(&self) -> u64 {
        self.config_gen.load(Ordering::Acquire)
    }

    pub fn topo(&self) -> &CpuTopology {
        &self.topo
    }

    pub fn stats(&self) -> Stats {
        *self.stats.lock().expect("统计锁中毒")
    }

    pub fn publish_stats(&self, s: Stats) {
        *self.stats.lock().expect("统计锁中毒") = s;
    }

    pub fn web_running(&self) -> bool {
        self.web_running.load(Ordering::Relaxed)
    }

    pub fn set_web_running(&self) {
        self.web_running.store(true, Ordering::Relaxed);
    }

    pub fn register_main_thread(&self) {
        self.main_thread
            .set(thread::current())
            .expect("主线程只注册一次");
    }

    /// 唤醒休眠中的主循环
    pub fn wake(&self) {
        if let Some(t) = self.main_thread.get() {
            t.unpark();
        }
    }

    /// 重新读取规则文件；内容未变且非强制时不替换规则集。返回是否替换了规则集
    pub fn reload_config(&self, force: bool) -> io::Result<bool> {
        let mut loaded = self.loaded.lock().expect("重载锁中毒");
        let path = PathBuf::from(self.settings().config_file);
        let now = FileStamp::of(&path)?;
        // 指纹相同且修改时间不新，足以认定内容未变；太新时要比对内容
        if !force && loaded.stamp == Some(now) && !now.is_racy() {
            return Ok(false);
        }
        let text = fs::read_to_string(&path)?;
        if !force && text == loaded.text {
            loaded.stamp = Some(now);
            return Ok(false);
        }
        let cpusets = self.cpusets.lock().expect("cpuset 锁中毒").clone();
        let cfg = AppConfig::parse(&text, self.topo.clone(), cpusets);

        println!("配置文件解析完成，共加载 {} 条规则", cfg.rule_count());
        for issue in &cfg.issues {
            eprintln!("警告: {issue}");
        }
        if cfg.parse_fails > 0 {
            eprintln!("警告: {} 条规则因格式无效被跳过", cfg.parse_fails);
        }

        *self.config.write().expect("配置锁中毒") = Arc::new(cfg);
        *loaded = Loaded {
            stamp: Some(now),
            text,
        };
        self.config_gen.fetch_add(1, Ordering::AcqRel);
        self.wake();
        Ok(true)
    }

    /// 在规则文件上执行一次行级编辑并重载；文件不存在按空文件处理
    pub fn edit_rules(
        &self,
        edit: impl FnOnce(&mut Vec<String>) -> Result<(), EditError>,
    ) -> Result<(), RuleEditError> {
        let _guard = self.edit_lock.lock().expect("编辑锁中毒");
        let path = PathBuf::from(self.settings().config_file);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        let mut lines: Vec<String> = text.lines().map(String::from).collect();
        edit(&mut lines).map_err(RuleEditError::Edit)?;
        let mut out = lines.join("\n");
        out.push('\n');
        write_atomic(&path, out.as_bytes())?;
        self.reload_config(false)?;
        Ok(())
    }

    /// 应用设置变更：先完成全部可能失败的准备（cpuset 目录、规则文件、设置落盘），再一并生效
    pub fn patch(&self, p: ConfigPatch) -> io::Result<()> {
        let cpusets = match &p.cpuset_name {
            Some(name) => Some(
                CpusetFs::open(Path::new(CPUSET_ROOT), name, &self.topo.present)?.map(Arc::new),
            ),
            None => None,
        };
        if let Some(file) = &p.config_file {
            ensure_file(Path::new(file))?;
            fs::read_to_string(file)?;
        }
        let reload = p.cpuset_name.is_some() || p.config_file.is_some();
        let ebpf_wanted = p.mode.is_some_and(|m| m != Mode::Proc);

        self.update_settings(|s| {
            s.mode = p.mode.unwrap_or(s.mode);
            s.check_interval = p.interval.unwrap_or(s.check_interval);
            if let Some(name) = p.cpuset_name {
                s.cpuset_name = name;
            }
            if let Some(file) = p.config_file {
                s.config_file = file;
            }
        })?;

        if let Some(cpusets) = cpusets {
            *self.cpusets.lock().expect("cpuset 锁中毒") = cpusets;
        }
        if ebpf_wanted {
            self.ebpf_gave_up.store(false, Ordering::Relaxed);
        }
        if reload {
            self.reload_config(true)?;
        }
        self.wake();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpuset::CpuSet;
    use crate::rule_edit;

    fn fixture(name: &str, rules: &str) -> (Shared, PathBuf) {
        let dir = std::env::temp_dir().join(format!("appopt-shared-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("applist.conf");
        fs::write(&conf, rules).unwrap();
        let settings = Settings {
            config_file: conf.to_string_lossy().into_owned(),
            ..Settings::default()
        };
        let set = |s| CpuSet::parse_list(s).unwrap();
        let topo = CpuTopology::from_groups(set("0-7"), [(1, set("0-3")), (2, set("4-7"))]);
        let shared = Shared::new(settings, dir.join("AppOpt.json"), topo, None).unwrap();
        (shared, dir)
    }

    #[test]
    fn initial_load_and_unchanged_reload() {
        let (s, dir) = fixture("load", "a=e-core\n");
        assert_eq!(s.config().rule_count(), 1);
        assert_eq!(s.config_gen(), 1);
        assert!(!s.reload_config(false).unwrap());
        assert_eq!(s.config_gen(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn external_change_is_picked_up_once() {
        let (s, dir) = fixture("ext", "a=e-core\n");
        let conf = PathBuf::from(s.settings().config_file);
        fs::write(&conf, "a=e-core\nb=hp-core\n").unwrap();
        assert!(s.reload_config(false).unwrap());
        assert_eq!(s.config().rule_count(), 2);
        assert!(!s.reload_config(false).unwrap());
        fs::remove_dir_all(dir).unwrap();
    }

    /// 同一时间戳粒度内同长度的两次改写，指纹相同，也不能漏掉第二次
    #[test]
    fn same_fingerprint_rewrite_is_not_missed() {
        let (s, dir) = fixture("racy", "a=e-core\n");
        let conf = PathBuf::from(s.settings().config_file);
        let before = fs::metadata(&conf).unwrap().modified().unwrap();
        fs::write(&conf, "b=e-core\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&conf)
            .unwrap()
            .set_modified(before)
            .unwrap();
        assert!(s.reload_config(false).unwrap());
        assert!(s.config().group("b").is_some());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn edit_writes_file_and_reloads_once() {
        let (s, dir) = fixture("edit", "a=e-core\n");
        let gen0 = s.config_gen();
        s.edit_rules(|l| rule_edit::upsert(l, "a", "T", "hp-core"))
            .unwrap();
        assert_eq!(s.config_gen(), gen0 + 1);
        assert_eq!(s.config().group("a").unwrap().items.len(), 2);
        let text = fs::read_to_string(s.settings().config_file).unwrap();
        assert_eq!(text, "a=e-core {\n\tT=hp-core\n}\n");
        // 随后的文件监听回调不会重复加载
        assert!(!s.reload_config(false).unwrap());

        assert!(matches!(
            s.edit_rules(|l| rule_edit::delete(l, "zzz", "")),
            Err(RuleEditError::Edit(EditError::NotFound))
        ));
        assert_eq!(s.config_gen(), gen0 + 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_rule_file_fails_at_startup() {
        let settings = Settings {
            config_file: "/nonexistent/applist.conf".into(),
            ..Settings::default()
        };
        let topo = CpuTopology::from_groups(CpuSet::default(), []);
        assert!(
            Shared::new(
                settings,
                PathBuf::from("/nonexistent/AppOpt.json"),
                topo,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn failed_patch_changes_nothing() {
        let (s, dir) = fixture("patchfail", "a=e-core\n");
        let before = s.settings();
        // 设置文件所在目录不存在，落盘必然失败
        let broken = Shared::new(
            before.clone(),
            dir.join("missing/AppOpt.json"),
            s.topo().clone(),
            None,
        )
        .unwrap();
        let r = broken.patch(ConfigPatch {
            mode: Some(Mode::Proc),
            interval: Some(9),
            ..Default::default()
        });
        assert!(r.is_err());
        assert_eq!(broken.settings(), before);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn patch_switches_rule_file() {
        let (s, dir) = fixture("patch", "a=e-core\n");
        let other = dir.join("other.conf");
        s.patch(ConfigPatch {
            mode: Some(Mode::Proc),
            interval: Some(9),
            config_file: Some(other.to_string_lossy().into_owned()),
            ..Default::default()
        })
        .unwrap();
        // 新文件被创建并加载，设置落盘
        assert!(other.exists());
        assert_eq!(s.config().rule_count(), 0);
        let st = s.settings();
        assert_eq!((st.mode, st.check_interval), (Mode::Proc, 9));
        assert_eq!(Settings::load(&dir.join("AppOpt.json")).unwrap(), st);
        fs::remove_dir_all(dir).unwrap();
    }
}
