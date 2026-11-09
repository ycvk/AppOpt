//! 应用设置：持久化于 AppOpt.json，命令行参数优先，web 端修改后写回

use std::fs;
use std::io;
use std::path::Path;

use serde_json::{Value, json};

use crate::fsutil::write_atomic;

pub const SETTINGS_FILE: &str = "./AppOpt.json";
pub const DEFAULT_CPUSET_NAME: &str = "AppOpt";
pub const DEFAULT_CONFIG_FILE: &str = "./applist.conf";
pub const DEFAULT_INTERVAL: u64 = 2;
pub const INTERVAL_RANGE: std::ops::RangeInclusive<u64> = 1..=3600;

/// 工作模式，数值同时是 JSON 与 web 接口中的编码
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// 优先 eBPF，初始化失败一次后保持 /proc
    Auto = 0,
    /// 强制 eBPF，不可用时持续重试
    Ebpf = 1,
    Proc = 2,
}

impl TryFrom<u64> for Mode {
    type Error = ();

    fn try_from(v: u64) -> Result<Self, ()> {
        match v {
            0 => Ok(Mode::Auto),
            1 => Ok(Mode::Ebpf),
            2 => Ok(Mode::Proc),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub web_enable: bool,
    pub mode: Mode,
    pub check_interval: u64,
    pub cpuset_name: String,
    pub config_file: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            web_enable: false,
            mode: Mode::Auto,
            check_interval: DEFAULT_INTERVAL,
            cpuset_name: DEFAULT_CPUSET_NAME.to_string(),
            config_file: DEFAULT_CONFIG_FILE.to_string(),
        }
    }
}

pub fn valid_cpuset_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() < 64
        && s != "."
        && s != ".."
        && !s.contains('/')
        && !s.bytes().any(|b| b <= b' ')
}

pub fn valid_path(s: &str) -> bool {
    !s.is_empty() && s.len() < 256 && !s.bytes().any(|b| b < 0x20 || b == 0x7f)
}

/// 字段缺省或为 null 视为未提供；提供了就必须被 parse 接受，否则返回 bad()
pub fn opt_field<'a, T, E>(
    v: &'a Value,
    key: &str,
    parse: impl FnOnce(&'a Value) -> Option<T>,
    bad: impl FnOnce() -> E,
) -> Result<Option<T>, E> {
    match v.get(key).filter(|x| !x.is_null()) {
        None => Ok(None),
        Some(x) => parse(x).map(Some).ok_or_else(bad),
    }
}

pub fn parse_mode(x: &Value) -> Option<Mode> {
    x.as_u64().and_then(|n| Mode::try_from(n).ok())
}

pub fn parse_interval(x: &Value) -> Option<u64> {
    x.as_u64().filter(|n| INTERVAL_RANGE.contains(n))
}

pub fn parse_cpuset_name(x: &Value) -> Option<String> {
    x.as_str()
        .filter(|s| valid_cpuset_name(s))
        .map(str::to_string)
}

impl Settings {
    /// 缺省字段取默认值，出现但取值非法的字段报错
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let d = Settings::default();
        let bad = |key: &'static str| move || format!("字段 {key} 的取值无效");
        let path = |x: &Value| x.as_str().filter(|s| valid_path(s)).map(str::to_string);
        Ok(Self {
            web_enable: opt_field(v, "web_enable", Value::as_bool, bad("web_enable"))?
                .unwrap_or(d.web_enable),
            mode: opt_field(v, "mode", parse_mode, bad("mode"))?.unwrap_or(d.mode),
            check_interval: opt_field(v, "check_interval", parse_interval, bad("check_interval"))?
                .unwrap_or(d.check_interval),
            cpuset_name: opt_field(v, "cpuset_name", parse_cpuset_name, bad("cpuset_name"))?
                .unwrap_or(d.cpuset_name),
            config_file: opt_field(v, "config_file", path, bad("config_file"))?
                .unwrap_or(d.config_file),
        })
    }

    pub fn to_json(&self) -> Value {
        json!({
            "web_enable": self.web_enable,
            "mode": self.mode as u8,
            "check_interval": self.check_interval,
            "cpuset_name": self.cpuset_name,
            "config_file": self.config_file,
        })
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let text = serde_json::to_string_pretty(&self.to_json()).expect("设置可序列化");
        write_atomic(path, format!("{text}\n").as_bytes())
    }

    /// 读取设置文件；文件不存在时写入默认设置，内容损坏时报错而不覆盖
    pub fn load(path: &Path) -> Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let v: Value = serde_json::from_str(&text)
                    .map_err(|e| format!("{} 不是合法 JSON: {e}", path.display()))?;
                Self::from_json(&v).map_err(|e| format!("{}: {e}", path.display()))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let d = Self::default();
                // 默认设置落不了盘不影响运行，告警后继续
                match d.save(path) {
                    Ok(()) => println!("设置文件不存在，已写入默认设置: {}", path.display()),
                    Err(e) => eprintln!(
                        "警告: 写入默认设置 {} 失败 ({e})，本次运行不保存设置",
                        path.display()
                    ),
                }
                Ok(d)
            }
            Err(e) => Err(format!("读取 {} 失败: {e}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_take_defaults() {
        assert_eq!(
            Settings::from_json(&json!({})).unwrap(),
            Settings::default()
        );
        let s = Settings::from_json(&json!({"mode": 2, "check_interval": 5, "web_enable": true}))
            .unwrap();
        assert_eq!(
            (s.mode, s.check_interval, s.web_enable),
            (Mode::Proc, 5, true)
        );
    }

    #[test]
    fn invalid_values_are_rejected() {
        for v in [
            json!({"mode": 3}),
            json!({"mode": "auto"}),
            json!({"check_interval": 0}),
            json!({"check_interval": 3601}),
            json!({"cpuset_name": "a/b"}),
            json!({"cpuset_name": "a b"}),
            json!({"cpuset_name": ".."}),
            json!({"cpuset_name": "."}),
            json!({"config_file": ""}),
            json!({"web_enable": 1}),
        ] {
            assert!(Settings::from_json(&v).is_err(), "{v}");
        }
    }

    #[test]
    fn invalid_values_name_the_field() {
        for (v, key) in [
            (json!({"web_enable": "yes"}), "web_enable"),
            (json!({"mode": 9}), "mode"),
            (json!({"check_interval": 0}), "check_interval"),
            (json!({"cpuset_name": ""}), "cpuset_name"),
            (json!({"config_file": 3}), "config_file"),
            (json!({"mode": 9, "check_interval": 0}), "mode"),
        ] {
            assert_eq!(
                Settings::from_json(&v).unwrap_err(),
                format!("字段 {key} 的取值无效"),
                "{v}"
            );
        }
    }

    #[test]
    fn roundtrip_and_load_rules() {
        let dir = std::env::temp_dir().join(format!("appopt-settings-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("AppOpt.json");

        // 不存在时写入默认值
        assert_eq!(Settings::load(&path).unwrap(), Settings::default());
        assert!(path.exists());

        let s = Settings {
            web_enable: true,
            mode: Mode::Ebpf,
            check_interval: 7,
            cpuset_name: "X".into(),
            config_file: "/a/b.conf".into(),
        };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), s);

        // 损坏的文件不会被默认值覆盖
        fs::write(&path, "{oops").unwrap();
        assert!(Settings::load(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{oops");
        fs::remove_dir_all(&dir).unwrap();
    }
}
