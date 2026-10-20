//! 浏览器登录通道（参考 V8.3 老项目「内置浏览器短信登录」）。
//!
//! 背景（老项目实测结论，见 `网盘管理/v8_3/docs/实验_纯HTTP登录.md`）：
//! * 百度现在的短信登录是登录页里的风控组件（ARMOR / 跨域 iframe / 设备指纹），
//!   老接口 `?getsmscode` 已 404、`?regphonesend` 回 200 但不真发短信 —— 纯 HTTP 走不通；
//! * 夸克 CAS 的短信通道同样没有稳定公开接口；
//! * 老项目的做法：**在程序内置浏览器里打开登录页**，用户在网页里点「短信登录」，
//!   填手机号 + 验证码完成登录，登录完自动取走完整会话。
//!
//! 本模块用等价方案：启动本机 Chrome/Edge/Chromium（独立临时配置 + 远程调试端口），
//! 用户在浏览器里用短信（也可扫码/密码）登录；程序通过 CDP（Chrome DevTools Protocol）
//! 读取该会话的 Cookie，交给各适配器已有的 `login_cookie` 校验并落盘。
//! 浏览器关闭后临时配置目录会删除，会话只会通过适配器保存。
//!
//! 注意：本模块不做任何风控绕过，只是把用户在浏览器里完成的正常登录结果取回。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{json, Value};
use wp_core::model::DriveKind;

/// 浏览器登录过程中的状态回调（在后台线程里调用）。
#[derive(Debug, Clone)]
pub enum BrowserLoginEvent {
    /// 已启动浏览器，等待用户在网页里登录
    Started(String),
    /// 已取到完整 Cookie 串（`name=value; ...`），交给适配器 Cookie 登录
    Cookies(String),
    /// 失败（未找到浏览器 / 超时 / 用户关闭 / CDP 错误）
    Failed(String),
}

/// 状态回调：由 GUI 传一个把事件塞进 Event 通道的闭包。
pub type Sink = Box<dyn Fn(BrowserLoginEvent) + Send + 'static>;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 登录页地址。
pub fn login_url(kind: DriveKind) -> &'static str {
    match kind {
        DriveKind::Baidu => "https://pan.baidu.com/",
        DriveKind::Quark => "https://pan.quark.cn/",
        _ => "https://pan.baidu.com/",
    }
}

/// 该网盘登录后必须取到的 Cookie 键（用于判断「用户已登录完成」）。
pub fn required_cookie_keys(kind: DriveKind) -> &'static [&'static str] {
    match kind {
        // 百度适配器 Session::valid 要求 BDUSS + STOKEN（写操作/刷新还需要 bdstoken，由适配器自己取）
        DriveKind::Baidu => &["BDUSS", "STOKEN"],
        // 夸克 Cookie 登录校验要求 __kps + __uid（下载直链建议再有 __puus）
        DriveKind::Quark => &["__kps", "__uid"],
        _ => &[],
    }
}

/// Cookie 属于哪个域的判断（登录后 Cookie 会散落在多个子域）。
pub fn cookie_domain_matches(kind: DriveKind, domain: &str) -> bool {
    let d = domain.trim_start_matches('.').to_ascii_lowercase();
    match kind {
        DriveKind::Baidu => d == "baidu.com" || d.ends_with(".baidu.com"),
        DriveKind::Quark => d == "quark.cn" || d.ends_with(".quark.cn"),
        _ => false,
    }
}

/// 浏览器可执行文件候选（`WP_LOGIN_BROWSER` 环境变量优先）。
pub fn browser_candidates() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("WP_LOGIN_BROWSER") {
        if !p.trim().is_empty() {
            out.push(PathBuf::from(p));
        }
    }
    #[cfg(target_os = "windows")]
    {
        let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
        let pf86 =
            std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| r"C:\Program Files (x86)".into());
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
        for base in [
            format!(r"{pf}\Google\Chrome\Application\chrome.exe"),
            format!(r"{pf86}\Google\Chrome\Application\chrome.exe"),
            format!(r"{local}\Google\Chrome\Application\chrome.exe"),
            format!(r"{pf}\Microsoft\Edge\Application\msedge.exe"),
            format!(r"{pf86}\Microsoft\Edge\Application\msedge.exe"),
            format!(r"{local}\Microsoft\Edge\Application\msedge.exe"),
            format!(r"{pf}\Chromium\Application\chrome.exe"),
            format!(r"{pf}\BraveSoftware\Brave-Browser\Application\brave.exe"),
        ] {
            out.push(PathBuf::from(base));
        }
    }
    #[cfg(target_os = "macos")]
    {
        for p in [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        ] {
            out.push(PathBuf::from(p));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for p in [
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/usr/bin/microsoft-edge",
            "/usr/bin/microsoft-edge-stable",
            "/usr/bin/brave-browser",
            "/snap/bin/chromium",
        ] {
            out.push(PathBuf::from(p));
        }
    }
    // 再从 PATH 里找（覆盖用户自定义安装/不同发行版命名）
    if let Ok(path) = std::env::var("PATH") {
        let names = if cfg!(target_os = "windows") {
            vec!["chrome.exe", "msedge.exe", "chromium.exe"]
        } else {
            vec![
                "google-chrome",
                "google-chrome-stable",
                "chromium",
                "chromium-browser",
                "microsoft-edge",
                "microsoft-edge-stable",
                "brave-browser",
            ]
        };
        for dir in path.split(if cfg!(target_os = "windows") {
            ';'
        } else {
            ':'
        }) {
            for name in &names {
                let p = Path::new(dir).join(name);
                if p.is_file() {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// 找到第一个可用的 Chrome/Edge/Chromium。
pub fn find_browser() -> Option<PathBuf> {
    browser_candidates().into_iter().find(|p| p.is_file())
}

/// 找空闲的本地 TCP 端口。
fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
        .unwrap_or(9222)
}

// ───────────────────────── CDP：HTTP/WS 最简客户端 ─────────────────────────

/// `GET http://127.0.0.1:<port><path>`，返回 JSON。
fn cdp_http_json(port: u16, path: &str) -> std::result::Result<Value, String> {
    let mut s =
        TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("连接调试端口失败：{e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    s.set_write_timeout(Some(Duration::from_secs(5))).ok();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| format!("请求调试端口失败：{e}"))?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf)
        .map_err(|e| format!("读取调试端口失败：{e}"))?;
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "调试端口响应缺少响应头".to_string())?;
    if !head.starts_with("HTTP/1.1 200") && !head.starts_with("HTTP/1.0 200") {
        return Err(format!(
            "调试端口返回非 200：{}",
            head.lines().next().unwrap_or("")
        ));
    }
    let lower = head.to_ascii_lowercase();
    let json_text = if lower.contains("transfer-encoding: chunked") {
        decode_chunked(body)?
    } else {
        body.to_string()
    };
    serde_json::from_str(json_text.trim()).map_err(|e| format!("调试端口 JSON 解析失败：{e}"))
}

fn decode_chunked(body: &str) -> std::result::Result<String, String> {
    let mut out = String::new();
    let mut rest = body;
    loop {
        let (line, after) = rest
            .split_once("\r\n")
            .ok_or_else(|| "chunked 响应不完整".to_string())?;
        let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or("0"), 16)
            .map_err(|e| format!("chunked 长度非法：{e}"))?;
        if size == 0 {
            break;
        }
        if after.len() < size {
            return Err("chunked 数据不完整".into());
        }
        out.push_str(&after[..size]);
        rest = after[size..].trim_start_matches("\r\n");
    }
    Ok(out)
}

fn base64_simple(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// 连接页面的 `webSocketDebuggerUrl`。
fn ws_connect(ws_url: &str) -> std::result::Result<TcpStream, String> {
    let rest = ws_url
        .strip_prefix("ws://")
        .ok_or_else(|| format!("不支持的调试地址：{ws_url}"))?;
    let (host_port, path) = match rest.split_once('/') {
        Some((hp, p)) => (hp, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let (host, port) = host_port
        .rsplit_once(':')
        .ok_or_else(|| "调试地址缺少端口".to_string())?;
    let port: u16 = port.parse().map_err(|e| format!("调试端口非法：{e}"))?;
    let mut s = TcpStream::connect((host, port)).map_err(|e| format!("连接 CDP 失败：{e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    s.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let key = base64_simple(
        format!(
            "{}-{}-{}",
            std::process::id(),
            now_secs(),
            std::time::Instant::now().elapsed().as_nanos()
        )
        .as_bytes(),
    );
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
    .map_err(|e| format!("发送 WS 握手失败：{e}"))?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while head.len() < 8192 {
        let n = s
            .read(&mut byte)
            .map_err(|e| format!("读取 WS 握手失败：{e}"))?;
        if n == 0 {
            break;
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&head);
    if !text.starts_with("HTTP/1.1 101") {
        return Err(format!(
            "WS 握手被拒绝：{}",
            text.lines().next().unwrap_or("")
        ));
    }
    Ok(s)
}

fn ws_write_text(s: &mut TcpStream, text: &str) -> std::result::Result<(), String> {
    let payload = text.as_bytes();
    let mut frame = Vec::with_capacity(payload.len() + 14);
    frame.push(0x81); // FIN + text
    let len = payload.len();
    if len < 126 {
        frame.push(0x80 | len as u8);
    } else if len <= u16::MAX as usize {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    let mask = [0x12u8, 0x34, 0x56, 0x78];
    frame.extend_from_slice(&mask);
    for (i, b) in payload.iter().enumerate() {
        frame.push(b ^ mask[i % 4]);
    }
    s.write_all(&frame)
        .map_err(|e| format!("发送 CDP 命令失败：{e}"))
}

fn read_exact_n(s: &mut TcpStream, n: usize) -> std::result::Result<Vec<u8>, String> {
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf)
        .map_err(|e| format!("读取 CDP 响应失败：{e}"))?;
    Ok(buf)
}

/// 读一条完整的 WS 文本消息（拼接分片；处理 ping/pong）。
fn ws_read_text(s: &mut TcpStream) -> std::result::Result<Option<String>, String> {
    let mut out: Vec<u8> = Vec::new();
    loop {
        let h = read_exact_n(s, 2)?;
        let fin = h[0] & 0x80 != 0;
        let opcode = h[0] & 0x0f;
        let masked = h[1] & 0x80 != 0;
        let mut len = (h[1] & 0x7f) as usize;
        if len == 126 {
            let b = read_exact_n(s, 2)?;
            len = u16::from_be_bytes([b[0], b[1]]) as usize;
        } else if len == 127 {
            let b = read_exact_n(s, 8)?;
            let mut a = [0u8; 8];
            a.copy_from_slice(&b);
            len = u64::from_be_bytes(a) as usize;
        }
        let mask = if masked {
            Some(read_exact_n(s, 4)?)
        } else {
            None
        };
        let mut payload = read_exact_n(s, len)?;
        if let Some(mask) = &mask {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
        }
        match opcode {
            0x1 | 0x0 => {
                out.extend_from_slice(&payload);
                if fin {
                    return Ok(Some(String::from_utf8_lossy(&out).to_string()));
                }
            }
            0x8 => return Ok(None), // close
            0x9 => {
                // ping → pong（带 mask）
                let mut frame = vec![0x8a];
                let len = payload.len();
                frame.push(0x80 | len.min(125) as u8);
                let mask = [1u8, 2, 3, 4];
                frame.extend_from_slice(&mask);
                for (i, b) in payload.iter().take(125).enumerate() {
                    frame.push(b ^ mask[i % 4]);
                }
                let _ = s.write_all(&frame);
            }
            _ => {} // pong / binary 忽略
        }
    }
}

/// 发一条 CDP 命令并等对应 id 的响应。
fn cdp_call(
    s: &mut TcpStream,
    id: u64,
    method: &str,
    params: Value,
) -> std::result::Result<Value, String> {
    let msg = json!({"id": id, "method": method, "params": params});
    ws_write_text(s, &msg.to_string())?;
    for _ in 0..50 {
        let Some(text) = ws_read_text(s)? else {
            return Err("CDP 连接已关闭".into());
        };
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("CDP 响应非法：{e}"))?;
        if v.get("id").and_then(Value::as_u64) == Some(id) {
            if let Some(err) = v.get("error") {
                return Err(format!("CDP 命令失败：{err}"));
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }
    Err("CDP 响应超时".into())
}

// ───────────────────────── Cookie 解析 / 过滤 ─────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct CdpCookie {
    name: String,
    value: String,
    #[serde(default)]
    domain: String,
}

/// 从 `result.cookies` 解析 Cookie 列表。
fn parse_cookies(result: &Value) -> Vec<CdpCookie> {
    result
        .get("cookies")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// 把 Cookie 过滤/去重成浏览器登录用的一行 Cookie 串；缺少必需键返回 None。
fn format_cookie_header(cookies: &[CdpCookie], kind: DriveKind) -> Option<String> {
    use std::collections::HashMap;
    // 同名 Cookie 选「域更具体」的（例如 pan.baidu.com 优于 .baidu.com），空值忽略
    let mut best: HashMap<String, (i32, String)> = HashMap::new();
    for c in cookies {
        if !cookie_domain_matches(kind, &c.domain) || c.name.is_empty() || c.value.is_empty() {
            continue;
        }
        let score = match kind {
            DriveKind::Baidu => {
                let d = c.domain.trim_start_matches('.');
                if d.starts_with("pan.baidu.com") {
                    30
                } else if d.starts_with("passport.baidu.com") {
                    20
                } else {
                    10
                }
            }
            DriveKind::Quark => {
                let d = c.domain.trim_start_matches('.');
                if d.starts_with("pan.quark.cn") {
                    30
                } else if d.starts_with("uop.quark.cn") {
                    20
                } else {
                    10
                }
            }
            _ => 10,
        };
        match best.get(&c.name) {
            Some((old, _)) if *old >= score => {}
            _ => {
                best.insert(c.name.clone(), (score, c.value.clone()));
            }
        }
    }
    let required = required_cookie_keys(kind);
    if required.iter().any(|k| !best.contains_key(*k)) {
        return None;
    }
    let mut parts: Vec<String> = best.iter().map(|(k, (_, v))| format!("{k}={v}")).collect();
    parts.sort();
    Some(parts.join("; "))
}

/// 若调试端口上已有登录后的必需 Cookie，返回 Cookie 串。
fn poll_cookies(ws_url: &str, kind: DriveKind) -> std::result::Result<Option<String>, String> {
    let mut s = ws_connect(ws_url)?;
    let result = cdp_call(&mut s, 1, "Network.getAllCookies", json!({}))?;
    let cookies = parse_cookies(&result);
    Ok(format_cookie_header(&cookies, kind))
}

/// 等调试端口出现页面 target，返回 (页面 URL, webSocketDebuggerUrl)。
fn wait_page_target(port: u16, timeout: Duration) -> std::result::Result<(String, String), String> {
    let start = Instant::now();
    let mut last_err = String::new();
    while start.elapsed() < timeout {
        match cdp_http_json(port, "/json/list") {
            Ok(Value::Array(list)) => {
                for t in list {
                    let ty = t.get("type").and_then(Value::as_str).unwrap_or("");
                    let ws = t
                        .get("webSocketDebuggerUrl")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if ty == "page" && !ws.is_empty() {
                        return Ok((
                            t.get("url")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            ws.to_string(),
                        ));
                    }
                }
                last_err = "调试端口暂无页面 target".into();
            }
            Ok(_) => last_err = "调试端口返回非页面列表".into(),
            Err(e) => last_err = e,
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(format!("等待浏览器调试端口超时：{last_err}"))
}

// ───────────────────────── 对外入口 ─────────────────────────

/// 在后台线程里运行一次浏览器登录（由 GUI 调用）。
///
/// 流程：启动浏览器 → 等 CDP 端口 → 每 2 秒查一次 Cookie → 必需键齐全后回调 Cookies → 关闭浏览器。
pub fn run(kind: DriveKind, drive_id: &str, data_root: &Path, cancel: Arc<AtomicBool>, sink: Sink) {
    let Some(browser) = find_browser() else {
        sink(BrowserLoginEvent::Failed(
            "没找到 Chrome / Edge / Chromium。请安装任一浏览器，或设置环境变量 WP_LOGIN_BROWSER 指向浏览器可执行文件；也可改用「Cookie」标签手动导入。"
                .into(),
        ));
        return;
    };
    let port = free_port();
    let profile = data_root
        .join("浏览器登录")
        .join(format!("{}-{}", drive_id, now_secs()));
    if let Err(e) = std::fs::create_dir_all(&profile) {
        sink(BrowserLoginEvent::Failed(format!(
            "创建临时浏览器配置失败：{e}"
        )));
        return;
    }
    let mut child = match Command::new(&browser)
        .args([
            format!("--remote-debugging-port={port}"),
            format!("--user-data-dir={}", profile.display()),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
            "--disable-sync".to_string(),
            "--disable-background-networking".to_string(),
            "--password-store=basic".to_string(),
            "--disable-dev-shm-usage".to_string(),
            login_url(kind).to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            sink(BrowserLoginEvent::Failed(format!(
                "启动浏览器失败：{e}（路径：{}）",
                browser.display()
            )));
            let _ = std::fs::remove_dir_all(&profile);
            return;
        }
    };
    sink(BrowserLoginEvent::Started(format!(
        "已打开 {}，请在浏览器里完成登录（短信/扫码/密码均可），{}",
        browser
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| browser.display().to_string()),
        "登录成功后程序会自动读取会话"
    )));

    let deadline = Instant::now() + Duration::from_secs(300);
    let mut last_err = String::new();
    let result: std::result::Result<Option<String>, String> = loop {
        if cancel.load(Ordering::Relaxed) {
            break Err("已取消浏览器登录".into());
        }
        if let Ok(Some(status)) = child.try_wait() {
            if !status.success() {
                break Err(format!("浏览器已退出（{status}）"));
            }
            break Err("浏览器已关闭，未取到登录会话".into());
        }
        if Instant::now() > deadline {
            break Err("等待登录超时（5 分钟），请重试".into());
        }
        match wait_page_target(port, Duration::from_secs(3)) {
            Ok((url, ws)) => {
                if !url.is_empty() {
                    wp_core::log_info!("界面", "浏览器登录：当前页面 {url}");
                }
                match poll_cookies(&ws, kind) {
                    Ok(Some(cookies)) => break Ok(Some(cookies)),
                    Ok(None) => {}
                    Err(e) => last_err = e,
                }
            }
            Err(e) => last_err = e,
        }
        std::thread::sleep(Duration::from_secs(2));
    };

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&profile);
    match result {
        Ok(Some(cookies)) => sink(BrowserLoginEvent::Cookies(cookies)),
        Ok(None) => sink(BrowserLoginEvent::Failed("未读取到登录会话，请重试".into())),
        Err(e) => sink(BrowserLoginEvent::Failed(if last_err.is_empty() {
            e
        } else {
            format!("{e}（最近一次 CDP 错误：{last_err}）")
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn cookie(name: &str, value: &str, domain: &str) -> CdpCookie {
        CdpCookie {
            name: name.into(),
            value: value.into(),
            domain: domain.into(),
        }
    }

    #[test]
    fn cookie_header_filters_domain_and_requires_keys() {
        let cs = vec![
            cookie("BDUSS", "b1", ".baidu.com"),
            cookie("STOKEN", "s1", ".baidu.com"),
            cookie("BDUSS", "b2", "pan.baidu.com"),
            cookie("other", "x", ".example.com"),
        ];
        let h = format_cookie_header(&cs, DriveKind::Baidu).expect("应能拼出 Cookie");
        assert!(h.contains("BDUSS=b2"), "同名的具体域优先：{h}");
        assert!(h.contains("STOKEN=s1"));
        assert!(!h.contains("other"));
        // 缺 STOKEN → 视为未登录完成
        let cs2 = vec![cookie("BDUSS", "b1", ".baidu.com")];
        assert!(format_cookie_header(&cs2, DriveKind::Baidu).is_none());
        // 夸克必需 __kps/__uid
        let cs3 = vec![
            cookie("__kps", "k", ".quark.cn"),
            cookie("__uid", "u", ".quark.cn"),
            cookie("__puus", "p", "pan.quark.cn"),
        ];
        let h = format_cookie_header(&cs3, DriveKind::Quark).unwrap();
        assert!(h.contains("__kps=k") && h.contains("__uid=u") && h.contains("__puus=p"));
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_simple(b""), "");
        assert_eq!(base64_simple(b"f"), "Zg==");
        assert_eq!(base64_simple(b"fo"), "Zm8=");
        assert_eq!(base64_simple(b"foo"), "Zm9v");
        assert_eq!(base64_simple(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn browser_candidates_include_common_names() {
        let c = browser_candidates();
        assert!(!c.is_empty());
        // 环境变量优先级
        unsafe { std::env::set_var("WP_LOGIN_BROWSER", "/tmp/fake-browser") };
        assert_eq!(
            browser_candidates().first().unwrap(),
            Path::new("/tmp/fake-browser")
        );
        unsafe { std::env::remove_var("WP_LOGIN_BROWSER") };
    }

    /// 假 CDP 服务器：先回 `/json/list`，再接受 WS 握手并回一条 getAllCookies 响应。
    fn mock_cdp_server() -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            // 1) HTTP /json/list
            let (mut c, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = c.read(&mut buf);
            let body = format!(
                r#"[{{"type":"page","url":"https://pan.baidu.com/","webSocketDebuggerUrl":"ws://127.0.0.1:{port}/devtools/page/MOCK"}}]"#
            );
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = c.write_all(resp.as_bytes());
            let _ = c.shutdown(std::net::Shutdown::Both);
            drop(c);
            // 2) WS 握手
            let (mut ws, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while head.len() < 8192 {
                if ws.read(&mut b).unwrap_or(0) == 0 {
                    break;
                }
                head.push(b[0]);
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let _ = ws.write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: dummy\r\n\r\n",
            );
            // 3) 读一条客户端帧（getAllCookies），回一条服务端文本帧
            let mut h = [0u8; 2];
            let _ = ws.read_exact(&mut h);
            let mut len = (h[1] & 0x7f) as usize;
            if len == 126 {
                let mut l = [0u8; 2];
                let _ = ws.read_exact(&mut l);
                len = u16::from_be_bytes(l) as usize;
            }
            let mut mask = [0u8; 4];
            let _ = ws.read_exact(&mut mask);
            let mut payload = vec![0u8; len];
            let _ = ws.read_exact(&mut payload);
            let body = json!({
                "id": 1,
                "result": {"cookies": [
                    {"name":"BDUSS","value":"mockbduss","domain":".baidu.com","path":"/"},
                    {"name":"STOKEN","value":"mockstoken","domain":".baidu.com","path":"/"}
                ]}
            })
            .to_string();
            let mut frame = vec![0x81u8];
            let n = body.len();
            if n < 126 {
                frame.push(n as u8);
            } else {
                frame.push(126);
                frame.extend_from_slice(&(n as u16).to_be_bytes());
            }
            frame.extend_from_slice(body.as_bytes());
            let _ = ws.write_all(&frame);
        });
        (port, handle)
    }

    #[test]
    fn cdp_ws_roundtrip_with_mock_server() {
        let (port, handle) = mock_cdp_server();
        let (url, ws) = wait_page_target(port, Duration::from_secs(3)).expect("mock 应提供页面");
        assert!(url.contains("pan.baidu.com"));
        let cookies = poll_cookies(&ws, DriveKind::Baidu)
            .expect("CDP 调用应成功")
            .expect("应取到必需 Cookie");
        assert!(cookies.contains("BDUSS=mockbduss"));
        assert!(cookies.contains("STOKEN=mockstoken"));
        handle.join().ok();
    }
}
