//! 内置登录通道：启动**随包分发的 Chromium**（`登录内核/`）打开网盘登录页，
//! 通过 CDP（Chrome DevTools Protocol）读回会话 Cookie。
//!
//! 产品定位：登录内核只用于登录，不打开用户系统里的浏览器，不需要用户装 Chrome。
//! * 内核来源：`scripts/fetch-login-chromium.sh` 生成 `登录内核/`，打包时一并带入；
//! * 找不到内核 → 明确提示重装完整包，并引导改用「Cookie」导入；
//! * 浏览器独立临时配置目录，关闭后清理，凭证只经适配器落盘。
//!
//! 背景：百度/夸克的「短信登录」在网页登录框的风控组件里（纯 HTTP 老接口走不通，
//! V8.3 老项目实测），所以登录必须有一次真实网页会话；这里用内置 Chromium 提供它。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
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
    /// 失败（未找到浏览器 / 超时 / CDP 错误）
    Failed(String),
    /// 已从网页登录态里抓到可用于导入的凭证（JSON 文本，如 access_token/refresh_token）
    Tokens(String),
    /// 窗口已正常关闭（用户关窗/取消；不是错误）
    Closed,
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
        // 打开网盘首页，随后用 CDP 自动点「去登录」弹出扫码登录框（用户要的形态）
        DriveKind::Baidu => "https://pan.baidu.com/",
        DriveKind::Quark => "https://pan.quark.cn/",
        DriveKind::Guangya => "https://www.guangyapan.com/",
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

/// 内置登录内核（Chromium）可执行文件候选。
///
/// **只认程序自带的 `登录内核/`**，不探测系统浏览器（产品要求：登录内核打包内置）。
/// `WP_LOGIN_BROWSER` 仅保留给开发调试，可指向任意 Chromium/Chrome 可执行文件。
pub fn browser_candidates() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("WP_LOGIN_BROWSER") {
        if !p.trim().is_empty() {
            out.push(PathBuf::from(p));
        }
    }

    // 便携版/安装版：可执行文件同级的 `登录内核/`；开发时：仓库根 `登录内核/`
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            roots.push(dir.to_path_buf());
            if let Some(parent) = dir.parent() {
                roots.push(parent.to_path_buf());
            }
        }
    }
    roots.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));

    #[cfg(target_os = "windows")]
    let names = ["chrome.exe", "登录内核/chrome.exe", "登录内核/chrome-linux/chrome.exe"];
    #[cfg(not(target_os = "windows"))]
    let names = ["登录内核/chrome", "登录内核/chromium", "登录内核/chrome-linux/chrome"];

    for root in roots {
        for name in names {
            out.push(root.join(name));
        }
    }
    out
}

pub fn find_browser() -> Option<PathBuf> {
    browser_candidates().into_iter().find(|p| p.is_file())
}

/// 登录内核信息（路径 + 版本串）。
#[derive(Debug, Clone)]
pub struct KernelInfo {
    pub path: PathBuf,
    /// `登录内核/VERSION.txt` 第一行，例如 `chromium-snapshot 1728157`
    pub version: String,
}

/// 从某个目录往下找登录内核（不给环境变量留口子，纯函数，便于单测）。
pub fn kernel_info_from(dir: &Path) -> Option<KernelInfo> {
    #[cfg(target_os = "windows")]
    let rels = ["登录内核/chrome.exe", "登录内核/chrome-linux/chrome.exe"];
    #[cfg(not(target_os = "windows"))]
    let rels = ["登录内核/chrome", "登录内核/chromium", "登录内核/chrome-linux/chrome"];
    for rel in rels {
        let p = dir.join(rel);
        if p.is_file() {
            let version = std::fs::read_to_string(dir.join("登录内核/VERSION.txt"))
                .ok()
                .and_then(|t| {
                    t.lines()
                        .find(|l| !l.trim().is_empty())
                        .map(|l| l.trim().to_string())
                })
                .unwrap_or_else(|| "未知版本".to_string());
            return Some(KernelInfo { path: p, version });
        }
    }
    None
}

/// 当前生效的登录内核（便携版看 exe 旁，开发时看仓库根）。
pub fn kernel_info() -> Option<KernelInfo> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if let Some(info) = kernel_info_from(dir) {
                return Some(info);
            }
            if let Some(parent) = dir.parent() {
                if let Some(info) = kernel_info_from(parent) {
                    return Some(info);
                }
            }
        }
    }
    kernel_info_from(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
}

/// 给 GUI 用的一行状态：`Chromium xxx（路径）` 或缺失提示。
pub fn kernel_report() -> String {
    match kernel_info() {
        Some(k) => {
            // 开发态路径可能含 `../..`，展示前尽量规范化
            let path = std::fs::canonicalize(&k.path).unwrap_or_else(|_| k.path.clone());
            format!("{}（{}）", k.version, path.display())
        }
        None => "未找到（登录窗口不可用；请使用完整绿色版或先跑 scripts/fetch-login-chromium.sh）".into(),
    }
}

/// 关窗后清理内核残留进程：Chromium 会 fork 渲染/GPU 等多个子进程，
/// 只 kill 主进程可能留下孤儿（它们还占着临时配置文件目录）。
fn kill_kernel_processes(profile: &Path) {
    #[cfg(unix)]
    {
        let marker = format!("--user-data-dir={}", profile.display());
        let _ = Command::new("pkill")
            .args(["-f", &marker])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        // pkill 只是发信号，子进程可能还在收尾；等它们退出再删目录，避免 ENOTEMPTY
        for _ in 0..20 {
            let alive = Command::new("pgrep")
                .args(["-f", &marker])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = profile;
    }
}

/// 删除临时登录配置目录（Chromium 收尾可能还在写文件，失败重试几次）。
fn cleanup_profile(profile: &Path) {
    for _ in 0..10 {
        if std::fs::remove_dir_all(profile).is_ok() || !profile.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    wp_core::log_warn!("登录", "临时登录配置清理失败：{}", profile.display());
}

/// 验收钩子（仅 `--features acceptance` 编译）：向真内核注入测试 Cookie，
/// 用来在没有真账号的情况下验证「内置窗口 → CDP 读取 → 事件回调 → 适配器导入」整条链路。
#[cfg(feature = "acceptance")]
fn inject_test_cookies(port: u16, kind: DriveKind) -> bool {
    let Ok((_url, ws_url)) = wait_page_target(port, Duration::from_secs(10)) else {
        return false;
    };
    let Ok(mut ws) = ws_connect(&ws_url) else {
        return false;
    };
    let domain = match kind {
        // 百度必须 pan 域 STOKEN（见 format_cookie_header 的说明）
        DriveKind::Baidu => "pan.baidu.com",
        DriveKind::Quark => ".quark.cn",
        _ => ".localhost",
    };
    for (i, key) in required_cookie_keys(kind).iter().enumerate() {
        let value = format!("wp_acceptance_{key}");
        let _ = cdp_call(
            &mut ws,
            (i + 1) as u64,
            "Network.setCookie",
            serde_json::json!({"name": key, "value": value, "domain": domain, "path": "/"}),
        );
    }
    wp_core::log_info!("界面", "验收模式：已向内置登录窗口注入测试 Cookie（验证自动导入链路）");
    true
}

/// 在页面里点一次「去登录/登录/立即登录」，让百度弹出扫码登录框。
const CLICK_LOGIN_JS: &str = r#"
(() => {
  const els = [...document.querySelectorAll('a,button,div,span')];
  const el = els.find(e => {
    const t = (e.innerText || '').trim();
    const r = e.getBoundingClientRect();
    return (t === '去登录' || t === '登录' || t === '立即登录') && r.width > 0 && r.height > 0;
  });
  if (!el) return 'not-found';
  el.click();
  return 'clicked:' + (el.innerText || '').trim();
})()
"#;

fn click_login_button(ws_url: &str) -> std::result::Result<String, String> {
    let mut ws = ws_connect(ws_url)?;
    let v = cdp_call(
        &mut ws,
        1,
        "Runtime.evaluate",
        serde_json::json!({"expression": CLICK_LOGIN_JS, "returnByValue": true}),
    )?;
    Ok(v
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string())
}

/// 用 CDP 把页面导航到指定地址（百度补偿：触发 pan 域会话 Cookie 下发）。
fn navigate_page(ws_url: &str, url: &str) -> std::result::Result<(), String> {
    let mut ws = ws_connect(ws_url)?;
    cdp_call(&mut ws, 1, "Page.navigate", serde_json::json!({"url": url}))?;
    wp_core::log_info!("界面", "内置登录窗口：已导航到 {url}");
    Ok(())
}

/// 当前正在运行的内置登录（主程序退出时要能取消/清理）。
struct ActiveLogin {
    profile: PathBuf,
    cancel: Arc<AtomicBool>,
}

static ACTIVE_LOGIN: StdMutex<Option<ActiveLogin>> = StdMutex::new(None);

/// 光鸭专用：内置浏览器打开「设备授权页」（无 Cookie 登录能力，走设备码流程）。
///
/// 窗口本身不带 Cookie 读取逻辑——设备码轮询由 App 侧复用扫码通道完成；
/// 这里只负责把授权页显示出来、并在取消/关窗时清理内核进程。
pub fn run_device_window(
    drive_id: &str,
    url: &str,
    data_root: &Path,
    cancel: Arc<AtomicBool>,
    sink: Sink,
) {
    let Some(browser) = find_browser() else {
        sink(BrowserLoginEvent::Failed(
            "找不到内置登录内核（程序目录下的「登录内核」）。请使用完整绿色版/安装包重新解压。"
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
            format!("--user-data-dir={}", profile.display()),
            format!("--remote-debugging-port={port}"),
            format!("--app={url}"),
            "--window-size=1024,860".to_string(),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
            "--disable-extensions".to_string(),
            "--disable-component-update".to_string(),
            "--disable-breakpad".to_string(),
            "--disable-crash-reporter".to_string(),
            "--disable-dev-shm-usage".to_string(),
            "--deny-permission-prompts".to_string(),
            "--disable-notifications".to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            sink(BrowserLoginEvent::Failed(format!(
                "启动内置登录窗口失败：{e}（路径：{}）",
                browser.display()
            )));
            cleanup_profile(&profile);
            return;
        }
    };
    wp_core::log_info!("界面", "内置登录窗口：设备授权页 {url}");
    sink(BrowserLoginEvent::Started(
        "已打开内置登录窗口（设备授权页）：请在窗口里完成登录/确认，程序会自动导入".to_string(),
    ));

    // 诊断：定期抓页面 localStorage/Cookie 快照（用户登录后即可看到 token 落在哪）
    // 从 localStorage 里找形如 {access_token, refresh_token} 的 JSON（光鸭网页版登录后写在
    // `credentials_<client_id>` 键里）。找到就直接返回该 JSON。
    const SNAPSHOT_JS: &str = r#"
(() => {
  try {
    for (let i = 0; i < localStorage.length; i++) {
      const k = localStorage.key(i);
      const v = localStorage.getItem(k) || '';
      if (v.indexOf('access_token') >= 0) {
        try { const j = JSON.parse(v); if (j && j.access_token) return v; } catch (e) {}
      }
    }
    return '';
  } catch (e) { return ''; }
})()
"#;
    let mut last_snap = String::new();
    // 等窗口关闭或取消（设备码轮询在 App 侧继续）
    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        if let Ok((_u, ws)) = wait_page_target(port, Duration::from_secs(1)) {
            let mut w = ws_connect(&ws).ok();
            if let Some(ref mut ws2) = w {
                if let Ok(v) = cdp_call(
                    ws2,
                    1,
                    "Runtime.evaluate",
                    serde_json::json!({"expression": SNAPSHOT_JS, "returnByValue": true}),
                ) {
                    let snap = v
                        .get("result")
                        .and_then(|r| r.get("value"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !snap.is_empty() && snap != last_snap {
                        last_snap = snap.clone();
                        wp_core::log_info!(
                            "界面",
                            "内置登录窗口：已从网页登录态抓到凭证（{} 字节），交给适配器导入",
                            snap.len()
                        );
                        sink(BrowserLoginEvent::Tokens(snap));
                        break;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1500));
    }
    let _ = child.kill();
    let _ = child.wait();
    kill_kernel_processes(&profile);
    cleanup_profile(&profile);
    sink(BrowserLoginEvent::Closed);
}

/// 启动时清理上次异常退出（崩溃/被 SIGKILL）留下的登录内核进程与临时配置。
///
/// 正常关闭走 `shutdown_active`；但如果进程是被强杀的，Chromium 子进程可能还活着、
/// 临时配置目录也会残留。这里按目录逐个 pkill + 删除，保证不会越积越多。
pub fn sweep_stale(data_root: &Path) {
    let root = data_root.join("浏览器登录");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    let mut cleaned = 0usize;
    for e in entries.flatten() {
        let dir = e.path();
        if !dir.is_dir() {
            continue;
        }
        kill_kernel_processes(&dir);
        cleanup_profile(&dir);
        cleaned += 1;
    }
    if cleaned > 0 {
        wp_core::log_info!("界面", "已清理上次异常退出残留的内置登录配置：{cleaned} 个");
    }
}

/// 主程序退出时调用：取消并清理正在运行的内置登录窗口。
///
/// 不做这件事的后果：用户开着登录窗口直接关主程序，登录线程随进程消失，
/// 但 Chromium 子进程会变成孤儿留在桌面上（还占着临时配置目录）。
pub fn shutdown_active() {
    let active = ACTIVE_LOGIN.lock().ok().and_then(|mut g| g.take());
    if let Some(a) = active {
        a.cancel.store(true, Ordering::Relaxed);
        kill_kernel_processes(&a.profile);
        cleanup_profile(&a.profile);
    }
}

/// RAII：`run` 结束时把自己从注册表摘掉（被 shutdown_active 摘掉也无妨）。
struct ActiveGuard {
    profile: PathBuf,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        if let Ok(mut g) = ACTIVE_LOGIN.lock() {
            let same = g
                .as_ref()
                .map(|a| a.profile == self.profile)
                .unwrap_or(false);
            if same {
                *g = None;
            }
        }
    }
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
///
/// 注意：Chrome 的 DevTools HTTP 端**不会因为 `Connection: close` 就立刻关连接**，
/// 老实现用 `read_to_end` 会一直等到读超时（`WouldBlock`/EAGAIN）。
/// 这里改成：先读到响应头，再按 `Content-Length`/chunked 读 body，不再依赖连接关闭。
fn cdp_http_json(port: u16, path: &str) -> std::result::Result<Value, String> {
    use std::io::Read as _;

    let mut s =
        TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("连接调试端口失败：{e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(3))).ok();
    s.set_write_timeout(Some(Duration::from_secs(3))).ok();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\n\
         User-Agent: wp-login-cdp\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| format!("请求调试端口失败：{e}"))?;

    // 1) 读响应头（读到 \r\n\r\n 为止）
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return Err("调试端口响应头过大".into());
        }
        let n = s
            .read(&mut tmp)
            .map_err(|e| format!("读取调试端口失败：{e}"))?;
        if n == 0 {
            return Err("调试端口在响应头前关闭连接".into());
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    if !head.starts_with("HTTP/1.1 200") && !head.starts_with("HTTP/1.0 200") {
        return Err(format!(
            "调试端口返回非 200：{}",
            head.lines().next().unwrap_or("")
        ));
    }
    let lower = head.to_ascii_lowercase();
    let mut body: Vec<u8> = buf[head_end + 4..].to_vec();

    // 2) 按头读 body
    if lower.contains("transfer-encoding: chunked") {
        // chunked：读到结束块或超时为止（已读到的部分先保留）
        let mut tmp = [0u8; 4096];
        loop {
            match s.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => body.extend_from_slice(&tmp[..n]),
                Err(_) => break,
            }
            if body.windows(7).any(|w| w == b"\r\n0\r\n\r\n") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&body).to_string();
        let json_text = decode_chunked(&text)?;
        return serde_json::from_str(json_text.trim())
            .map_err(|e| format!("调试端口 JSON 解析失败：{e}"));
    }

    let content_len = lower
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok());
    if let Some(want) = content_len {
        while body.len() < want {
            match s.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => body.extend_from_slice(&tmp[..n]),
                Err(_) => break,
            }
        }
    }
    let text = String::from_utf8_lossy(&body).to_string();
    serde_json::from_str(text.trim()).map_err(|e| format!("调试端口 JSON 解析失败：{e}"))
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
    let mut best: HashMap<String, (i32, String, String)> = HashMap::new();
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
            Some((old, _, _)) if *old >= score => {}
            _ => {
                best.insert(c.name.clone(), (score, c.value.clone(), c.domain.clone()));
            }
        }
    }
    let required = required_cookie_keys(kind);
    for k in required {
        // 百度登录态可能是 BDUSS 或新版 BDUSS_BFESS（适配器两者等价），任一即可
        let alt_ok = matches!(kind, DriveKind::Baidu)
            && *k == "BDUSS"
            && best.contains_key("BDUSS_BFESS");
        if !best.contains_key(*k) && !alt_ok {
            return None;
        }
    }
    // 百度：只有 pan 域 STOKEN 才带写权限；passport 域的 64 字符 STOKEN 只能只读。
    // 这一步是「浏览器登录成功但导入后只读」的根因修复：不能一凑齐 BDUSS+STOKEN 就返回，
    // 否则会在 pan 会话建立前把 passport 域 STOKEN 抓走。
    if matches!(kind, DriveKind::Baidu) {
        let pan = best
            .get("STOKEN")
            .map(|(score, _, _)| *score >= 30)
            .unwrap_or(false);
        if !pan {
            return None;
        }
    }
    if matches!(kind, DriveKind::Baidu) {
        if let Some((score, v, dom)) = best.get("STOKEN") {
            wp_core::log_info!(
                "界面",
                "内置登录窗口：百度 STOKEN 域={dom} 评分={score} 长度={}",
                v.len()
            );
        }
    }
    let mut parts: Vec<String> = best.iter().map(|(k, (_, v, _))| format!("{k}={v}")).collect();
    parts.sort();
    Some(parts.join("; "))
}

/// 若调试端口上已有登录后的必需 Cookie，返回 Cookie 串。
fn fetch_cookies(ws_url: &str) -> std::result::Result<Vec<CdpCookie>, String> {
    let mut s = ws_connect(ws_url)?;
    let result = cdp_call(&mut s, 1, "Network.getAllCookies", json!({}))?;
    Ok(parse_cookies(&result))
}

fn poll_cookies(ws_url: &str, kind: DriveKind) -> std::result::Result<Option<String>, String> {
    let cookies = fetch_cookies(ws_url)?;
    Ok(format_cookie_header(&cookies, kind))
}

/// 百度补偿导航策略：**只在登录已发生（有 BDUSS）但还缺 pan 域 STOKEN 时**导航，
/// 且总次数有上限。登录前绝不能导航——会把用户正在填的短信/密码表单刷掉。
fn should_nudge_pan(cookies: &[CdpCookie], nudges_done: u32) -> bool {
    if nudges_done >= 5 {
        return false;
    }
    let has_bduss = cookies
        .iter()
        .any(|c| c.name == "BDUSS" && !c.value.is_empty());
    let has_pan_stoken = cookies.iter().any(|c| {
        c.name == "STOKEN"
            && !c.value.is_empty()
            && c.domain
                .trim_start_matches('.')
                .starts_with("pan.baidu.com")
    });
    has_bduss && !has_pan_stoken
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
            "找不到内置登录内核（程序目录下的「登录内核」）。请使用完整绿色版/安装包重新解压；也可改用「Cookie」标签手动导入。"
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
    // 注册「正在登录」，供主程序退出时取消/清理
    if let Ok(mut g) = ACTIVE_LOGIN.lock() {
        *g = Some(ActiveLogin {
            profile: profile.clone(),
            cancel: cancel.clone(),
        });
    }
    let _active_guard = ActiveGuard {
        profile: profile.clone(),
    };

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
            // 登录页在 1024x768 下更好操作（短信弹层不用横向滚动）
            "--window-size=1024,768".to_string(),
            // 只显示登录页本身：app 窗口没有标签栏/地址栏/书签栏/菜单
            format!("--app={}", login_url(kind)),
            // 登录用不到的 Chromium 组件一律关掉（少进程、少界面、少联网）
            "--disable-extensions".to_string(),
            "--disable-component-update".to_string(),
            "--disable-breakpad".to_string(),
            "--disable-crash-reporter".to_string(),
            "--disable-default-apps".to_string(),
            // 登录窗口不需要任何设备权限：自动拒绝，避免页面反复弹权限对话框
            "--deny-permission-prompts".to_string(),
            "--disable-notifications".to_string(),
            "--disable-features=Translate,TranslateUI,MediaRouter,OptimizationHints,\
InterestFeedContentSuggestions,AutofillServerCommunication,PrivacySandboxSettings4,\
DialMediaRouteProvider,GlobalMediaControls,HardwareMediaKeyHandling,CalculateNativeWinOcclusion,\
LocalNetworkAccessChecks,LocalNetworkAccessPermission"
                .to_string(),
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
            cleanup_profile(&profile);
            return;
        }
    };
    // 文案统一成「内置登录窗口」：登录内核可能是 chrome，但对用户不是「浏览器」
    let ver = kernel_info()
        .map(|k| k.version)
        .unwrap_or_else(|| "Chromium".to_string());
    sink(BrowserLoginEvent::Started(format!(
        "已打开内置登录窗口（{ver}）：请在里面完成登录（短信/扫码/密码均可），完成后再回到本程序"
    )));

    #[cfg(feature = "acceptance")]
    if std::env::var("WP_LOGIN_TEST_COOKIES").map(|v| v == "1").unwrap_or(false) {
        // 等窗口起来一点再注入，避免 target 还没就绪
        for _ in 0..40 {
            if inject_test_cookies(port, kind) {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    let deadline = Instant::now() + Duration::from_secs(300);
    let mut last_err = String::new();
    let mut login_clicked = false;
    let mut nudges_done: u32 = 0;
    let mut last_nudge = Instant::now()
        .checked_sub(Duration::from_secs(8))
        .unwrap_or_else(Instant::now);
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
                    wp_core::log_info!("界面", "内置登录窗口：当前页面 {url}");
                }
                // 百度：进了网盘首页后自动点「去登录」，直接弹出扫码登录框（不需要手动点）
                if matches!(kind, DriveKind::Baidu)
                    && !login_clicked
                    && url.contains("pan.baidu.com")
                {
                    if let Ok(r) = click_login_button(&ws) {
                        wp_core::log_info!("界面", "内置登录窗口：自动点击登录按钮 → {r}");
                        if r.starts_with("clicked") {
                            login_clicked = true;
                        }
                    }
                }
                match poll_cookies(&ws, kind) {
                    Ok(Some(cookies)) => break Ok(Some(cookies)),
                    Ok(None) => {
                        // 百度：登录已发生但缺 pan 域 STOKEN 时，才导航回 pan 触发会话下发；
                        // 登录前绝不导航，避免把用户正在填的表单刷掉。
                        if matches!(kind, DriveKind::Baidu)
                            && last_nudge.elapsed() >= Duration::from_secs(8)
                        {
                            if let Ok(cookies) = fetch_cookies(&ws) {
                                if should_nudge_pan(&cookies, nudges_done) {
                                    let _ = navigate_page(&ws, "https://pan.baidu.com/");
                                    nudges_done += 1;
                                    last_nudge = Instant::now();
                                }
                            }
                        }
                    }
                    Err(e) => last_err = e,
                }
            }
            Err(e) => last_err = e,
        }
        std::thread::sleep(Duration::from_secs(2));
    };

    let _ = child.kill();
    let _ = child.wait();
    kill_kernel_processes(&profile);
    cleanup_profile(&profile);
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

    /// 真内核类测试串行执行：它们都会启动 Chromium、且 shutdown_active 是全局状态。
    static KERNEL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn kernel_lock() -> std::sync::MutexGuard<'static, ()> {
        KERNEL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn cookie(name: &str, value: &str, domain: &str) -> CdpCookie {
        CdpCookie {
            name: name.into(),
            value: value.into(),
            domain: domain.into(),
        }
    }

    #[test]
    fn cookie_header_filters_domain_and_requires_keys() {
        // pan 域 STOKEN 才代表可写会话；passport 域的 64 字符 STOKEN 只能只读
        let cs = vec![
            cookie("BDUSS", "b1", ".baidu.com"),
            cookie("STOKEN", "s1", "passport.baidu.com"),
            cookie("STOKEN", "s2", "pan.baidu.com"),
            cookie("other", "x", ".example.com"),
        ];
        let h = format_cookie_header(&cs, DriveKind::Baidu).expect("应能拼出 Cookie");
        assert!(h.contains("BDUSS=b1"), "{h}");
        assert!(h.contains("STOKEN=s2"), "必须选 pan 域 STOKEN：{h}");
        assert!(!h.contains("STOKEN=s1"), "不能选 passport 域 STOKEN：{h}");
        assert!(!h.contains("other"));
        // 只有 passport 域 STOKEN → 继续等待（否则会导入只读会话）
        let cs_passport = vec![
            cookie("BDUSS", "b1", ".baidu.com"),
            cookie("STOKEN", "s1", "passport.baidu.com"),
        ];
        assert!(format_cookie_header(&cs_passport, DriveKind::Baidu).is_none());
        // 新版 Cookie：只有 BDUSS_BFESS 也应可用（适配器支持两者等价）
        let cs_bfess = vec![
            cookie("BDUSS_BFESS", "bf", ".baidu.com"),
            cookie("STOKEN", "s2", "pan.baidu.com"),
        ];
        let h = format_cookie_header(&cs_bfess, DriveKind::Baidu)
            .expect("BDUSS_BFESS 应可替代 BDUSS");
        assert!(h.contains("BDUSS_BFESS=bf"), "{h}");
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
    fn browser_candidates_prefer_env_then_bundled_kernel() {
        let c = browser_candidates();
        assert!(!c.is_empty());
        // 候选里必须能找到「登录内核/」路径（内置内核是唯一正式来源）
        assert!(
            c.iter().any(|p| p.to_string_lossy().contains("登录内核")),
            "候选应包含内置登录内核路径：{c:?}"
        );
        // 环境变量（开发调试）优先级最高
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
                    {"name":"STOKEN","value":"mockstoken","domain":"pan.baidu.com","path":"/"}
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
    fn kernel_info_reads_version_and_path() {
        let dir = std::env::temp_dir().join(format!("wp-kernel-test-{}", std::process::id()));
        let kernel = dir.join("登录内核");
        std::fs::create_dir_all(&kernel).unwrap();
        #[cfg(target_os = "windows")]
        let exe = kernel.join("chrome.exe");
        #[cfg(not(target_os = "windows"))]
        let exe = kernel.join("chrome");
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        std::fs::write(kernel.join("VERSION.txt"), "chromium-snapshot 1728157\nsource x\n").unwrap();

        let info = kernel_info_from(&dir).expect("应找到登录内核");
        assert_eq!(info.version, "chromium-snapshot 1728157");
        assert!(info.path.ends_with("chrome") || info.path.ends_with("chrome.exe"));

        // 没有内核的目录要返回 None，不能误报
        let empty = std::env::temp_dir().join(format!("wp-kernel-empty-{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        assert!(kernel_info_from(&empty).is_none());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty);
    }

    /// 真内核联调：启动随包 Chromium → CDP 注入 Cookie → 用产品同款过滤逻辑读回。
    ///
    /// 这条覆盖「真实 Chromium 的 Cookie 存储 + 我们的 WS/过滤管线」，
    /// 而不是 mock。没有内核或没有图形环境时自动跳过（CI 安全）。
    #[test]
    fn real_kernel_cdp_reads_cookies() {
        let Some(kernel) = find_browser() else {
            eprintln!("跳过：没有 登录内核/");
            return;
        };
        // 注意：本机 DISPLAY 可能是「已设置但为空」，不能只看 is_some
        let has_display = std::env::var("DISPLAY").map(|v| !v.trim().is_empty()).unwrap_or(false);
        let has_xvfb = Command::new("xvfb-run")
            .arg("--help")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !has_display && !has_xvfb {
            eprintln!("跳过：没有 DISPLAY 也没有 xvfb-run");
            return;
        }
        let _serial = kernel_lock();

        let port = free_port();
        let profile = std::env::temp_dir().join(format!("wp-kernel-cdp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&profile);
        std::fs::create_dir_all(&profile).unwrap();

        let mut cmd = if has_display {
            Command::new(&kernel)
        } else {
            let mut c = Command::new("xvfb-run");
            c.arg("-a").arg(&kernel);
            c
        };
        // 必须用 `--flag=value` 形式：Chromium 159 对空格分隔的 --user-data-dir 会当成
        // 无值开关，值被当成 URL，于是回落到默认配置并「在现有浏览器会话中打开」。
        let mut child = cmd
            .args([
                format!("--user-data-dir={}", profile.display()),
                format!("--remote-debugging-port={port}"),
                "--no-first-run".to_string(),
                "--no-default-browser-check".to_string(),
                "about:blank".to_string(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("启动登录内核失败");

        let result = (|| -> std::result::Result<String, String> {
            let (_url, ws_url) = match wait_page_target(port, Duration::from_secs(20)) {
                Ok(v) => v,
                Err(e) => {
                    // 诊断：换 curl 再试一次，区分「Chrome 没起」和「我们的最小 HTTP 客户端问题」
                    let curl = Command::new("curl")
                        .args(["-sS", "-m", "3", &format!("http://127.0.0.1:{port}/json/version")])
                        .output();
                    let curl_msg = match curl {
                        Ok(o) => format!(
                            "curl rc={:?} out={} err={}",
                            o.status.code(),
                            String::from_utf8_lossy(&o.stdout).chars().take(120).collect::<String>(),
                            String::from_utf8_lossy(&o.stderr).chars().take(120).collect::<String>()
                        ),
                        Err(e) => format!("curl 无法执行：{e}"),
                    };
                    return Err(format!("{e} | 诊断：{curl_msg}"));
                }
            };
            let mut ws = ws_connect(&ws_url)?;
            for (i, (name, value)) in [("BDUSS", "testbduss"), ("STOKEN", "teststoken")]
                .iter()
                .enumerate()
            {
                cdp_call(
                    &mut ws,
                    (i + 1) as u64,
                    "Network.setCookie",
                    // 百度必须 pan 域 STOKEN（passport 域只能只读）
                    json!({"name": name, "value": value, "domain": "pan.baidu.com", "path": "/"}),
                )?;
            }
            drop(ws);
            let header = poll_cookies(&ws_url, DriveKind::Baidu)?
                .ok_or_else(|| "真实 Chromium 里没读到注入的 Cookie".to_string())?;
            Ok(header)
        })();

        let _ = child.kill();
        let _ = child.wait();
        // xvfb-run 会再 fork 出 chrome，单独 kill 包装进程可能留下子进程
        let marker = profile.to_string_lossy().to_string();
        let _ = Command::new("pkill")
            .args(["-f", &marker])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(&profile);

        let header = result.expect("真内核 CDP 链路应能读到 Cookie");
        assert!(header.contains("BDUSS=testbduss"), "Cookie 头：{header}");
        assert!(header.contains("STOKEN=teststoken"), "Cookie 头：{header}");
    }

    /// 生命周期：取消后必须「有 Failed(已取消) 事件 + 临时配置目录被删 + 无内核进程残留」。
    /// 这才是绿色版真实使用时的收尾路径（用户点取消/关窗）。
    #[test]
    fn run_cancel_cleans_up_kernel() {
        if find_browser().is_none() {
            eprintln!("跳过：没有 登录内核/");
            return;
        }
        let has_display = std::env::var("DISPLAY").map(|v| !v.trim().is_empty()).unwrap_or(false);
        let has_xvfb = Command::new("xvfb-run")
            .arg("--help")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !has_display && !has_xvfb {
            eprintln!("跳过：没有 DISPLAY 也没有 xvfb-run");
            return;
        }
        let _serial = kernel_lock();

        let root = std::env::temp_dir().join(format!("wp-login-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let cancel2 = cancel.clone();
        let root2 = root.clone();
        let handle = std::thread::spawn(move || {
            let sink: Sink = Box::new(move |ev| {
                let _ = tx.send(format!("{ev:?}"));
            });
            run(DriveKind::Baidu, "cancel-test", &root2, cancel2, sink);
        });

        let first = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("启动后应至少有一个事件");
        assert!(first.contains("Started"), "首个事件应是 Started，实际：{first}");

        std::thread::sleep(Duration::from_millis(800));
        cancel.store(true, Ordering::Relaxed);

        let mut last = first;
        let deadline = Instant::now() + Duration::from_secs(40);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(ev) => {
                    let done = ev.contains("Failed");
                    last = ev;
                    if done {
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        handle.join().expect("登录线程应正常结束");

        assert!(last.contains("已取消"), "取消后应收 Failed(已取消)，实际：{last}");

        let profile_root = root.join("浏览器登录");
        let leftover: Vec<_> = std::fs::read_dir(&profile_root)
            .map(|it| it.filter_map(|e| e.ok()).map(|e| e.path()).collect())
            .unwrap_or_default();
        assert!(leftover.is_empty(), "临时配置目录未清理：{leftover:?}");

        if let Ok(out) = Command::new("pgrep")
            .args([
                "-f",
                &format!("--user-data-dir={}", profile_root.display()),
            ])
            .output()
        {
            assert!(out.stdout.is_empty(), "仍有内核进程占用该配置目录");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 主程序退出路径：`shutdown_active()` 必须取消登录、清目录、不留进程。
    #[test]
    fn shutdown_active_cancels_running_login() {
        if find_browser().is_none() {
            eprintln!("跳过：没有 登录内核/");
            return;
        }
        let has_display = std::env::var("DISPLAY").map(|v| !v.trim().is_empty()).unwrap_or(false);
        let has_xvfb = Command::new("xvfb-run")
            .arg("--help")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !has_display && !has_xvfb {
            eprintln!("跳过：没有 DISPLAY 也没有 xvfb-run");
            return;
        }
        let _serial = kernel_lock();

        let root = std::env::temp_dir().join(format!("wp-login-shutdown-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let root2 = root.clone();
        let handle = std::thread::spawn(move || {
            let sink: Sink = Box::new(move |ev| {
                let _ = tx.send(format!("{ev:?}"));
            });
            run(
                DriveKind::Baidu,
                "shutdown-test",
                &root2,
                Arc::new(AtomicBool::new(false)),
                sink,
            );
        });

        let first = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("启动后应至少有一个事件");
        assert!(first.contains("Started"), "首个事件应是 Started，实际：{first}");
        std::thread::sleep(Duration::from_millis(300));

        // 模拟主程序退出
        shutdown_active();

        // 等登录线程收尾（事件通道断开即线程结束）
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(_) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "登录线程未在超时内退出");
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        handle.join().expect("登录线程应正常结束");

        let profile_root = root.join("浏览器登录");
        let leftover: Vec<_> = std::fs::read_dir(&profile_root)
            .map(|it| it.filter_map(|e| e.ok()).map(|e| e.path()).collect())
            .unwrap_or_default();
        assert!(leftover.is_empty(), "退出后临时配置目录未清理：{leftover:?}");
        let _ = std::fs::remove_dir_all(&root);

        // 幂等：没有登录时再调一次不应 panic
        shutdown_active();
    }

    #[test]
    fn sweep_stale_removes_leftover_profiles() {
        let root = std::env::temp_dir().join(format!("wp-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("浏览器登录").join("baidu-123");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lock"), b"x").unwrap();

        sweep_stale(&root);

        assert!(!dir.exists(), "残留临时配置应被清理");
        // 幂等：目录不存在时再调不 panic
        sweep_stale(&root);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 用户真机 bug 的回归测试：百度登录先下发 passport 域 64 字符 STOKEN，
    /// 此时**不能**返回会话；必须等 pan 域 STOKEN 出现（否则导入的就是只读会话）。
    #[test]
    fn nudge_pan_only_after_login_and_without_pan_stoken() {
        let c = |name: &str, value: &str, domain: &str| CdpCookie {
            name: name.into(),
            value: value.into(),
            domain: domain.into(),
        };
        // 登录前（没有 BDUSS）：绝不能导航
        assert!(!should_nudge_pan(&[], 0));
        assert!(!should_nudge_pan(
            &[c("STOKEN", "p64", "passport.baidu.com")],
            0
        ));
        // 已登录 + 只有 passport 域 STOKEN → 应该导航
        assert!(should_nudge_pan(
            &[
                c("BDUSS", "bd", ".baidu.com"),
                c("STOKEN", "p64", "passport.baidu.com")
            ],
            0
        ));
        // 已拿到 pan 域 STOKEN → 不该再导航
        assert!(!should_nudge_pan(
            &[
                c("BDUSS", "bd", ".baidu.com"),
                c("STOKEN", "p128", "pan.baidu.com")
            ],
            0
        ));
        // 次数上限
        assert!(!should_nudge_pan(&[c("BDUSS", "bd", ".baidu.com")], 5));
    }

    #[test]
    fn real_kernel_waits_for_pan_domain_stoken() {
        if find_browser().is_none() {
            eprintln!("跳过：没有 登录内核/");
            return;
        }
        let has_display = std::env::var("DISPLAY").map(|v| !v.trim().is_empty()).unwrap_or(false);
        let has_xvfb = Command::new("xvfb-run")
            .arg("--help")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !has_display && !has_xvfb {
            eprintln!("跳过：没有 DISPLAY 也没有 xvfb-run");
            return;
        }
        let _serial = kernel_lock();

        let port = free_port();
        let profile = std::env::temp_dir().join(format!("wp-kernel-pan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&profile);
        std::fs::create_dir_all(&profile).unwrap();
        let mut cmd = if has_display {
            Command::new(find_browser().unwrap())
        } else {
            let mut c = Command::new("xvfb-run");
            c.arg("-a").arg(find_browser().unwrap());
            c
        };
        let mut child = cmd
            .args([
                format!("--user-data-dir={}", profile.display()),
                format!("--remote-debugging-port={port}"),
                "--no-first-run".to_string(),
                "--no-default-browser-check".to_string(),
                // 用 app 窗口登录页，贴近真实登录场景
                "--app=https://pan.baidu.com/".to_string(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("启动登录内核失败");

        let result = (|| -> std::result::Result<String, String> {
            let (_url, ws_url) = wait_page_target(port, Duration::from_secs(25))?;
            let mut ws = ws_connect(&ws_url)?;
            let mut id = 0u64;
            let mut set_cookie = |ws: &mut TcpStream, id: &mut u64, name: &str, value: &str, domain: &str| {
                *id += 1;
                cdp_call(
                    ws,
                    *id,
                    "Network.setCookie",
                    json!({"name": name, "value": value, "domain": domain, "path": "/"}),
                )
            };
            // 1) 只有 passport 域 STOKEN：必须继续等，不能返回
            set_cookie(&mut ws, &mut id, "BDUSS", "bd", ".baidu.com")?;
            set_cookie(&mut ws, &mut id, "STOKEN", "passport64", "passport.baidu.com")?;
            if poll_cookies(&ws_url, DriveKind::Baidu)?.is_some() {
                return Err("只有 passport 域 STOKEN 时不应返回会话（否则会导入只读会话）".into());
            }
            // 2) 主动导航回 pan.baidu.com（产品里的补偿逻辑）
            navigate_page(&ws_url, "https://pan.baidu.com/")?;
            // 3) pan 域 STOKEN 出现后才返回，且必须选 pan 域那个
            set_cookie(&mut ws, &mut id, "STOKEN", "pan128", "pan.baidu.com")?;
            let header = poll_cookies(&ws_url, DriveKind::Baidu)?
                .ok_or_else(|| "pan 域 STOKEN 已存在但没返回会话".to_string())?;
            Ok(header)
        })();

        let _ = child.kill();
        let _ = child.wait();
        let marker = profile.to_string_lossy().to_string();
        let _ = Command::new("pkill")
            .args(["-f", &marker])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(&profile);

        let header = result.expect("pan 域 STOKEN 规则回归测试失败");
        assert!(header.contains("STOKEN=pan128"), "必须取 pan 域 STOKEN：{header}");
        assert!(!header.contains("passport64"), "不能取 passport 域 STOKEN：{header}");
    }

    #[test]
    fn cdp_ws_roundtrip_with_mock_server() {
        let (port, handle) = mock_cdp_server();
        let (url, ws) = wait_page_target(port, Duration::from_secs(10)).expect("mock 应提供页面");
        assert!(url.contains("pan.baidu.com"));
        let cookies = poll_cookies(&ws, DriveKind::Baidu)
            .expect("CDP 调用应成功")
            .expect("应取到必需 Cookie");
        assert!(cookies.contains("BDUSS=mockbduss"));
        assert!(cookies.contains("STOKEN=mockstoken"));
        handle.join().ok();
    }
}
