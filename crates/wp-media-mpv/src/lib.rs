//! libmpv 播放后端（wp-media-mpv）：**运行时动态加载 + 进程内 GL 渲染（Mode A）**。
//!
//! 设计要点（详见 `docs/PLAYER_ARCHITECTURE.md`）：
//! - 不编译期链接 libmpv：用 [`libloading`] 运行时加载（源码可控 / 用户可替换 / 缺库可回退）；
//! - [`MpvEngine`] 实现 [`wp_media::MediaEngine`]：内部事件线程 + 命令队列 + 属性观察；
//! - `render` 模块（feature `render-gl`，默认开）提供 [`render::MpvRenderContext`]：
//!   用 eframe/egui 当前的 GL 上下文把 mpv 画面渲染进 egui 的绘制回调（进程内、可覆盖 UI）。
//!
//! # 查找顺序（Linux，发布形态）
//!
//! 1. [`MpvConfig::lib_path`]（显式指定，最高优先级）
//! 2. 环境变量 `WP_MPV_LIB`（开发/测试/发布验收用）
//! 3. **可执行文件同级的 `播放器运行时/lib/libmpv.so.2`**（随包发布的 LGPL 构建；
//!    相对 `current_exe()` 解析，dev（`target/debug/`）与发布目录都适用）
//! 4. 可执行文件同级（某些打包方式直接放库）
//! 5. 系统库 `/usr/lib/libmpv.so.2` 等（兜底）
//!
//! 实际加载的路径与来源会记录到日志（`已加载 libmpv：<path>（来源：随包运行时/…）`），
//! 并可用 [`probe_version_with_path`] 在启动自检/发布验收中读取。
//!
//! Windows 对应 `libmpv-2.dll`（同样「程序目录 `播放器运行时\` 优先 → 系统兜底」）。
//!
//! # GUI 集成（eframe 0.37 / egui 0.37）
//!
//! ```no_run
//! # use wp_media::MediaEngine;
//! # use wp_media_mpv::{MpvConfig, MpvEngine};
//! # fn demo(cc: &eframe::CreationContext<'_>) -> Result<(), Box<dyn std::error::Error>> {
//! // ① 创建引擎（找不到 libmpv 时给可读错误，不 panic）
//! let engine = MpvEngine::new(MpvConfig::default())?;
//! // ② 创建渲染桥（不要求当前 GL 上下文；真正的 render context 在首次绘制时创建）
//! let rc = wp_media_mpv::render::MpvRenderContext::new(
//!     engine.render_source()?,
//!     cc.get_proc_address.clone().expect("glow 后端才有"),
//!     cc.egui_ctx.clone(),
//! )?;
//! // ③ 播放页每帧：在 egui 视频矩形里画
//! //    let _resp = rc.paint_video(ui, egui::vec2(1280.0, 720.0));
//! // ④ App::on_exit：先 destroy 渲染桥，再 shutdown 引擎（顺序不能反）
//! # Ok(()) }
//! ```

#![forbid(unsafe_op_in_unsafe_fn)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use wp_core::{Error, Result};

mod engine;
#[allow(dead_code)] // 手写绑定天然会有未用到的常量/事件 id，保留全部定义便于对照头文件
pub(crate) mod ffi;
mod props;
#[cfg(feature = "render-gl")]
pub mod render;

pub use engine::{MpvEngine, RenderSource};
pub use props::OBSERVED_PROPERTIES;

/// 渲染模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    /// Mode A：进程内 GL render API（默认；Linux/Windows 统一）。
    EmbeddedGl,
    /// 外部播放窗口（兜底；非默认）。
    ExternalWindow,
}

/// MpvEngine 配置。
#[derive(Debug, Clone)]
pub struct MpvConfig {
    /// 指定 libmpv 路径；None=自动探测。
    pub lib_path: Option<PathBuf>,
    /// hwdec（auto-safe / vaapi / d3d11va / vulkan / no ...）。
    pub hwdec: String,
    /// 网络缓存秒数（mpv 内部，代理模式可小）。
    pub network_cache_secs: u64,
    /// 渲染模式。
    pub render_mode: RenderMode,
    /// 启动额外 mpv 选项（调试用，如 `vo=null` / `ao=null`，最后应用可覆盖默认值）。
    pub extra_options: Vec<String>,
    /// 字幕字体目录（`--sub-fonts-dir`，open 前设置；None=用 mpv 默认字体搜索）。
    ///
    /// mpv 在初始化/换文件时扫描该目录；运行期修改需重建引擎（见 `render_probe`/GUI 入口）。
    pub fonts_dir: Option<PathBuf>,
    /// 外挂字幕自动加载模式（`--sub-auto`：`no` / `exact` / `fuzzy` / `all`）。
    ///
    /// 默认 `no`：本应用由上层显式注入字幕（避免同名字幕乱入）；
    /// T9「同名自动加载」可设为 `fuzzy`。
    pub sub_auto: String,
}

impl Default for MpvConfig {
    fn default() -> Self {
        Self {
            lib_path: None,
            hwdec: "auto-safe".into(),
            network_cache_secs: 10,
            render_mode: RenderMode::EmbeddedGl,
            extra_options: Vec::new(),
            fonts_dir: None,
            sub_auto: "no".into(),
        }
    }
}

/// libmpv 版本信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MpvVersion {
    /// client API major（mpv 0.41.0 = 2）。
    pub api_major: u32,
    /// client API minor（mpv 0.41.0 = 5）。
    pub api_minor: u32,
    /// mpv 自身版本串（如 `mpv v0.41.0`），拿到实例后才有。
    pub mpv: Option<String>,
}

impl MpvVersion {
    /// `"client API 2.5"` 形式。
    pub fn api_string(&self) -> String {
        format!("client API {}.{}", self.api_major, self.api_minor)
    }
}

impl std::fmt::Display for MpvVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.mpv {
            Some(v) => write!(f, "{v}（{}）", self.api_string()),
            None => write!(f, "{}", self.api_string()),
        }
    }
}

/// 已加载的 libmpv 动态库 + 符号表 + 版本。
pub struct MpvLibrary {
    _lib: libloading::Library,
    api: Arc<ffi::MpvApi>,
    path: PathBuf,
    api_major: u32,
    api_minor: u32,
    /// 加载后探测到的 mpv 版本串（`None` 表示尚未创建实例）。
    mpv_version: std::sync::Mutex<Option<String>>,
}

impl std::fmt::Debug for MpvLibrary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MpvLibrary")
            .field("path", &self.path)
            .field("api_major", &self.api_major)
            .field("api_minor", &self.api_minor)
            .finish()
    }
}

impl MpvLibrary {
    /// 实际加载的动态库路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn api(&self) -> &Arc<ffi::MpvApi> {
        &self.api
    }

    /// 探测到的版本（mpv 版本串可能需要等引擎实例化后才有）。
    pub fn version(&self) -> MpvVersion {
        MpvVersion {
            api_major: self.api_major,
            api_minor: self.api_minor,
            mpv: self
                .mpv_version
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }

    pub(crate) fn set_mpv_version(&self, version: String) {
        *self.mpv_version.lock().unwrap_or_else(|e| e.into_inner()) = Some(version);
    }

    /// 一行可读描述，用于诊断面板/日志。
    pub fn describe(&self) -> String {
        format!(
            "{}（{}，{}）",
            self.version(),
            self.path.display(),
            self.api.api_string_hint()
        )
    }
}

impl ffi::MpvApi {
    fn api_string_hint(&self) -> String {
        let (major, minor) = self.api_version();
        format!("client API {major}.{minor}")
    }
}

/// 最低可接受的 client API 版本（mpv 0.35+；本项目 pin 0.41.0 = API 2.5）。
const MIN_API_MAJOR: u32 = 2;

/// 运行时目录名（随包发布的 libmpv 放这里）。
pub const RUNTIME_DIR_NAME: &str = "播放器运行时";

/// libmpv 候选来源（诊断/发布验收用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateSource {
    /// 显式指定（[`MpvConfig::lib_path`] / [`load_library`] 的 `path` 参数）
    Explicit,
    /// 环境变量 `WP_MPV_LIB`
    Env,
    /// 可执行文件同级的 `播放器运行时/`（随包 LGPL 构建，发布形态首选）
    BundledRuntime,
    /// 可执行文件同级（某些打包方式直接把库放 exe 旁）
    ExeDir,
    /// 系统库（发行版包管理安装，兜底）
    System,
}

impl CandidateSource {
    /// 人读标签（日志 / 发布验收输出用）。
    pub fn label(self) -> &'static str {
        match self {
            CandidateSource::Explicit => "显式指定",
            CandidateSource::Env => "WP_MPV_LIB",
            CandidateSource::BundledRuntime => "随包运行时",
            CandidateSource::ExeDir => "程序目录",
            CandidateSource::System => "系统库",
        }
    }
}

/// 一个 libmpv 候选：路径 + 来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryCandidate {
    pub path: PathBuf,
    pub source: CandidateSource,
}

/// 可执行文件同级的 `播放器运行时/` 目录（不存在也返回路径）。
pub fn bundled_runtime_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.join(RUNTIME_DIR_NAME);
    Some(dir)
}

/// 纯函数：按优先级生成候选列表（不读环境/当前进程，便于单测伪造路径）。
///
/// 顺序（发布形态）：`WP_MPV_LIB` → `exe_dir/播放器运行时/lib/libmpv.so.2` →
/// `exe_dir/播放器运行时/libmpv.so.2` → `exe_dir/libmpv.so.2` → 系统库。
fn candidates_from(wp_mpv_lib: Option<&str>, exe_dir: Option<&Path>) -> Vec<LibraryCandidate> {
    let mut out: Vec<LibraryCandidate> = Vec::new();
    let push = |out: &mut Vec<LibraryCandidate>, path: PathBuf, source: CandidateSource| {
        out.push(LibraryCandidate { path, source });
    };

    // 1) 环境变量显式覆盖（开发调试 / 自定义构建 / 发布验收）
    if let Some(p) = wp_mpv_lib.map(str::trim).filter(|s| !s.is_empty()) {
        push(&mut out, PathBuf::from(p), CandidateSource::Env);
    }

    // 2) 随包运行时：相对「可执行文件所在目录」解析（dev=target/debug、发布=安装目录）
    #[cfg(target_os = "linux")]
    if let Some(dir) = exe_dir {
        let rt = dir.join(RUNTIME_DIR_NAME);
        for path in [
            rt.join("lib").join("libmpv.so.2"),
            rt.join("lib").join("libmpv.so"),
            rt.join("libmpv.so.2"),
            rt.join("libmpv.so"),
        ] {
            push(&mut out, path, CandidateSource::BundledRuntime);
        }
    }

    // 3) 可执行文件同级
    #[cfg(target_os = "linux")]
    if let Some(dir) = exe_dir {
        for name in ["libmpv.so.2", "libmpv.so"] {
            push(&mut out, dir.join(name), CandidateSource::ExeDir);
        }
    }

    // 4) 系统库兜底
    #[cfg(target_os = "linux")]
    for p in [
        "/usr/lib/libmpv.so.2",
        "/usr/lib/libmpv.so",
        "/usr/local/lib/libmpv.so.2",
        "/usr/local/lib/libmpv.so",
        "/usr/lib/x86_64-linux-gnu/libmpv.so.2",
        "/usr/lib/x86_64-linux-gnu/libmpv.so",
        "/usr/lib/aarch64-linux-gnu/libmpv.so.2",
    ] {
        push(&mut out, PathBuf::from(p), CandidateSource::System);
    }

    #[cfg(target_os = "windows")]
    if let Some(dir) = exe_dir {
        let rt = dir.join(RUNTIME_DIR_NAME);
        for name in ["libmpv-2.dll", "libmpv.dll", "mpv-2.dll"] {
            // 程序目录播放器运行时/lib 优先 → 播放器运行时 根目录 → exe 旁边 → 系统 PATH（由加载器兜底）
            for path in [rt.join("lib").join(name), rt.join(name), dir.join(name)] {
                let source = if path.starts_with(&rt) {
                    CandidateSource::BundledRuntime
                } else {
                    CandidateSource::ExeDir
                };
                push(&mut out, path, source);
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(dir) = exe_dir {
            let rt = dir.join(RUNTIME_DIR_NAME);
            for name in ["libmpv.2.dylib", "libmpv.dylib"] {
                for path in [rt.join("lib").join(name), rt.join(name), dir.join(name)] {
                    let source = if path.starts_with(&rt) {
                        CandidateSource::BundledRuntime
                    } else {
                        CandidateSource::ExeDir
                    };
                    push(&mut out, path, source);
                }
            }
        }
        for p in [
            "/opt/homebrew/lib/libmpv.2.dylib",
            "/usr/local/lib/libmpv.2.dylib",
        ] {
            push(&mut out, PathBuf::from(p), CandidateSource::System);
        }
    }

    out
}

/// 按优先级列出 libmpv 候选（含来源；读取当前环境与可执行文件路径）。
pub fn library_candidates_annotated() -> Vec<LibraryCandidate> {
    let env_val = std::env::var("WP_MPV_LIB").ok();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf));
    candidates_from(env_val.as_deref(), exe_dir.as_deref())
}

/// 按优先级列出 libmpv 候选路径（不检查存在性，由加载流程逐个尝试）。
pub fn library_candidates() -> Vec<PathBuf> {
    library_candidates_annotated()
        .into_iter()
        .map(|c| c.path)
        .collect()
}

/// 加载 libmpv。
///
/// `path = None` 时按 [`library_candidates`] 顺序尝试；失败信息包含**每个**尝试过的
/// 路径与原因，便于用户排查「没装 mpv / 装的是不能渲染的精简版」。
pub fn load_library(path: Option<&Path>) -> Result<Arc<MpvLibrary>> {
    let candidates: Vec<LibraryCandidate> = match path {
        Some(p) => vec![LibraryCandidate {
            path: p.to_path_buf(),
            source: CandidateSource::Explicit,
        }],
        None => library_candidates_annotated(),
    };
    if candidates.is_empty() {
        return Err(Error::unsupported(
            "未找到 libmpv：请安装 mpv（Linux: pacman -S mpv / apt install libmpv2，\
             Windows: 放置 libmpv-2.dll 到程序目录）或设置 WP_MPV_LIB 指向动态库",
        ));
    }
    let mut failures = Vec::with_capacity(candidates.len());
    for cand in &candidates {
        if !cand.path.exists() && path.is_none() {
            // 自动探测时静默跳过不存在的候选，减少噪音
            continue;
        }
        match unsafe { MpvLibrary::open(&cand.path) } {
            Ok(lib) => {
                // 诊断/发布验收关键证据：记录**实际加载**的库路径与来源，
                // 便于确认「随包运行时」被使用、而非误用系统库。
                wp_core::log_info!(
                    "播放器",
                    "已加载 libmpv：{}（来源：{}，{}）",
                    lib.path().display(),
                    cand.source.label(),
                    lib.version().api_string()
                );
                return Ok(Arc::new(lib));
            }
            Err(e) => failures.push(format!("{}：{e}", cand.path.display())),
        }
    }
    let detail = if failures.is_empty() {
        "候选路径均不存在".to_string()
    } else {
        failures.join("\n  ")
    };
    Err(Error::unsupported(format!(
        "无法加载 libmpv（已尝试 {} 个路径）：\n  {detail}",
        candidates.len()
    )))
}

impl MpvLibrary {
    /// # Safety
    /// 调用方需保证动态库 ABI 与头文件一致（由 mpv 官方 soname 保证）。
    unsafe fn open(path: &Path) -> Result<Self> {
        let lib = unsafe { libloading::Library::new(path) }.map_err(|e| {
            Error::unsupported(format!("无法加载动态库（{}）：{e}", path.display()))
        })?;
        let lib_label = path.display().to_string();
        let api = unsafe { ffi::MpvApi::load(&lib, &lib_label)? };
        let (api_major, api_minor) = api.api_version();
        if api_major < MIN_API_MAJOR {
            return Err(Error::unsupported(format!(
                "libmpv client API 版本过低（{api_major}.{api_minor}，{}）：\
                 需要 ≥ {MIN_API_MAJOR}.0（mpv 0.35+；本项目 pin mpv 0.41.0 = API 2.5）",
                path.display()
            )));
        }
        Ok(Self {
            _lib: lib,
            api: Arc::new(api),
            path: path.to_path_buf(),
            api_major,
            api_minor,
            mpv_version: std::sync::Mutex::new(None),
        })
    }
}

/// 探测系统/随包 libmpv 的版本（找不到返回 `None`）。
///
/// 只做「动态库能加载 + client API 版本可见」，不创建播放实例，可在启动时廉价调用。
pub fn probe_version() -> Option<String> {
    let lib = load_library(None).ok()?;
    Some(lib.describe())
}

/// 探测 libmpv 并返回（可读描述, **实际加载路径**）。
///
/// 发布验收/启动自检用：可断言加载的是 `播放器运行时/lib/libmpv.so.2` 而不是系统库。
pub fn probe_version_with_path() -> Option<(String, PathBuf)> {
    let lib = load_library(None).ok()?;
    Some((lib.describe(), lib.path().to_path_buf()))
}

// ============================ 测试 ============================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_order_env_bundled_exedir_system() {
        // 纯函数 + 伪造路径：不读真实环境、不需要真实库。
        let exe = Path::new("/opt/fake-app");
        let cands = candidates_from(Some("/custom/libmpv.so.2"), Some(exe));

        // 1) env 永远第一
        assert_eq!(cands[0].source, CandidateSource::Env);
        assert_eq!(cands[0].path, PathBuf::from("/custom/libmpv.so.2"));

        #[cfg(target_os = "linux")]
        {
            // 2) 随包运行时必须是「次优先」，且相对 exe 目录解析到 播放器运行时/lib/libmpv.so.2
            let bundled: Vec<&LibraryCandidate> = cands
                .iter()
                .filter(|c| c.source == CandidateSource::BundledRuntime)
                .collect();
            assert!(!bundled.is_empty(), "Linux 候选应包含随包运行时");
            assert_eq!(
                bundled[0].path,
                PathBuf::from("/opt/fake-app/播放器运行时/lib/libmpv.so.2")
            );
            // 3) 程序目录 → 系统库兜底，且顺序严格：随包全部在系统库之前
            let first_system = cands
                .iter()
                .position(|c| c.source == CandidateSource::System)
                .expect("Linux 候选应包含系统库兜底");
            let last_bundled = cands
                .iter()
                .rposition(|c| c.source == CandidateSource::BundledRuntime)
                .unwrap();
            assert!(
                last_bundled < first_system,
                "随包运行时必须排在系统库之前（发布形态优先用随包库）"
            );
            assert!(cands.iter().any(|c| {
                c.source == CandidateSource::System
                    && c.path == PathBuf::from("/usr/lib/libmpv.so.2")
            }));
            assert!(cands.iter().any(|c| c.source == CandidateSource::ExeDir));
        }
    }

    #[test]
    fn candidates_without_env_prefers_bundled_over_system() {
        let cands = candidates_from(None, Some(Path::new("/opt/published-app")));
        assert!(!cands.is_empty());
        #[cfg(target_os = "linux")]
        {
            assert_eq!(
                cands[0].source,
                CandidateSource::BundledRuntime,
                "没有 WP_MPV_LIB 时，随包运行时应是第一候选"
            );
            assert!(cands[0].path.ends_with("播放器运行时/lib/libmpv.so.2"));
            // 系统库仍保留在末尾兜底
            assert_eq!(
                cands.last().map(|c| c.source),
                Some(CandidateSource::System)
            );
        }
    }

    #[test]
    fn candidates_without_exe_keep_system_fallback() {
        // current_exe 拿不到时（异常环境）仍要能兜底系统库
        let cands = candidates_from(None, None);
        #[cfg(target_os = "linux")]
        {
            assert!(cands.iter().any(|c| c.source == CandidateSource::System));
            assert!(cands.iter().all(|c| c.source == CandidateSource::System));
        }
        #[cfg(not(target_os = "linux"))]
        assert!(cands.is_empty());
    }

    #[test]
    fn candidate_source_labels_are_readable() {
        assert_eq!(CandidateSource::Env.label(), "WP_MPV_LIB");
        assert_eq!(CandidateSource::BundledRuntime.label(), "随包运行时");
        assert_eq!(CandidateSource::ExeDir.label(), "程序目录");
        assert_eq!(CandidateSource::System.label(), "系统库");
        assert_eq!(CandidateSource::Explicit.label(), "显式指定");
    }

    #[test]
    fn library_candidates_do_not_panic_and_are_enumerable() {
        let cands = library_candidates();
        assert!(!cands.is_empty(), "默认候选列表不应为空");
        // 带来源的版本与纯路径版本一一对应
        let annotated = library_candidates_annotated();
        assert_eq!(annotated.len(), cands.len());
        assert!(annotated
            .iter()
            .zip(cands.iter())
            .all(|(a, p)| &a.path == p));
    }

    #[test]
    fn bundled_library_loads_and_reports_its_path_when_present() {
        // 发布验收：真实加载随包 libmpv（若仓库里已生成 播放器运行时/），断言实际路径。
        // 干净 checkout 下产物不存在 → 跳过（打包脚本生成后再跑即可覆盖）。
        let bundled =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../播放器运行时/lib/libmpv.so.2");
        if !bundled.exists() {
            eprintln!(
                "跳过：{} 不存在（先跑 scripts/package-player-runtime-linux.sh）",
                bundled.display()
            );
            return;
        }
        let lib = load_library(Some(&bundled)).expect("随包 libmpv 应可加载");
        assert_eq!(lib.path(), bundled.as_path(), "实际加载路径应是随包库");
        assert!(lib.version().api_major >= 2);
        assert!(lib.describe().contains("client API"));
    }

    #[test]
    fn probe_version_with_path_reports_existing_path() {
        // 环境里有 libmpv（系统或随包）时校验：返回的路径必须真实存在。
        if let Some((desc, path)) = probe_version_with_path() {
            assert!(
                path.exists(),
                "probe_version_with_path 返回路径应存在：{}",
                path.display()
            );
            assert!(desc.contains("client API"), "描述应包含 client API：{desc}");
            assert!(desc.contains(&path.display().to_string()));
        }
    }

    #[test]
    fn version_display_and_api_string() {
        let v = MpvVersion {
            api_major: 2,
            api_minor: 5,
            mpv: None,
        };
        assert_eq!(v.api_string(), "client API 2.5");
        assert_eq!(v.to_string(), "client API 2.5");
        let v2 = MpvVersion {
            mpv: Some("mpv v0.41.0".into()),
            ..v
        };
        assert!(v2.to_string().contains("mpv v0.41.0"));
    }

    #[test]
    fn load_library_with_missing_explicit_path_errs_readably() {
        let r = load_library(Some(Path::new("/nonexistent/libmpv.so.2")));
        let err = r.expect_err("不存在的路径必须报错");
        let msg = err.to_string();
        assert!(msg.contains("libmpv"), "错误信息应包含 libmpv：{msg}");
    }

    #[test]
    fn load_library_default_candidates_do_not_panic() {
        // 有 libmpv 时应成功、无 libmpv 时应给出可读错误；两种环境都不能 panic。
        match load_library(None) {
            Ok(lib) => {
                assert!(lib.version().api_major >= 2);
                assert!(lib.describe().contains("client API"));
            }
            Err(e) => assert!(e.to_string().contains("libmpv")),
        }
    }

    #[test]
    fn bundled_runtime_dir_under_exe() {
        if let Some(dir) = bundled_runtime_dir() {
            assert!(dir.ends_with(RUNTIME_DIR_NAME));
        }
    }
}
