#![cfg(any(target_os = "linux", target_os = "android"))]

use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;

use appopt::cli::{self, Command};
use appopt::config::ensure_file;
use appopt::cpuset::{CPUSET_ROOT, CpuTopology, CpusetFs};
use appopt::settings::{SETTINGS_FILE, Settings};
use appopt::shared::Shared;
use appopt::{daemon, ebpf_mode, watcher, web};

fn fatal(msg: impl std::fmt::Display) -> ! {
    eprintln!("错误: {msg}");
    process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.first().map_or("AppOpt", String::as_str);

    match cli::parse(&args[1..]) {
        Ok(Command::Run(overrides)) => run(overrides),
        Ok(Command::Version) => {
            let suffix = if ebpf_mode::available() { " eBPF" } else { "" };
            println!("AppOpt 版本 {}{suffix}", env!("CARGO_PKG_VERSION"));
        }
        Ok(Command::Help) => println!("{}", cli::usage(prog)),
        Err(msg) => {
            eprintln!("{msg}");
            eprintln!("{}", cli::usage(prog));
            process::exit(1);
        }
    }
}

fn run(overrides: cli::Overrides) {
    let settings =
        overrides.apply(Settings::load(Path::new(SETTINGS_FILE)).unwrap_or_else(|e| fatal(e)));

    let topo = CpuTopology::detect().unwrap_or_else(|e| fatal(format!("读取 CPU 拓扑失败: {e}")));
    let cpusets = match CpusetFs::open(Path::new(CPUSET_ROOT), &settings.cpuset_name, &topo.present)
    {
        Ok(fs) => fs.map(Arc::new),
        Err(e) => {
            eprintln!("警告: cpuset 不可用 ({e})，仅设置 CPU 亲和性");
            None
        }
    };

    let config_file = PathBuf::from(&settings.config_file);
    match ensure_file(&config_file) {
        Ok(true) => println!(
            "配置文件不存在，重建一个空的配置文件: {}",
            config_file.display()
        ),
        Ok(false) => {}
        Err(e) => fatal(format!("无法创建配置文件 {}: {e}", config_file.display())),
    }

    let web_enable = settings.web_enable;
    let shared = Shared::new(settings, PathBuf::from(SETTINGS_FILE), topo, cpusets)
        .unwrap_or_else(|e| fatal(format!("初始配置加载失败: {e}")));
    let shared = Arc::new(shared);

    if web_enable {
        if let Err(e) = web::start(shared.clone()) {
            eprintln!("Web: 监听 127.0.0.1:{} 失败 ({e})", web::PORT);
        }
        // -w 启用后落盘，重启保持开启；同时保存命令行覆盖的设置
        if let Err(e) = shared.save_settings() {
            eprintln!("警告: 写入设置失败 ({e})");
        }
    }
    watcher::spawn(shared.clone()).unwrap_or_else(|e| fatal(format!("启动配置监听线程失败: {e}")));

    println!("启动AppOpt服务 v{}", env!("CARGO_PKG_VERSION"));
    daemon::run(shared)
}
