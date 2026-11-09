//! 命令行参数

use crate::settings::{
    DEFAULT_CONFIG_FILE, DEFAULT_CPUSET_NAME, DEFAULT_INTERVAL, INTERVAL_RANGE, SETTINGS_FILE,
    Settings, valid_cpuset_name, valid_path,
};

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run(Overrides),
    Version,
    Help,
}

/// 命令行给出的设置，优先于设置文件
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Overrides {
    pub config_file: Option<String>,
    pub interval: Option<u64>,
    pub cpuset_name: Option<String>,
    pub web: bool,
}

impl Overrides {
    pub fn apply(self, mut s: Settings) -> Settings {
        s.config_file = self.config_file.unwrap_or(s.config_file);
        s.check_interval = self.interval.unwrap_or(s.check_interval);
        s.cpuset_name = self.cpuset_name.unwrap_or(s.cpuset_name);
        s.web_enable |= self.web;
        s
    }
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let mut o = Overrides::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |what: &str| {
            it.next()
                .ok_or_else(|| format!("错误: {arg} 需要指定{what}"))
        };
        match arg.as_str() {
            "-c" => {
                let v = value("配置文件路径")?;
                if !valid_path(v) {
                    return Err(format!("无效的配置文件路径: {v}"));
                }
                o.config_file = Some(v.clone());
            }
            "-s" => {
                let v = value("时间间隔")?;
                o.interval = Some(
                    v.parse()
                        .ok()
                        .filter(|n| INTERVAL_RANGE.contains(n))
                        .ok_or_else(|| {
                            format!(
                                "无效的时间间隔: {v}\n间隔必须是 {}-{} 的整数",
                                INTERVAL_RANGE.start(),
                                INTERVAL_RANGE.end()
                            )
                        })?,
                );
            }
            "-b" => {
                let v = value("cpuset 目录名")?;
                if !valid_cpuset_name(v) {
                    return Err(format!(
                        "无效的 cpuset 目录名: {v}\n目录名不能为空，不能含路径分隔符或空白"
                    ));
                }
                o.cpuset_name = Some(v.clone());
            }
            "-w" => o.web = true,
            "-v" => return Ok(Command::Version),
            "-h" => return Ok(Command::Help),
            other => return Err(format!("未知选项: {other}")),
        }
    }
    Ok(Command::Run(o))
}

pub fn usage(prog: &str) -> String {
    format!(
        "Usage: {prog} [OPTIONS]
Options:
  -c <config_file>   指定配置文件 (默认: {DEFAULT_CONFIG_FILE})
  -s <interval>      设置检查间隔(秒) ({lo}-{hi}, 默认: {DEFAULT_INTERVAL})
  -b <cpuset_name>   指定 BASE_CPUSET 目录名 (默认: {DEFAULT_CPUSET_NAME})
  -w                 启用网页前端 (仅本机 127.0.0.1:8889)
  -v                 显示程序版本
  -h                 显示帮助信息

示例:
  {prog} -c /data/applist.conf -s 3
  {prog} -b MyAppOpt

应用设置保存于 {SETTINGS_FILE}，首次运行自动创建；
命令行参数优先于设置文件，web 端修改会写回该文件。

规则格式:
  # 注释以 # 或 // 开头
  com.example=0-3           包级规则，绑定到 CPU 0-3
  com.example=e-core        语义核心，绑定到全部小核
  com.example=p-core        语义核心，绑定到全部中核
  com.example=hp-core       语义核心，绑定到全部大核

  块语法，包级规则 + 线程规则
  com.example {{
    RenderThread=6-7
    Thread-1=0-5
  }}
  线程 RenderThread 绑定到 CPU 6-7
  线程 Thread-1 绑定到 CPU 0-5",
        lo = INTERVAL_RANGE.start(),
        hi = INTERVAL_RANGE.end()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_options() {
        let Command::Run(o) =
            parse(&args(&["-c", "/x.conf", "-s", "3", "-b", "My", "-w"])).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            o,
            Overrides {
                config_file: Some("/x.conf".into()),
                interval: Some(3),
                cpuset_name: Some("My".into()),
                web: true
            }
        );
        assert_eq!(
            parse(&args(&[])).unwrap(),
            Command::Run(Overrides::default())
        );
        assert_eq!(parse(&args(&["-v"])).unwrap(), Command::Version);
        assert_eq!(parse(&args(&["-h"])).unwrap(), Command::Help);
    }

    #[test]
    fn rejects_bad_input() {
        for bad in [
            &["-c"][..],
            &["-s"],
            &["-s", "0"],
            &["-s", "x"],
            &["-b", "a/b"],
            &["-b", ""],
            &["-z"],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn overrides_win_over_settings() {
        let o = Overrides {
            interval: Some(9),
            web: true,
            ..Default::default()
        };
        let s = o.apply(Settings::default());
        assert_eq!(
            (s.check_interval, s.web_enable, s.cpuset_name.as_str()),
            (9, true, DEFAULT_CPUSET_NAME)
        );
    }
}
