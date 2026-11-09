//! CPU 集合、核心分层拓扑与 cpuset 目录管理

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

pub const CPU_SETSIZE: usize = 1024;
const WORD_BITS: usize = u64::BITS as usize;
const WORDS: usize = CPU_SETSIZE / WORD_BITS;

pub const SYS_PRESENT: &str = "/sys/devices/system/cpu/present";
pub const SYS_CPUFREQ: &str = "/sys/devices/system/cpu/cpufreq";
pub const CPUSET_ROOT: &str = "/dev/cpuset";

/// 与内核 cpu_set_t 布局一致的位图，下标即 CPU 编号
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct CpuSet {
    bits: [u64; WORDS],
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const _: () = {
    assert!(std::mem::size_of::<CpuSet>() == std::mem::size_of::<libc::cpu_set_t>());
    assert!(cfg!(target_endian = "little"));
};

impl CpuSet {
    pub fn insert(&mut self, cpu: usize) {
        self.bits[cpu / WORD_BITS] |= 1 << (cpu % WORD_BITS);
    }

    pub fn contains(&self, cpu: usize) -> bool {
        cpu < CPU_SETSIZE && self.bits[cpu / WORD_BITS] & (1 << (cpu % WORD_BITS)) != 0
    }

    pub fn len(&self) -> usize {
        self.bits.iter().map(|w| w.count_ones() as usize).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.bits.iter().all(|&w| w == 0)
    }

    pub fn union(mut self, other: &CpuSet) -> CpuSet {
        for (a, b) in self.bits.iter_mut().zip(&other.bits) {
            *a |= b;
        }
        self
    }

    pub fn intersection(mut self, other: &CpuSet) -> CpuSet {
        for (a, b) in self.bits.iter_mut().zip(&other.bits) {
            *a &= b;
        }
        self
    }

    /// 按编号升序遍历
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.bits.iter().enumerate().flat_map(|(w, &word)| {
            let mut rest = word;
            std::iter::from_fn(move || {
                (rest != 0).then(|| {
                    let bit = rest.trailing_zeros() as usize;
                    rest &= rest - 1;
                    w * WORD_BITS + bit
                })
            })
        })
    }

    /// 加入 lo..=hi，超出 CPU_SETSIZE 的部分忽略
    pub fn insert_range(&mut self, lo: usize, hi: usize) {
        for cpu in lo..=hi.min(CPU_SETSIZE - 1) {
            self.insert(cpu);
        }
    }

    /// 解析内核风格列表 `0-3,6`，空串得到空集合
    pub fn parse_list(s: &str) -> Result<CpuSet, SpecError> {
        let mut set = CpuSet::default();
        for tok in s.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            let (lo, hi) = parse_range(tok).ok_or_else(|| SpecError::Token(tok.to_string()))?;
            set.insert_range(lo, hi);
        }
        Ok(set)
    }
}

/// `3` 或 `2-5`，反向范围按升序处理
fn parse_range(tok: &str) -> Option<(usize, usize)> {
    let num = |s: &str| {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<usize>().ok())
            .flatten()
    };
    match tok.split_once('-') {
        None => num(tok).map(|n| (n, n)),
        Some((a, b)) => {
            let (a, b) = (num(a)?, num(b)?);
            Some((a.min(b), a.max(b)))
        }
    }
}

/// 输出 `0-3,6-7` 形式
impl fmt::Display for CpuSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut cpus = self.iter().peekable();
        let mut first = true;
        while let Some(lo) = cpus.next() {
            let mut hi = lo;
            while cpus.next_if_eq(&(hi + 1)).is_some() {
                hi += 1;
            }
            if !first {
                f.write_str(",")?;
            }
            first = false;
            if lo == hi {
                write!(f, "{lo}")?;
            } else {
                write!(f, "{lo}-{hi}")?;
            }
        }
        Ok(())
    }
}

impl fmt::Debug for CpuSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CpuSet({self})")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SpecError {
    Empty,
    Token(String),
    /// 语法合法但与本机在线核心无交集
    NoCpu,
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpecError::Empty => f.write_str("CPU 规格为空"),
            SpecError::Token(t) => write!(f, "无效的 CPU 规格: {t}"),
            SpecError::NoCpu => f.write_str("CPU 规格与本机核心无交集"),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl CpuSet {
    pub fn of_thread(tid: i32) -> io::Result<CpuSet> {
        let mut set = CpuSet::default();
        // SAFETY: CpuSet 与 cpu_set_t 布局一致，指针与长度描述同一块缓冲区
        let ret = unsafe {
            libc::sched_getaffinity(tid, size_of::<CpuSet>(), (&mut set as *mut CpuSet).cast())
        };
        if ret == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(set)
        }
    }

    pub fn pin_thread(&self, tid: i32) -> io::Result<()> {
        // SAFETY: 同 of_thread
        let ret = unsafe {
            libc::sched_setaffinity(tid, size_of::<CpuSet>(), (self as *const CpuSet).cast())
        };
        if ret == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

/// 核心分层：按 cpufreq 策略最高频率升序，首组为 e-core，末组为 hp-core，中间各组合并为 p-core
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpuTopology {
    pub present: CpuSet,
    pub e_core: CpuSet,
    pub p_core: CpuSet,
    pub hp_core: CpuSet,
}

impl CpuTopology {
    /// 读取 sysfs；present 不可读视为错误，缺少 cpufreq 时不分层
    pub fn detect() -> io::Result<Self> {
        let present = CpuSet::parse_list(fs::read_to_string(SYS_PRESENT)?.trim()).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("{SYS_PRESENT}: {e}"))
        })?;
        let entries = match fs::read_dir(SYS_CPUFREQ) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(Self::from_groups(present, []));
            }
            Err(e) => return Err(e),
        };
        let mut groups = Vec::new();
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("policy") {
                continue;
            }
            let dir = entry.path();
            let freq: u64 = fs::read_to_string(dir.join("cpuinfo_max_freq"))
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            let mut cpus = CpuSet::default();
            for cpu in fs::read_to_string(dir.join("related_cpus"))
                .unwrap_or_default()
                .split_whitespace()
                .filter_map(|c| c.parse::<usize>().ok().filter(|&c| c < CPU_SETSIZE))
            {
                cpus.insert(cpu);
            }
            if freq > 0 && !cpus.is_empty() {
                groups.push((freq, cpus));
            }
        }
        Ok(Self::from_groups(present, groups))
    }

    /// 由 (最高频率, 核心集合) 分组构造，同频率分组合并
    pub fn from_groups(present: CpuSet, groups: impl IntoIterator<Item = (u64, CpuSet)>) -> Self {
        let mut by_freq: BTreeMap<u64, CpuSet> = BTreeMap::new();
        for (freq, cpus) in groups {
            let slot = by_freq.entry(freq).or_default();
            *slot = slot.union(&cpus);
        }
        let mut tiers = by_freq.into_values().collect::<Vec<_>>();
        let hp_core = if tiers.len() >= 2 {
            tiers.pop().unwrap()
        } else {
            CpuSet::default()
        };
        let mut tiers = tiers.into_iter();
        let e_core = tiers.next().unwrap_or_default();
        let p_core = tiers.fold(CpuSet::default(), |acc, t| acc.union(&t));
        Self {
            present,
            e_core,
            p_core,
            hp_core,
        }
    }

    /// 解析 `e-core,0-3,7` 形式的规格：语义名与数字范围可混用，数字范围按在线核心裁剪
    pub fn parse_spec(&self, spec: &str) -> Result<CpuSet, SpecError> {
        let mut set = CpuSet::default();
        let mut any = false;
        for tok in spec.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            any = true;
            match tok {
                "e-core" => set = set.union(&self.e_core),
                "p-core" => set = set.union(&self.p_core),
                "hp-core" => set = set.union(&self.hp_core),
                "all-core" => set = set.union(&self.present),
                _ => {
                    let (lo, hi) =
                        parse_range(tok).ok_or_else(|| SpecError::Token(tok.to_string()))?;
                    let mut range = CpuSet::default();
                    range.insert_range(lo, hi);
                    set = set.union(&range.intersection(&self.present));
                }
            }
        }
        if !any {
            Err(SpecError::Empty)
        } else if set.is_empty() {
            Err(SpecError::NoCpu)
        } else {
            Ok(set)
        }
    }

    /// 集合对应的规格名：恰为某一分层时返回语义名，否则返回范围串
    pub fn spec_name(&self, cpus: &CpuSet) -> String {
        if !cpus.is_empty() {
            for (name, tier) in [
                ("e-core", &self.e_core),
                ("p-core", &self.p_core),
                ("hp-core", &self.hp_core),
                ("all-core", &self.present),
            ] {
                if cpus == tier {
                    return name.to_string();
                }
            }
        }
        cpus.to_string()
    }
}

/// `/dev/cpuset/<name>` 下的 cpuset 目录：根目录容纳全部在线核心，其余按 CPU 集合各建一个子目录
pub struct CpusetFs {
    root: PathBuf,
    mems: String,
    present: CpuSet,
    /// 已建好的子目录
    ready: Mutex<HashSet<CpuSet>>,
    /// 创建失败并已告警的子目录；失败不缓存，下次使用时重试
    warned: Mutex<HashSet<CpuSet>>,
    move_warned: AtomicBool,
}

impl CpusetFs {
    /// 创建 `<cpuset_root>/<name>`；cpuset_root 不存在表示系统未挂载 cpuset，返回 Ok(None)
    pub fn open(cpuset_root: &Path, name: &str, present: &CpuSet) -> io::Result<Option<Self>> {
        if !cpuset_root.exists() {
            return Ok(None);
        }
        let mems = fs::read_to_string(cpuset_root.join("mems"))?
            .trim()
            .to_string();
        if mems.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cpuset mems 为空",
            ));
        }
        let this = Self {
            root: cpuset_root.join(name),
            mems,
            present: *present,
            ready: Mutex::new(HashSet::new()),
            warned: Mutex::new(HashSet::new()),
            move_warned: AtomicBool::new(false),
        };
        make_dir(&this.root, &present.to_string(), &this.mems)?;
        Ok(Some(this))
    }

    pub fn name(&self) -> &str {
        self.root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
    }

    /// cpus 对应目录的 tasks 文件，子目录按需创建
    pub fn tasks_path(&self, cpus: &CpuSet) -> PathBuf {
        if *cpus == self.present {
            return self.root.join("tasks");
        }
        let name = cpus.to_string();
        let dir = self.root.join(&name);
        let mut ready = self.ready.lock().expect("cpuset 目录表锁中毒");
        if !ready.contains(cpus) {
            match make_dir(&dir, &name, &self.mems) {
                Ok(()) => {
                    ready.insert(*cpus);
                }
                Err(e) => {
                    if self
                        .warned
                        .lock()
                        .expect("cpuset 告警表锁中毒")
                        .insert(*cpus)
                    {
                        eprintln!(
                            "警告: 创建 cpuset 目录 {} 失败 ({e})，暂用 {}，之后每次使用时重试",
                            dir.display(),
                            self.root.display()
                        );
                    }
                    return self.root.join("tasks");
                }
            }
        }
        dir.join("tasks")
    }

    /// 把线程移入 cpus 对应的 cpuset，线程已退出（ESRCH）时返回 false；其余失败仅首次告警
    fn adopt(&self, tid: i32, cpus: &CpuSet) -> bool {
        use std::io::Write;
        let path = self.tasks_path(cpus);
        let res = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut f| writeln!(f, "{tid}"));
        match res {
            Err(e) if e.raw_os_error() == Some(libc::ESRCH) => false,
            Err(e) => {
                if !self.move_warned.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "警告: 写入 {} 失败 ({e})，后续同类错误不再提示",
                        path.display()
                    );
                }
                true
            }
            Ok(()) => true,
        }
    }
}

fn make_dir(path: &Path, cpus: &str, mems: &str) -> io::Result<()> {
    match fs::create_dir(path) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    fs::write(path.join("cpus"), cpus)?;
    fs::write(path.join("mems"), mems)
}

/// 单线程绑定结果
#[cfg(any(target_os = "linux", target_os = "android"))]
pub enum Bind {
    Done,
    /// 线程已退出
    Gone,
    Failed(io::Error),
}

/// 把线程绑定到 cpus：先移入对应 cpuset（若启用），再设置亲和性；已符合目标时不做任何写入
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn bind_thread(tid: i32, cpus: &CpuSet, cpusets: Option<&CpusetFs>) -> Bind {
    if CpuSet::of_thread(tid).is_ok_and(|cur| cur == *cpus) {
        return Bind::Done;
    }
    if cpusets.is_some_and(|fs| !fs.adopt(tid, cpus)) {
        return Bind::Gone;
    }
    match cpus.pin_thread(tid) {
        Ok(()) => Bind::Done,
        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => Bind::Gone,
        Err(e) => Bind::Failed(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(spec: &str) -> CpuSet {
        CpuSet::parse_list(spec).unwrap()
    }

    fn topo_3tier() -> CpuTopology {
        CpuTopology::from_groups(
            set("0-7"),
            [
                (1800000, set("0-3")),
                (2400000, set("4-6")),
                (3000000, set("7")),
            ],
        )
    }

    #[test]
    fn display_groups_runs() {
        assert_eq!(set("0,1,2,5,7-9").to_string(), "0-2,5,7-9");
        assert_eq!(set("").to_string(), "");
        assert_eq!(set("63,64,65").to_string(), "63-65");
        assert_eq!(set("1023").to_string(), "1023");
    }

    #[test]
    fn parse_list_rules() {
        assert_eq!(set("3-1"), set("1-3"));
        assert_eq!(set(" 0 , 2 ").to_string(), "0,2");
        assert_eq!(set("2000").to_string(), "");
        assert!(CpuSet::parse_list("a").is_err());
        assert!(CpuSet::parse_list("1-").is_err());
        assert!(CpuSet::parse_list("1-2-3").is_err());
        assert!(CpuSet::parse_list("99999999999999999999").is_err());
    }

    #[test]
    fn tiers_from_freq_groups() {
        let t = topo_3tier();
        assert_eq!(t.e_core, set("0-3"));
        assert_eq!(t.p_core, set("4-6"));
        assert_eq!(t.hp_core, set("7"));

        let two = CpuTopology::from_groups(set("0-7"), [(1, set("0-3")), (2, set("4-7"))]);
        assert_eq!(
            (two.e_core, two.p_core, two.hp_core),
            (set("0-3"), set(""), set("4-7"))
        );

        let one = CpuTopology::from_groups(set("0-3"), [(1, set("0-1")), (1, set("2-3"))]);
        assert_eq!(
            (one.e_core, one.p_core, one.hp_core),
            (set("0-3"), set(""), set(""))
        );

        let none = CpuTopology::from_groups(set("0-3"), []);
        assert!(none.e_core.is_empty() && none.hp_core.is_empty());
    }

    #[test]
    fn spec_resolution() {
        let t = topo_3tier();
        assert_eq!(t.parse_spec("e-core,hp-core"), Ok(set("0-3,7")));
        assert_eq!(t.parse_spec("all-core"), Ok(set("0-7")));
        assert_eq!(t.parse_spec("0-1,6-20"), Ok(set("0-1,6-7")));
        assert_eq!(t.parse_spec("5-4"), Ok(set("4-5")));
        assert_eq!(t.parse_spec(""), Err(SpecError::Empty));
        assert_eq!(t.parse_spec(" , "), Err(SpecError::Empty));
        assert_eq!(t.parse_spec("8-9"), Err(SpecError::NoCpu));
        assert_eq!(
            t.parse_spec("x-core"),
            Err(SpecError::Token("x-core".into()))
        );
        assert_eq!(t.parse_spec("0-3,abc"), Err(SpecError::Token("abc".into())));
        // 语义名在无该分层的设备上贡献空集，整体为空时报错
        let flat = CpuTopology::from_groups(set("0-3"), []);
        assert_eq!(flat.parse_spec("hp-core"), Err(SpecError::NoCpu));
        assert_eq!(flat.parse_spec("hp-core,0"), Ok(set("0")));
    }

    #[test]
    fn spec_names() {
        let t = topo_3tier();
        assert_eq!(t.spec_name(&set("0-3")), "e-core");
        assert_eq!(t.spec_name(&set("4-6")), "p-core");
        assert_eq!(t.spec_name(&set("7")), "hp-core");
        assert_eq!(t.spec_name(&set("0-7")), "all-core");
        assert_eq!(t.spec_name(&set("1,3")), "1,3");
    }

    #[test]
    fn cpuset_dirs_follow_cpu_sets() {
        let base = std::env::temp_dir().join(format!("appopt-cpuset-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("mems"), "0\n").unwrap();

        let present = set("0-7");
        let cs = CpusetFs::open(&base, "AppOpt", &present).unwrap().unwrap();
        assert_eq!(cs.name(), "AppOpt");
        assert_eq!(fs::read_to_string(base.join("AppOpt/cpus")).unwrap(), "0-7");
        assert_eq!(fs::read_to_string(base.join("AppOpt/mems")).unwrap(), "0");

        assert_eq!(cs.tasks_path(&present), base.join("AppOpt/tasks"));
        let sub = cs.tasks_path(&set("4-7"));
        assert_eq!(sub, base.join("AppOpt/4-7/tasks"));
        assert_eq!(
            fs::read_to_string(base.join("AppOpt/4-7/cpus")).unwrap(),
            "4-7"
        );
        assert_eq!(cs.tasks_path(&set("4-7")), sub);

        assert!(
            CpusetFs::open(&base.join("absent"), "x", &present)
                .unwrap()
                .is_none()
        );
        fs::remove_dir_all(&base).unwrap();
    }
}
