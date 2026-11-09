//! 本机 HTTP 管理面板：静态页面 + JSON 接口，仅监听 127.0.0.1

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::{MAX_PKG_LEN, MAX_THREAD_LEN};
use crate::rule_edit::{self, EditError};
use crate::settings::{opt_field, parse_cpuset_name, parse_interval, parse_mode, valid_path};
use crate::shared::{ConfigPatch, RuleEditError, Shared};
use crate::{ebpf_mode, procfs};

pub const PORT: u16 = 8889;
const INDEX_HTML: &str = include_str!("../../web/index.html");

const MAX_HEAD: usize = 8192;
const MAX_BODY: usize = 16384;
const MAX_CONNECTIONS: usize = 16;
const SUGGEST_LIMIT: usize = 20;

/// 在 127.0.0.1:PORT 启动服务线程
pub fn start(shared: Arc<Shared>) -> io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", PORT))?;
    shared.set_web_running();
    println!("Web: 前端已启用 http://127.0.0.1:{PORT}/");
    thread::Builder::new()
        .name("WebAccept".into())
        .spawn(move || {
            let active = Arc::new(AtomicUsize::new(0));
            for stream in listener.incoming().flatten() {
                // 超出上限的连接直接关闭，避免本机其他应用耗尽守护进程的线程与句柄
                if active.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
                    active.fetch_sub(1, Ordering::AcqRel);
                    continue;
                }
                let slot = Slot(active.clone());
                let shared = shared.clone();
                thread::spawn(move || {
                    let _slot = slot;
                    serve(&shared, stream);
                });
            }
        })?;
    Ok(())
}

/// 并发连接占位，释放时归还名额
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Request {
    method: String,
    path: String,
    host: String,
    origin: String,
    fetch_site: String,
    content_type: String,
    body: Vec<u8>,
    keep_alive: bool,
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

fn serve(shared: &Shared, stream: TcpStream) {
    // 客户端断开等连接级错误无需处理，结束该连接即可
    let _ = (|| -> io::Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        stream.set_nodelay(true)?;
        let mut writer = stream.try_clone()?;
        let mut reader = BufReader::new(stream);
        while let Some(req) = read_request(&mut reader) {
            send(&mut writer, &dispatch(shared, &req), req.keep_alive)?;
            if !req.keep_alive {
                break;
            }
        }
        Ok(())
    })();
}

/// 读一行，总量受 budget 限制；超限或连接关闭返回 None
fn read_line_limited(reader: &mut impl BufRead, budget: &mut usize) -> Option<Vec<u8>> {
    let mut line = Vec::new();
    let n = reader
        .take(*budget as u64 + 1)
        .read_until(b'\n', &mut line)
        .ok()?;
    if n == 0 || n > *budget {
        return None;
    }
    *budget -= n;
    Some(line)
}

fn read_request(reader: &mut impl BufRead) -> Option<Request> {
    let mut budget = MAX_HEAD;
    let mut head = String::new();
    loop {
        let line = read_line_limited(reader, &mut budget)?;
        if line == b"\r\n" || line == b"\n" {
            break;
        }
        head.push_str(&String::from_utf8_lossy(&line));
    }

    let mut lines = head.lines();
    let mut parts = lines.next()?.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.split('?').next().unwrap_or("").to_string();
    let version = parts.next().unwrap_or("HTTP/1.1");

    let (mut host, mut origin, mut fetch_site, mut content_type, mut conn) = Default::default();
    let mut len = 0usize;
    for h in lines {
        let Some((k, v)) = h.split_once(':') else {
            continue;
        };
        let v = v.trim().to_ascii_lowercase();
        match k.trim().to_ascii_lowercase().as_str() {
            "host" => host = v,
            "origin" => origin = v,
            "sec-fetch-site" => fetch_site = v,
            "content-type" => content_type = v,
            "content-length" => len = v.parse().unwrap_or(usize::MAX),
            "connection" => conn = v,
            // 不支持分块编码
            "transfer-encoding" => return None,
            _ => {}
        }
    }
    if len > MAX_BODY {
        return None;
    }

    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok()?;
    let keep_alive = if version == "HTTP/1.0" {
        conn.contains("keep-alive")
    } else {
        !conn.contains("close")
    };
    Some(Request {
        method,
        path,
        host,
        origin,
        fetch_site,
        content_type,
        body,
        keep_alive,
    })
}

fn send(out: &mut impl Write, resp: &Response, keep_alive: bool) -> io::Result<()> {
    let reason = match resp.status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    };
    write!(
        out,
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: {}\r\n\r\n",
        resp.status,
        resp.content_type,
        resp.body.len(),
        if keep_alive { "keep-alive" } else { "close" }
    )?;
    out.write_all(&resp.body)?;
    out.flush()
}

/// 仅接受本机页面发起的请求：Host 必须是回环地址，带 Origin 时须同源，POST 必须是 JSON
fn request_allowed(req: &Request) -> bool {
    let host = req
        .host
        .strip_suffix(&format!(":{PORT}"))
        .unwrap_or(&req.host);
    matches!(host, "127.0.0.1" | "localhost")
        && (req.origin.is_empty()
            || req.origin == "null"
            || matches!(req.fetch_site.as_str(), "" | "none" | "same-origin"))
        && (req.method != "POST" || req.content_type.starts_with("application/json"))
}

struct ApiError(u16, String);

type ApiResult = Result<Value, ApiError>;

fn api_err(status: u16, msg: impl Into<String>) -> ApiError {
    ApiError(status, msg.into())
}

fn bad_request(msg: impl Into<String>) -> ApiError {
    api_err(400, msg)
}

fn ok() -> ApiResult {
    Ok(json!({ "ok": true }))
}

fn json_response(status: u16, body: Value) -> Response {
    Response {
        status,
        content_type: "application/json",
        body: body.to_string().into_bytes(),
    }
}

fn dispatch(shared: &Shared, req: &Request) -> Response {
    if !request_allowed(req) {
        return json_response(403, json!({ "ok": false, "err": "forbidden" }));
    }
    if req.method == "GET" && matches!(req.path.as_str(), "/" | "/index.html") {
        return Response {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: INDEX_HTML.as_bytes().to_vec(),
        };
    }

    let result = match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/api/status") => status(shared),
        ("GET", "/api/rules") => Ok(rules(shared)),
        ("GET", "/api/config") => Ok(config(shared)),
        ("POST", "/api/rule") => json_body(req).and_then(|v| rule_set(shared, &v)),
        ("POST", "/api/rule/del") => json_body(req).and_then(|v| rule_del(shared, &v)),
        ("POST", "/api/rule/rename") => json_body(req).and_then(|v| rule_rename(shared, &v)),
        ("POST", "/api/config") => json_body(req).and_then(|v| config_set(shared, &v)),
        ("POST", "/api/suggest") => json_body(req).and_then(|v| suggest(&v)),
        _ => Err(api_err(404, "not found")),
    };
    match result {
        Ok(body) => json_response(200, body),
        Err(ApiError(status, msg)) => json_response(status, json!({ "ok": false, "err": msg })),
    }
}

fn json_body(req: &Request) -> Result<Value, ApiError> {
    serde_json::from_slice(&req.body).map_err(|_| bad_request("请求体不是合法 JSON"))
}

fn engine_label(ebpf: bool) -> &'static str {
    if ebpf { "ebpf" } else { "proc" }
}

fn status(shared: &Shared) -> ApiResult {
    let (cfg, stats, settings) = (shared.config(), shared.stats(), shared.settings());
    let topo = &cfg.topo;
    let total_procs =
        procfs::thread_count().map_err(|e| api_err(500, format!("sysinfo 失败: {e}")))?;
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "mode": engine_label(stats.ebpf),
        "uptime": shared.started.elapsed().as_secs(),
        "rules": cfg.rule_count(),
        "pkgs": cfg.groups().len(),
        "parse_fail": cfg.parse_fails,
        "hit_pkgs": stats.hit_pkgs,
        "threads": stats.threads,
        "total_procs": total_procs,
        "interval": settings.check_interval,
        "e_core": topo.e_core.to_string(),
        "p_core": topo.p_core.to_string(),
        "hp_core": topo.hp_core.to_string(),
        "all_core": topo.present.to_string(),
        "cores": topo.present.len(),
        "cpuset_enabled": cfg.cpusets.is_some(),
    }))
}

fn rules(shared: &Shared) -> Value {
    let cfg = shared.config();
    let groups: Vec<Value> = cfg
        .groups()
        .iter()
        .map(|g| {
            let items: Vec<Value> = g
                .items
                .iter()
                .map(|r| json!({ "thread": r.thread, "spec": cfg.topo.spec_name(&r.cpus) }))
                .collect();
            json!({ "pkg": &*g.pkg, "items": items })
        })
        .collect();
    json!({ "rules": groups })
}

fn config(shared: &Shared) -> Value {
    let (cfg, stats, settings) = (shared.config(), shared.stats(), shared.settings());
    json!({
        "mode": settings.mode as u8,
        "mode_active": engine_label(stats.ebpf),
        "ebpf_available": ebpf_mode::available(),
        "interval": settings.check_interval,
        "cpuset_name": settings.cpuset_name,
        "config_file": settings.config_file,
        "cpuset_enabled": cfg.cpusets.is_some(),
    })
}

fn config_set(shared: &Shared, v: &Value) -> ApiResult {
    let bad = |msg: &'static str| move || bad_request(msg);
    let mode = opt_field(v, "mode", parse_mode, bad("无效的工作模式"))?;
    let interval = opt_field(v, "interval", parse_interval, bad("间隔需在 1-3600 秒之间"))?;
    let cpuset_name = opt_field(
        v,
        "cpuset_name",
        parse_cpuset_name,
        bad("无效的 cpuset 目录名"),
    )?;
    let config_file = opt_field(
        v,
        "config_file",
        |x| x.as_str().filter(|p| config_path_ok(p)).map(str::to_string),
        bad("无效的配置文件路径，文件名需以 .conf 结尾"),
    )?;

    shared
        .patch(ConfigPatch {
            mode,
            interval,
            cpuset_name,
            config_file,
        })
        .map_err(|e| api_err(500, format!("应用设置失败: {e}")))?;
    ok()
}

/// 经 web 切换的规则文件必须是 .conf，避免守护进程（root）被借来改写其他类型的文件
fn config_path_ok(p: &str) -> bool {
    valid_path(p) && Path::new(p).extension().is_some_and(|e| e == "conf")
}

/// 名称校验：非空、长度受限、无控制字符，且不含会被当作注释的 `#` 与 `//`
fn token_ok(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() < max
        && !s.bytes().any(|b| b < 0x20 || b == 0x7f)
        && !s.contains('#')
        && !s.contains("//")
}

/// `pkg{...}` 形态在文件里会被解析成单行线程规则，不能作为包名
fn pkg_shape_ok(pkg: &str) -> bool {
    !(pkg.contains('{') && pkg.ends_with('}'))
}

fn edit_failure(e: RuleEditError, not_found: &str) -> ApiError {
    match e {
        RuleEditError::Edit(EditError::NotFound) => api_err(404, not_found),
        RuleEditError::Edit(EditError::Conflict) => api_err(409, "目标包名已存在规则"),
        RuleEditError::Edit(EditError::Malformed) => {
            api_err(409, "配置文件存在未闭合块，请修复后重试")
        }
        RuleEditError::Io(e) => api_err(500, format!("配置文件读写失败: {e}")),
    }
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v[key].as_str().map(str::trim)
}

fn rule_set(shared: &Shared, v: &Value) -> ApiResult {
    let (Some(pkg), Some(cpus)) = (str_field(v, "pkg"), str_field(v, "cpus")) else {
        return Err(bad_request("缺少 pkg 或 cpus 字段"));
    };
    let thread = str_field(v, "thread").unwrap_or("");
    if !token_ok(pkg, MAX_PKG_LEN) || (!thread.is_empty() && !token_ok(thread, MAX_THREAD_LEN)) {
        return Err(bad_request("名称含有非法字符"));
    }
    if thread.is_empty() && !pkg_shape_ok(pkg) {
        return Err(bad_request(
            "包名含 { 且以 } 结尾时不支持包级规则，可改用线程规则",
        ));
    }
    if cpus.len() >= 64 {
        return Err(bad_request("CPU 规格过长"));
    }
    shared
        .config()
        .topo
        .parse_spec(cpus)
        .map_err(|e| bad_request(e.to_string()))?;

    shared
        .edit_rules(|lines| rule_edit::upsert(lines, pkg, thread, cpus))
        .map_err(|e| edit_failure(e, "规则不存在"))?;
    ok()
}

fn rule_del(shared: &Shared, v: &Value) -> ApiResult {
    let Some(pkg) = str_field(v, "pkg") else {
        return Err(bad_request("缺少 pkg 字段"));
    };
    let thread = str_field(v, "thread").unwrap_or("");
    let all = v["all"].as_bool().unwrap_or(false);
    shared
        .edit_rules(|lines| {
            if all {
                rule_edit::delete_pkg(lines, pkg)
            } else {
                rule_edit::delete(lines, pkg, thread)
            }
        })
        .map_err(|e| edit_failure(e, "规则不存在"))?;
    ok()
}

fn rule_rename(shared: &Shared, v: &Value) -> ApiResult {
    let (Some(old), Some(new)) = (str_field(v, "old"), str_field(v, "new")) else {
        return Err(bad_request("缺少 old 或 new 字段"));
    };
    if !token_ok(old, MAX_PKG_LEN) || !token_ok(new, MAX_PKG_LEN) {
        return Err(bad_request("名称含有非法字符"));
    }
    if !pkg_shape_ok(new) {
        return Err(bad_request("包名含 { 且以 } 结尾时不可作为重命名目标"));
    }
    if old == new {
        return ok();
    }
    shared
        .edit_rules(|lines| rule_edit::rename(lines, old, new))
        .map_err(|e| edit_failure(e, "原包名不存在"))?;
    ok()
}

/// 输入补全：不带 pkg 时补全包名，带 pkg 时补全该进程的线程名
fn suggest(v: &Value) -> ApiResult {
    let q = str_field(v, "q").unwrap_or("");
    if q.len() > 64 {
        return Err(bad_request("q 过长"));
    }
    let io_err = |e: io::Error| api_err(500, format!("读取 /proc 失败: {e}"));
    let counts = match str_field(v, "pkg").filter(|p| !p.is_empty()) {
        None => package_counts().map_err(io_err)?,
        Some(pkg) => {
            if !token_ok(pkg, MAX_PKG_LEN) {
                return Err(bad_request("名称含有非法字符"));
            }
            thread_counts(pkg).map_err(io_err)?
        }
    };
    Ok(json!({ "ok": true, "list": rank_top(counts, q) }))
}

/// 已安装应用包名与正在运行的进程名，值为进程数
fn package_counts() -> io::Result<BTreeMap<String, usize>> {
    // 受 SELinux 限制读不到已安装列表时，只用运行中的进程名补全
    let installed = match fs::read_dir("/data/data") {
        Ok(dir) => dir,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            return running_counts(BTreeMap::new());
        }
        Err(e) => return Err(e),
    };
    let counts: BTreeMap<String, usize> = installed
        .flatten()
        .map(|d| d.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains('.') && !n.starts_with('.'))
        .map(|n| (n, 0))
        .collect();
    running_counts(counts)
}

/// 在 counts 上累加正在运行的进程名
fn running_counts(mut counts: BTreeMap<String, usize>) -> io::Result<BTreeMap<String, usize>> {
    for pid in procfs::pids()? {
        if let Some(name) = procfs::cmdline(pid).filter(|n| n.contains('.')) {
            *counts.entry(name).or_default() += 1;
        }
    }
    Ok(counts)
}

/// 指定进程名下各线程名出现的次数
fn thread_counts(pkg: &str) -> io::Result<BTreeMap<String, usize>> {
    let mut counts = BTreeMap::new();
    for pid in procfs::pids()? {
        if procfs::cmdline(pid).as_deref() != Some(pkg) {
            continue;
        }
        for tid in procfs::tids(pid).unwrap_or_default() {
            if let Some(comm) = procfs::comm(tid) {
                *counts.entry(comm).or_default() += 1;
            }
        }
    }
    Ok(counts)
}

/// 前缀匹配优先于子串匹配，同级按出现次数降序，取前 SUGGEST_LIMIT 个名称
fn rank_top(counts: BTreeMap<String, usize>, query: &str) -> Vec<String> {
    let q = query.to_ascii_lowercase();
    let mut ranked: Vec<(u8, Reverse<usize>, String)> = counts
        .into_iter()
        .filter_map(|(name, n)| {
            let lower = name.to_ascii_lowercase();
            let tier = if lower.starts_with(&q) {
                0
            } else if lower.contains(&q) {
                1
            } else {
                return None;
            };
            Some((tier, Reverse(n), name))
        })
        .collect();
    ranked.sort_unstable();
    ranked.truncate(SUGGEST_LIMIT);
    ranked.into_iter().map(|(_, _, name)| name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse(raw: &str) -> Option<Request> {
        read_request(&mut Cursor::new(raw.as_bytes().to_vec()))
    }

    #[test]
    fn parses_post_with_body() {
        let req = parse("POST /api/rule?x=1 HTTP/1.1\r\nHost: 127.0.0.1:8889\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}").unwrap();
        assert_eq!(
            (req.method.as_str(), req.path.as_str(), req.body.as_slice()),
            ("POST", "/api/rule", &b"{}"[..])
        );
        assert!(req.keep_alive);
        assert!(request_allowed(&req));
    }

    #[test]
    fn rejects_malformed_requests() {
        assert!(parse("POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n").is_none());
        assert!(parse("POST / HTTP/1.1\r\nContent-Length: 99999\r\n\r\n").is_none());
        assert!(parse("POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\nshort").is_none());
        assert!(parse("").is_none());
        // 无换行的超长头部不会无限读取
        assert!(parse(&"a".repeat(MAX_HEAD * 4)).is_none());
        let many = format!("GET / HTTP/1.1\r\n{}\r\n", "X: y\r\n".repeat(MAX_HEAD));
        assert!(parse(&many).is_none());
    }

    #[test]
    fn keep_alive_rules() {
        assert!(
            !parse("GET / HTTP/1.1\r\nConnection: close\r\n\r\n")
                .unwrap()
                .keep_alive
        );
        assert!(!parse("GET / HTTP/1.0\r\n\r\n").unwrap().keep_alive);
        assert!(
            parse("GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n")
                .unwrap()
                .keep_alive
        );
    }

    #[test]
    fn origin_and_host_checks() {
        let ok = |raw: &str| request_allowed(&parse(raw).unwrap());
        assert!(ok("GET / HTTP/1.1\r\nHost: localhost:8889\r\n\r\n"));
        assert!(!ok("GET / HTTP/1.1\r\nHost: evil.example\r\n\r\n"));
        assert!(!ok(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:8889\r\nOrigin: http://evil.example\r\nSec-Fetch-Site: cross-site\r\n\r\n"
        ));
        assert!(ok(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:8889\r\nOrigin: http://127.0.0.1:8889\r\nSec-Fetch-Site: same-origin\r\n\r\n"
        ));
        assert!(!ok(
            "POST / HTTP/1.1\r\nHost: 127.0.0.1:8889\r\nContent-Type: text/plain\r\n\r\n"
        ));
    }

    #[test]
    fn suggestions_prefer_prefix_then_frequency() {
        let counts: BTreeMap<String, usize> = [
            ("com.example.app", 1),
            ("org.com.other", 9),
            ("com.zzz", 3),
            ("unrelated", 5),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        assert_eq!(
            rank_top(counts.clone(), "COM"),
            ["com.zzz", "com.example.app", "org.com.other"]
        );
        assert_eq!(rank_top(counts, "none"), Vec::<String>::new());
    }

    #[test]
    fn web_may_only_switch_to_conf_files() {
        assert!(config_path_ok("/sdcard/applist.conf"));
        assert!(config_path_ok("./applist.conf"));
        assert!(!config_path_ok("/data/adb/service.d/x.sh"));
        assert!(!config_path_ok("/data/adb/modules/m/service.sh"));
        assert!(!config_path_ok("/etc/passwd"));
        assert!(!config_path_ok("conf"));
        assert!(!config_path_ok(""));
    }

    #[test]
    fn name_rules() {
        assert!(token_ok("com.a.b", MAX_PKG_LEN));
        assert!(!token_ok("a#b", MAX_PKG_LEN));
        assert!(!token_ok("a//b", MAX_PKG_LEN));
        assert!(!token_ok("a\tb\u{1}", MAX_PKG_LEN));
        assert!(!token_ok(&"x".repeat(MAX_THREAD_LEN), MAX_THREAD_LEN));
        assert!(!pkg_shape_ok("a{T}"));
        assert!(pkg_shape_ok("a{T"));
    }

    fn shared_fixture(name: &str) -> (Shared, std::path::PathBuf) {
        use crate::cpuset::{CpuSet, CpuTopology};
        use crate::settings::Settings;
        let dir = std::env::temp_dir().join(format!("appopt-web-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("applist.conf");
        fs::write(&conf, "a=0\n").unwrap();
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
    fn config_set_validates_each_field() {
        let (s, dir) = shared_fixture("cfgset");
        let before = s.settings();
        for (body, msg) in [
            (json!({"mode": 3}), "无效的工作模式"),
            (json!({"mode": "1"}), "无效的工作模式"),
            (json!({"interval": 0}), "间隔需在 1-3600 秒之间"),
            (json!({"interval": 3601}), "间隔需在 1-3600 秒之间"),
            (json!({"interval": "2"}), "间隔需在 1-3600 秒之间"),
            (json!({"cpuset_name": "a/b"}), "无效的 cpuset 目录名"),
            (json!({"cpuset_name": 5}), "无效的 cpuset 目录名"),
            (
                json!({"config_file": "/x/y.txt"}),
                "无效的配置文件路径，文件名需以 .conf 结尾",
            ),
            (
                json!({"config_file": 1}),
                "无效的配置文件路径，文件名需以 .conf 结尾",
            ),
            (json!({"mode": 1, "interval": 0}), "间隔需在 1-3600 秒之间"),
        ] {
            match config_set(&s, &body) {
                Err(ApiError(status, m)) => assert_eq!((status, m.as_str()), (400, msg), "{body}"),
                Ok(_) => panic!("应拒绝 {body}"),
            }
        }
        assert_eq!(s.settings(), before);

        // 缺省与 null 视为未提供
        assert!(config_set(&s, &json!({"mode": null, "cpuset_name": null})).is_ok());
        assert_eq!(s.settings(), before);
        assert!(config_set(&s, &json!({"mode": 2, "interval": 7})).is_ok());
        let after = s.settings();
        assert_eq!(
            (after.mode, after.check_interval),
            (crate::settings::Mode::Proc, 7)
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
