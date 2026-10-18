//! 应用主状态：驱动注册、事件循环、主窗口布局、页面/对话框调度。

use crate::pages;
use crate::theme::{self, Theme};
use crate::{dialogs, fonts};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wp_core::config::{AppConfig, DriveConfig};
use wp_core::guard::{GuardDecision, GuardManager};
use wp_core::model::*;
use wp_core::util::{norm_path, sort_entries, SortBy};
use wp_core::{log_error, log_info, log_warn, Paths};
use wp_drive::{Drive, DriveInit};
use wp_transfer::{EngineOptions, TransferEngine, UploadGuardHook};

/// 左侧功能页。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Browser,
    Transfers,
    Tools,
    Guard,
    Logs,
    Settings,
    About,
}

impl Page {
    pub fn label(self) -> &'static str {
        match self {
            Page::Browser => "网盘",
            Page::Transfers => "传输",
            Page::Tools => "工具",
            Page::Guard => "敏感词",
            Page::Logs => "日志",
            Page::Settings => "设置",
            Page::About => "关于",
        }
    }
    pub fn icon(self) -> &'static str {
        match self {
            Page::Browser => "☁",
            Page::Transfers => "⇅",
            Page::Tools => "🛠",
            Page::Guard => "🛡",
            Page::Logs => "📋",
            Page::Settings => "⚙",
            Page::About => "ℹ",
        }
    }
}

/// 账号检查状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountStatus {
    Unknown,
    Checking,
    LoggedIn,
    LoggedOut,
    Failed(String),
}

impl AccountStatus {
    pub fn label(&self) -> String {
        match self {
            AccountStatus::Unknown => "未检查".into(),
            AccountStatus::Checking => "检查中…".into(),
            AccountStatus::LoggedIn => "已登录".into(),
            AccountStatus::LoggedOut => "未登录".into(),
            AccountStatus::Failed(e) => format!("状态失败：{e}"),
        }
    }
}

/// 一个网盘槽位（配置 + 运行时驱动）。
pub struct DriveSlot {
    pub cfg: DriveConfig,
    pub drive: Option<Arc<dyn Drive>>,
    pub account: Option<AccountInfo>,
    pub status: AccountStatus,
    /// 创建失败原因（适配器初始化错误）
    pub init_error: Option<String>,
}

impl DriveSlot {
    pub fn id(&self) -> &str {
        &self.cfg.id
    }
    pub fn kind(&self) -> DriveKind {
        self.cfg.kind
    }
    pub fn display(&self) -> &str {
        if self.cfg.name.trim().is_empty() {
            self.cfg.kind.label()
        } else {
            &self.cfg.name
        }
    }
    pub fn caps(&self) -> Option<wp_drive::DriveCaps> {
        self.drive.as_ref().map(|d| d.caps())
    }
}

/// 文件浏览状态（每个网盘各自保留位置/选择）。
pub struct BrowseState {
    pub path: String,
    pub entries: std::sync::Arc<Vec<RemoteEntry>>,
    pub selected: BTreeSet<String>,
    pub loading: bool,
    pub error: Option<String>,
    pub filter: String,
    pub sort: SortBy,
    pub desc: bool,
    /// 请求序号：防止旧请求覆盖新目录
    pub token: u64,
    /// 后退/前进历史
    pub back: Vec<String>,
    pub forward: Vec<String>,
}

impl Default for BrowseState {
    fn default() -> Self {
        Self {
            path: "/".into(),
            entries: std::sync::Arc::new(Vec::new()),
            selected: BTreeSet::new(),
            loading: false,
            error: None,
            filter: String::new(),
            sort: SortBy::Name,
            desc: false,
            token: 0,
            back: Vec::new(),
            forward: Vec::new(),
        }
    }
}

/// 提示条类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warn,
    Error,
}

pub struct Toast {
    pub kind: ToastKind,
    pub text: String,
    pub at: Instant,
}

/// 异步操作回传事件。
pub enum Event {
    Listed {
        drive: String,
        path: String,
        token: u64,
        result: Result<Vec<RemoteEntry>, wp_core::Error>,
    },
    AccountChecked {
        drive: String,
        result: Result<AccountInfo, wp_core::Error>,
    },
    QrStarted {
        drive: String,
        result: Result<wp_drive::QrChallenge, wp_core::Error>,
    },
    QrPolled {
        drive: String,
        result: Result<wp_drive::QrPoll, wp_core::Error>,
    },
    LoggedIn {
        drive: String,
        result: Result<AccountInfo, wp_core::Error>,
    },
    LoggedOut {
        drive: String,
        result: Result<(), wp_core::Error>,
    },
    /// 一般操作（删除/移动/重命名/建目录/退出登录）完成
    OpDone {
        message: String,
        refresh: bool,
        result: Result<String, wp_core::Error>,
    },
    ShareCreated(Result<ShareInfo, wp_core::Error>),
    DirectLink {
        path: String,
        result: Result<DirectLink, wp_core::Error>,
    },
    /// 移动/复制目标目录选择器：列目录结果
    PickerListed {
        path: String,
        result: Result<Vec<RemoteEntry>, wp_core::Error>,
    },
    // ── 工具页 ──
    RecycleListed {
        drive: String,
        result: Result<Vec<RecycleEntry>, wp_core::Error>,
    },
    RecycleOp {
        message: String,
        result: Result<String, wp_core::Error>,
    },
    CloudTasksListed {
        drive: String,
        result: Result<Vec<CloudTask>, wp_core::Error>,
    },
    CloudAdded {
        result: Result<String, wp_core::Error>,
    },
    RecordsListed {
        drive: String,
        result: Result<Vec<UploadRecord>, wp_core::Error>,
    },
    SharesListed {
        drive: String,
        result: Result<Vec<ShareInfo>, wp_core::Error>,
    },
    ShareInfoGot {
        result: Result<ShareDetail, wp_core::Error>,
    },
    ShareSaved {
        result: Result<String, wp_core::Error>,
    },
    CategoriesListed {
        drive: String,
        result: Result<Vec<RemoteEntry>, wp_core::Error>,
    },
    // ── 缩略图 ──
    ThumbLoaded {
        key: String,
        result: Result<Vec<u8>, wp_core::Error>,
    },
}

/// 共享守卫钩子（传输引擎上传前调用）。
pub struct SharedGuard(pub Arc<Mutex<GuardManager>>);

impl UploadGuardHook for SharedGuard {
    fn precheck(&self, instance: &str, kind: DriveKind, name: &str) -> (GuardDecision, String) {
        let mut g = match self.0.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        g.precheck(instance, kind, name)
    }
}

/// 登录对话框状态。
pub struct LoginDialog {
    pub drive: String,
    pub caps: AuthCaps,
    pub busy: bool,
    pub error: String,
    pub qr: Option<wp_drive::QrChallenge>,
    pub qr_texture: Option<egui::TextureHandle>,
    pub qr_state: QrState,
    pub qr_message: String,
    pub qr_timer: Instant,
    pub poll_inflight: bool,
    pub cookie_text: String,
    pub phone: String,
    pub sms_code: String,
    pub sms_hint: String,
    pub token_text: String,
    pub tab: LoginTab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginTab {
    Qr,
    Cookie,
    Sms,
    Token,
}

impl LoginDialog {
    pub fn new(drive: String) -> Self {
        Self {
            drive,
            caps: AuthCaps::default(),
            busy: false,
            error: String::new(),
            qr: None,
            qr_texture: None,
            qr_state: QrState::Waiting,
            qr_message: String::new(),
            qr_timer: Instant::now(),
            poll_inflight: false,
            cookie_text: String::new(),
            phone: String::new(),
            sms_code: String::new(),
            sms_hint: String::new(),
            token_text: String::new(),
            tab: LoginTab::Qr,
        }
    }
}

/// 新建文件夹 / 重命名输入框。
pub struct InputDialog {
    pub title: String,
    pub label: String,
    pub value: String,
    pub on_ok: InputAction,
}

#[derive(Debug, Clone)]
pub enum InputAction {
    NewFolder { drive: String, dir: String },
    Rename { drive: String, path: String },
}

pub struct ConfirmDialog {
    pub title: String,
    pub message: String,
    pub danger: bool,
    pub action: ConfirmAction,
}

#[derive(Debug, Clone)]
pub enum ConfirmAction {
    Delete { drive: String, paths: Vec<String> },
    CancelTasks { ids: Vec<String> },
}

/// 本地文件选择器。
pub struct FilePicker {
    pub path: PathBuf,
    pub entries: Vec<LocalEntry>,
    pub selected: BTreeSet<PathBuf>,
    pub manual: String,
    pub error: String,
    pub dir_mode: bool,
}

#[derive(Debug, Clone)]
pub struct LocalEntry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// 云端目录选择器（移动/复制/跨盘目标）。
pub struct DirPicker {
    pub drive: String,
    pub path: String,
    pub entries: Vec<RemoteEntry>,
    pub loading: bool,
    pub error: String,
    pub action: DirPickerAction,
}

#[derive(Debug, Clone)]
pub enum DirPickerAction {
    Move { paths: Vec<String> },
    Copy { paths: Vec<String> },
}

pub struct ShareDialog {
    pub drive: String,
    pub paths: Vec<String>,
    pub expire_days: u32,
    pub password: String,
    pub busy: bool,
    pub result: Option<ShareInfo>,
    pub error: String,
}

pub struct DetailsDialog {
    pub entry: RemoteEntry,
}

pub struct CrossCopyDialog {
    pub src_drive: String,
    pub paths: Vec<String>,
    pub dst_drive: String,
    pub dst_dir: String,
}

pub enum Dialog {
    Login(LoginDialog),
    Input(InputDialog),
    Confirm(ConfirmDialog),
    DirPicker(DirPicker),
    Share(ShareDialog),
    Details(DetailsDialog),
    CrossCopy(CrossCopyDialog),
}

/// 工具页标签。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolsTab {
    Recycle,
    Cloud,
    Records,
    Shares,
    Categories,
}

impl ToolsTab {
    pub fn label(self) -> &'static str {
        match self {
            ToolsTab::Recycle => "回收站",
            ToolsTab::Cloud => "云任务",
            ToolsTab::Records => "上传记录",
            ToolsTab::Shares => "分享管理 / 转存",
            ToolsTab::Categories => "分类浏览",
        }
    }
}

/// 一组异步加载的数据。
#[derive(Debug)]
pub struct LoadBox<T> {
    pub data: Vec<T>,
    pub loading: bool,
    pub error: String,
}

impl<T> Default for LoadBox<T> {
    fn default() -> Self {
        Self {
            data: Vec::new(),
            loading: false,
            error: String::new(),
        }
    }
}

/// 工具页状态。
#[derive(Default)]
pub struct ToolsState {
    pub tab: Option<ToolsTab>,
    /// 已加载的 key：`drive:tab:extra`，用于判断是否需要（重新）加载
    pub loaded_key: String,
    pub recycle: LoadBox<RecycleEntry>,
    pub recycle_selected: BTreeSet<String>,
    pub cloud_tasks: LoadBox<CloudTask>,
    pub cloud_url: String,
    pub cloud_dest: String,
    pub cloud_busy: bool,
    pub records: LoadBox<UploadRecord>,
    pub shares: LoadBox<ShareInfo>,
    pub share_url: String,
    pub share_pwd: String,
    pub share_dest: String,
    pub share_detail: Option<ShareDetail>,
    pub share_busy: bool,
    pub categories: LoadBox<RemoteEntry>,
    pub category: String,
}

/// 全局应用状态。
pub struct App {
    pub paths: Paths,
    pub cfg: AppConfig,
    pub cfg_path: PathBuf,
    pub drives: Vec<DriveSlot>,
    pub active: usize,
    pub page: Page,
    pub browse: HashMap<String, BrowseState>,
    pub transfer: TransferEngine,
    pub guard: Arc<Mutex<GuardManager>>,
    pub theme: Theme,
    pub applied_theme: String,
    pub dialog: Option<Dialog>,
    pub file_picker: Option<FilePicker>,
    pub toasts: Vec<Toast>,
    pub log_last_seq: u64,
    pub log_lines: Vec<wp_core::logging::LogLine>,
    pub log_level_filter: wp_core::logging::LogLevel,
    pub log_search: String,
    pub log_auto_scroll: bool,
    /// 设置页草稿（保存后写盘）
    pub settings_draft: SettingsDraft,
    pub settings_draft_kind: DriveKind,
    pub tools: ToolsState,
    /// 文件列表缩略图缓存（图片/视频预览）
    pub thumbs: crate::thumbs::ThumbCache,
    pub guard_new_word: String,
    pub guard_new_instance_word: String,
    pub loading_accounts: bool,
    pub event_tx: Sender<Event>,
    pub event_rx: Receiver<Event>,
    pub rt: Option<tokio::runtime::Handle>,
    pub ctx: Option<egui::Context>,
    /// 开发/验收：启动参数
    pub dev: DevOptions,
    pub frames: u64,
    pub last_transfer_tick: Instant,
    pub transfer_snapshot: Vec<TransferTask>,
    pub transfer_stats: TransferStats,
}

#[derive(Debug, Clone, Default)]
pub struct SettingsDraft {
    pub small_threshold_mb: u64,
    pub small_concurrency: usize,
    pub large_concurrency: usize,
    pub retry: u32,
    pub adaptive: bool,
    pub speed_limit_mbps: u64,
    pub segments: usize,
    pub segment_threshold_mb: u64,
    pub resume: bool,
    pub theme: String,
    pub font_path: String,
    pub download_dir: String,
    pub guard_enabled: bool,
    pub guard_action: wp_core::config::GuardAction,
    pub keep_failed_temp: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DevOptions {
    pub screenshot: Option<PathBuf>,
    pub initial_drive: Option<String>,
    pub initial_page: Option<String>,
    pub exit_after_frames: Option<u64>,
    /// 验收用：启动即打开某个对话框（login/newfolder/rename/delete/move/share/crosscopy/details/filepicker）
    pub dialog: Option<String>,
    /// 验收用：启动后直接导航到的逻辑路径
    pub initial_path: Option<String>,
    /// 验收用：工具页默认标签（recycle/cloud/records/shares/categories）
    pub tools_tab: Option<String>,
    /// 验收用：第 4 帧时阻塞等待毫秒数（真实账号截图等待网络数据用）
    pub wait_ms: Option<u64>,
}

impl App {
    pub fn new(paths: Paths, cfg: AppConfig, rt: tokio::runtime::Handle, dev: DevOptions) -> Self {
        paths.ensure();
        let cfg_path = paths.config_file();
        // 日志
        wp_core::logging::init(
            &paths.log_dir(),
            wp_core::logging::LogLevel::parse(&cfg.ui.log_level),
            true,
        );
        log_info!(
            "应用",
            "启动 {} v{}",
            wp_core::APP_NAME,
            wp_core::APP_VERSION
        );
        log_info!(
            "应用",
            "数据目录：{}（{}）",
            paths.root.display(),
            if paths.portable {
                "绿色模式"
            } else {
                "用户目录模式"
            }
        );
        // 主题
        let theme = theme::by_name(&cfg.ui.theme);
        // 敏感词守卫
        let guard = Arc::new(Mutex::new(GuardManager::load(
            &paths.sensitive_file(),
            cfg.guard.action,
            cfg.guard.enabled,
        )));
        // 传输引擎
        let download_dir = cfg.resolved_download_dir(&paths);
        let options = EngineOptions {
            transfer: cfg.transfer.clone(),
            download: cfg.download.clone(),
            cache_dir: paths.cache_dir(),
            tasks_file: paths.tasks_file(),
            download_dir,
            keep_failed_temp: cfg.keep_failed_temp,
            guard: Some(Arc::new(SharedGuard(guard.clone()))),
        };
        let mut transfer = TransferEngine::new(options);
        transfer.set_runtime(rt.clone());
        transfer.load();
        // 网盘实例
        let mut drives = Vec::new();
        for dcfg in cfg.drives.clone() {
            if !dcfg.enabled {
                continue;
            }
            let slot = Self::build_slot(&paths, &dcfg);
            drives.push(slot);
        }
        let active = dev
            .initial_drive
            .as_ref()
            .and_then(|id| {
                cfg.drives
                    .iter()
                    .position(|d| &d.id == id)
                    .filter(|&i| drives.get(i).is_some())
            })
            .or_else(|| {
                cfg.drives
                    .iter()
                    .position(|d| d.id == cfg.ui.last_drive)
                    .filter(|&i| drives.get(i).is_some())
            })
            .unwrap_or(0);
        let page = match dev.initial_page.as_deref() {
            Some("transfers") => Page::Transfers,
            Some("tools") => Page::Tools,
            Some("settings") => Page::Settings,
            Some("logs") => Page::Logs,
            Some("guard") => Page::Guard,
            Some("about") => Page::About,
            _ => Page::Browser,
        };
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let settings_draft = settings_from_config(&cfg);
        let mut app = Self {
            paths,
            cfg,
            cfg_path,
            drives,
            active,
            page,
            browse: HashMap::new(),
            transfer,
            guard,
            theme,
            applied_theme: String::new(),
            dialog: None,
            file_picker: None,
            toasts: Vec::new(),
            log_last_seq: 0,
            log_lines: Vec::new(),
            log_level_filter: wp_core::logging::LogLevel::Debug,
            log_search: String::new(),
            log_auto_scroll: true,
            settings_draft,
            settings_draft_kind: DriveKind::Baidu,
            tools: ToolsState {
                tab: Some(ToolsTab::Recycle),
                ..Default::default()
            },
            thumbs: crate::thumbs::ThumbCache::default(),
            guard_new_word: String::new(),
            guard_new_instance_word: String::new(),
            loading_accounts: false,
            event_tx,
            event_rx,
            rt: Some(rt),
            ctx: None,
            dev,
            frames: 0,
            last_transfer_tick: Instant::now(),
            transfer_snapshot: Vec::new(),
            transfer_stats: TransferStats::default(),
        };
        app.refresh_all_accounts();
        if let Some(slot) = app.drives.get(app.active) {
            let id = slot.id().to_string();
            app.load_dir_for(&id, "/");
        }
        if let Some(tab) = app.dev.tools_tab.clone() {
            app.tools.tab = Some(match tab.as_str() {
                "cloud" => ToolsTab::Cloud,
                "records" => ToolsTab::Records,
                "shares" => ToolsTab::Shares,
                "categories" => ToolsTab::Categories,
                _ => ToolsTab::Recycle,
            });
            app.tools.loaded_key.clear();
        }
        if let Some(path) = app.dev.initial_path.clone() {
            let id = app.active_drive_id();
            app.navigate_drive(&id, &path, false);
        }
        if let Some(name) = app.dev.dialog.clone() {
            app.open_dev_dialog(&name);
        }
        app
    }

    /// 验收/截图用：按名字打开一个代表性对话框。
    fn open_dev_dialog(&mut self, name: &str) {
        let drive = self.active_drive_id();
        let sample = RemoteEntry::file("示例影片-4K.mp4", "/影视/示例影片-4K.mp4", 4_500_000_000)
            .with_id("fake-0000")
            .with_modified(wp_core::util::now_ts() - 86400)
            // 验收截图：详情弹窗里展示缩略图预览（演示盘支持）
            .with_thumbnail(Some("demo-thumb://影视/示例影片-4K.mp4".into()));
        match name {
            "login" => self.open_login_dialog(),
            "newfolder" => {
                self.dialog = Some(Dialog::Input(InputDialog {
                    title: "新建文件夹".into(),
                    label: "文件夹名称".into(),
                    value: "新建文件夹".into(),
                    on_ok: InputAction::NewFolder {
                        drive,
                        dir: "/".into(),
                    },
                }));
            }
            "rename" => {
                self.dialog = Some(Dialog::Input(InputDialog {
                    title: "重命名".into(),
                    label: "新名称".into(),
                    value: "示例影片-4K.mp4".into(),
                    on_ok: InputAction::Rename {
                        drive,
                        path: "/影视/示例影片-4K.mp4".into(),
                    },
                }));
            }
            "delete" => {
                self.dialog = Some(Dialog::Confirm(ConfirmDialog {
                    title: "删除确认".into(),
                    message: "确认删除选中的 2 项？\n/影视/示例影片-4K.mp4\n/文档/使用说明.txt\n\n（目录会连同其中内容一起删除）".into(),
                    danger: true,
                    action: ConfirmAction::Delete {
                        drive,
                        paths: vec![
                            "/影视/示例影片-4K.mp4".into(),
                            "/文档/使用说明.txt".into(),
                        ],
                    },
                }));
            }
            "move" => self.open_dir_picker(DirPickerAction::Move {
                paths: vec!["/影视/示例影片-4K.mp4".into()],
            }),
            "share" => {
                self.dialog = Some(Dialog::Share(ShareDialog {
                    drive,
                    paths: vec!["/影视/示例影片-4K.mp4".into()],
                    expire_days: 7,
                    password: String::new(),
                    busy: false,
                    result: None,
                    error: String::new(),
                }));
            }
            "crosscopy" => {
                let dst = self
                    .drives
                    .iter()
                    .find(|s| s.id() != self.active_drive_id() && s.drive.is_some())
                    .map(|s| s.id().to_string())
                    .unwrap_or_default();
                self.dialog = Some(Dialog::CrossCopy(CrossCopyDialog {
                    src_drive: drive,
                    paths: vec!["/影视/示例影片-4K.mp4".into()],
                    dst_drive: dst,
                    dst_dir: "/备份".into(),
                }));
            }
            "details" => {
                self.dialog = Some(Dialog::Details(DetailsDialog { entry: sample }));
            }
            "filepicker" => self.open_file_picker(false),
            _ => {}
        }
    }

    fn build_slot(paths: &Paths, dcfg: &DriveConfig) -> DriveSlot {
        let init = DriveInit {
            id: dcfg.id.clone(),
            name: dcfg.display_name().to_string(),
            data_dir: paths.account_path(&dcfg.id),
            threads: dcfg.threads,
            verbose: false,
        };
        let created: wp_core::Result<Arc<dyn Drive>> = match dcfg.kind {
            DriveKind::Baidu => wp_drive_baidu::create(init),
            DriveKind::Quark => wp_drive_quark::create(init),
            DriveKind::Guangya => wp_drive_guangya::create(init),
            DriveKind::Local => wp_drive::local::create(init),
            DriveKind::Fake => wp_drive::fake::create(init),
        };
        match created {
            Ok(drive) => {
                log_info!("网盘", "已加载实例 {}（{}）", dcfg.id, dcfg.display_name());
                DriveSlot {
                    cfg: dcfg.clone(),
                    drive: Some(drive),
                    account: None,
                    status: AccountStatus::Unknown,
                    init_error: None,
                }
            }
            Err(e) => {
                log_error!("网盘", "实例 {} 初始化失败：{e}", dcfg.id);
                DriveSlot {
                    cfg: dcfg.clone(),
                    drive: None,
                    account: None,
                    status: AccountStatus::Failed(e.to_string()),
                    init_error: Some(e.to_string()),
                }
            }
        }
    }

    // ───────────── 查询辅助 ─────────────

    pub fn active_slot(&self) -> Option<&DriveSlot> {
        self.drives.get(self.active)
    }

    pub fn active_drive(&self) -> Option<Arc<dyn Drive>> {
        self.drives.get(self.active).and_then(|s| s.drive.clone())
    }

    pub fn slot_by_id(&self, id: &str) -> Option<&DriveSlot> {
        self.drives.iter().find(|s| s.id() == id)
    }

    pub fn slot_idx(&self, id: &str) -> Option<usize> {
        self.drives.iter().position(|s| s.id() == id)
    }

    pub fn browse_mut(&mut self, drive: &str) -> &mut BrowseState {
        self.browse.entry(drive.to_string()).or_default()
    }

    pub fn browse_ref(&self, drive: &str) -> Option<&BrowseState> {
        self.browse.get(drive)
    }

    pub fn active_browse(&self) -> Option<&BrowseState> {
        self.drives
            .get(self.active)
            .and_then(|s| self.browse.get(s.id()))
    }

    pub fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        let text = text.into();
        // 同样内容 1 秒内不重复
        if self
            .toasts
            .iter()
            .any(|t| t.text == text && t.at.elapsed() < Duration::from_secs(1))
        {
            return;
        }
        match kind {
            ToastKind::Error => log_error!("界面", "{text}"),
            ToastKind::Warn => log_warn!("界面", "{text}"),
            _ => log_info!("界面", "{text}"),
        }
        self.toasts.push(Toast {
            kind,
            text,
            at: Instant::now(),
        });
        if self.toasts.len() > 6 {
            self.toasts.remove(0);
        }
    }

    /// 在 tokio 上跑一个异步事件回传任务。
    pub fn spawn<F>(&self, fut: F)
    where
        F: std::future::Future<Output = Event> + Send + 'static,
    {
        let Some(rt) = &self.rt else {
            log_warn!("应用", "tokio runtime 尚未就绪，操作被丢弃");
            return;
        };
        let tx = self.event_tx.clone();
        let ctx = self.ctx.clone();
        rt.spawn(async move {
            let ev = fut.await;
            let _ = tx.send(ev);
            if let Some(c) = ctx {
                c.request_repaint();
            }
        });
    }

    // ───────────── 网盘操作 ─────────────

    pub fn refresh_all_accounts(&mut self) {
        self.loading_accounts = true;
        let list: Vec<(String, Arc<dyn Drive>)> = self
            .drives
            .iter_mut()
            .filter_map(|s| {
                if let Some(d) = &s.drive {
                    s.status = AccountStatus::Checking;
                    Some((s.id().to_string(), d.clone()))
                } else {
                    None
                }
            })
            .collect();
        for (id, drive) in list {
            self.spawn(async move {
                Event::AccountChecked {
                    drive: id,
                    result: drive.account().await,
                }
            });
        }
        self.loading_accounts = false;
    }

    pub fn refresh_account(&mut self, drive_id: &str) {
        if let Some(slot) = self.drives.iter_mut().find(|s| s.id() == drive_id) {
            if let Some(d) = slot.drive.clone() {
                slot.status = AccountStatus::Checking;
                let id = drive_id.to_string();
                self.spawn(async move {
                    Event::AccountChecked {
                        drive: id,
                        result: d.account().await,
                    }
                });
            }
        }
    }

    pub fn load_dir_for(&mut self, drive_id: &str, path: &str) {
        let Some(slot) = self.drives.iter().find(|s| s.id() == drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            let st = self.browse_mut(drive_id);
            st.error = Some("该网盘未初始化成功，无法浏览".into());
            st.loading = false;
            return;
        };
        let path = norm_path(path);
        let token = {
            let st = self.browse_mut(drive_id);
            st.token += 1;
            st.loading = true;
            st.error = None;
            st.token
        };
        let id = drive_id.to_string();
        let p = path.clone();
        self.spawn(async move {
            let result = drive.list(&p).await;
            Event::Listed {
                drive: id,
                path: p,
                token,
                result,
            }
        });
    }

    pub fn navigate(&mut self, path: &str) {
        let Some(slot) = self.drives.get(self.active) else {
            return;
        };
        let id = slot.id().to_string();
        self.navigate_drive(&id, path, true);
    }

    pub fn navigate_drive(&mut self, drive_id: &str, path: &str, push_history: bool) {
        let path = norm_path(path);
        let current = self
            .browse
            .get(drive_id)
            .map(|s| s.path.clone())
            .unwrap_or_else(|| "/".into());
        {
            let st = self.browse_mut(drive_id);
            if push_history && st.path != path {
                st.back.push(st.path.clone());
                st.forward.clear();
                if st.back.len() > 100 {
                    st.back.remove(0);
                }
            }
            st.path = path.clone();
            st.selected.clear();
        }
        self.load_dir_for(drive_id, &path);
        let _ = current;
    }

    pub fn go_back(&mut self) {
        let Some(slot) = self.drives.get(self.active) else {
            return;
        };
        let id = slot.id().to_string();
        let target = {
            let st = self.browse_mut(&id);
            let Some(prev) = st.back.pop() else { return };
            st.forward.push(st.path.clone());
            prev
        };
        {
            let st = self.browse_mut(&id);
            st.path = target.clone();
            st.selected.clear();
        }
        self.load_dir_for(&id, &target);
    }

    pub fn go_forward(&mut self) {
        let Some(slot) = self.drives.get(self.active) else {
            return;
        };
        let id = slot.id().to_string();
        let target = {
            let st = self.browse_mut(&id);
            let Some(next) = st.forward.pop() else { return };
            st.back.push(st.path.clone());
            next
        };
        {
            let st = self.browse_mut(&id);
            st.path = target.clone();
            st.selected.clear();
        }
        self.load_dir_for(&id, &target);
    }

    pub fn open_entry(&mut self, entry: &RemoteEntry) {
        if entry.is_dir {
            let drive_id = self.active_slot().map(|s| s.id().to_string());
            if let Some(id) = drive_id {
                self.navigate_drive(&id, &entry.path, true);
            }
        } else {
            self.dialog = Some(Dialog::Details(DetailsDialog {
                entry: entry.clone(),
            }));
        }
    }

    /// 当前选中条目（无选中时返回 None）。
    pub fn selected_entries(&self) -> Vec<RemoteEntry> {
        let Some(slot) = self.active_slot() else {
            return Vec::new();
        };
        let Some(st) = self.browse.get(slot.id()) else {
            return Vec::new();
        };
        st.entries
            .iter()
            .filter(|e| st.selected.contains(&e.path))
            .cloned()
            .collect()
    }

    pub fn active_drive_id(&self) -> String {
        self.drives
            .get(self.active)
            .map(|s| s.id().to_string())
            .unwrap_or_default()
    }

    // ───────────── 传输 ─────────────

    pub fn enqueue_upload_files(&mut self, files: &[PathBuf]) {
        let Some(drive) = self.active_drive() else {
            self.toast(ToastKind::Error, "当前网盘不可用");
            return;
        };
        let dir = self
            .active_browse()
            .map(|s| s.path.clone())
            .unwrap_or_else(|| "/".into());
        if files.is_empty() {
            self.toast(ToastKind::Warn, "没有选择要上传的文件");
            return;
        }
        for f in files {
            self.transfer
                .enqueue_upload(drive.clone(), f.clone(), dir.clone());
        }
        self.toast(
            ToastKind::Success,
            format!("已加入上传队列：{} 个文件 → {}", files.len(), dir),
        );
        self.page = Page::Transfers;
    }

    pub fn enqueue_download_entries(&mut self, entries: &[RemoteEntry]) {
        let Some(drive) = self.active_drive() else {
            return;
        };
        let mut n = 0usize;
        let mut skipped = 0usize;
        for e in entries {
            if e.is_dir {
                skipped += 1;
                continue;
            }
            self.transfer
                .enqueue_download(drive.clone(), e.path.clone(), None);
            n += 1;
        }
        if n > 0 {
            self.toast(ToastKind::Success, format!("已加入下载队列：{n} 个文件"));
            self.page = Page::Transfers;
        }
        if skipped > 0 {
            self.toast(
                ToastKind::Warn,
                format!("{skipped} 个目录被跳过（暂不支持整目录下载）"),
            );
        }
    }

    pub fn enqueue_cross_copy(&mut self, src_paths: &[String], dst_drive_id: &str, dst_dir: &str) {
        let Some(src) = self.active_drive() else {
            return;
        };
        let Some(dst_slot) = self.slot_by_id(dst_drive_id) else {
            self.toast(ToastKind::Error, "目标网盘不存在");
            return;
        };
        let Some(dst) = dst_slot.drive.clone() else {
            self.toast(ToastKind::Error, "目标网盘不可用");
            return;
        };
        let mut n = 0;
        for p in src_paths {
            self.transfer.enqueue_cross_copy(
                src.clone(),
                p.clone(),
                dst.clone(),
                dst_dir.to_string(),
            );
            n += 1;
        }
        self.toast(
            ToastKind::Success,
            format!("已加入跨盘传输队列：{n} 项 → {dst_dir}"),
        );
        self.page = Page::Transfers;
    }

    pub fn tick_transfers(&mut self, force: bool) {
        if force || self.last_transfer_tick.elapsed() >= Duration::from_millis(300) {
            self.last_transfer_tick = Instant::now();
            self.transfer_snapshot = self.transfer.tasks();
            self.transfer_stats = self.transfer.stats();
        }
    }

    /// 配置里的下载目录。
    pub fn download_dir(&self) -> PathBuf {
        self.cfg.resolved_download_dir(&self.paths)
    }

    // ───────────── 配置/主题 ─────────────

    pub fn save_config(&mut self) {
        // 记住界面状态
        self.cfg.ui.theme = self.theme.name.to_string();
        if let Some(slot) = self.drives.get(self.active) {
            self.cfg.ui.last_drive = slot.id().to_string();
        }
        self.cfg.ui.window_size = [1280.0, 800.0];
        match self.cfg.save(&self.cfg_path) {
            Ok(()) => {}
            Err(e) => self.toast(ToastKind::Error, format!("保存配置失败：{e}")),
        }
    }

    pub fn apply_settings(&mut self) {
        let d = self.settings_draft.clone();
        self.cfg.transfer.small_file_threshold = d.small_threshold_mb.max(1) * 1024 * 1024;
        self.cfg.transfer.small_file_concurrency = d.small_concurrency.clamp(1, 64);
        self.cfg.transfer.large_file_concurrency = d.large_concurrency.clamp(1, 32);
        self.cfg.transfer.retry = d.retry.min(10);
        self.cfg.transfer.adaptive = d.adaptive;
        self.cfg.transfer.max_speed_bps = d.speed_limit_mbps * 1024 * 1024;
        self.cfg.download.segments = d.segments.clamp(1, 16);
        self.cfg.download.segment_threshold = d.segment_threshold_mb.max(1) * 1024 * 1024;
        self.cfg.download.resume = d.resume;
        self.cfg.ui.theme = d.theme.clone();
        self.cfg.ui.font_path = d.font_path.clone();
        self.cfg.download_dir = d.download_dir.clone();
        self.cfg.guard.enabled = d.guard_enabled;
        self.cfg.guard.action = d.guard_action;
        self.cfg.keep_failed_temp = d.keep_failed_temp;
        self.theme = theme::by_name(&d.theme);
        self.transfer
            .update_config(self.cfg.transfer.clone(), self.cfg.download.clone());
        self.transfer
            .set_download_dir(self.cfg.resolved_download_dir(&self.paths));
        let guard_save_error = {
            let mut g = match self.guard.lock() {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
            g.action = d.guard_action;
            g.enabled = d.guard_enabled;
            g.save().err()
        };
        if let Some(e) = guard_save_error {
            self.toast(ToastKind::Error, format!("保存敏感词库失败：{e}"));
        }
        self.save_config();
        self.toast(ToastKind::Success, "设置已保存");
    }

    // ───────────── 事件处理 ─────────────

    pub fn poll_events(&mut self) {
        for _ in 0..200 {
            let ev = match self.event_rx.try_recv() {
                Ok(ev) => ev,
                Err(_) => break,
            };
            self.handle_event(ev);
        }
    }

    fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Listed {
                drive,
                path,
                token,
                result,
            } => {
                let Some(st) = self.browse.get_mut(&drive) else {
                    return;
                };
                if st.token != token {
                    return; // 过期请求
                }
                st.loading = false;
                match result {
                    Ok(mut entries) => {
                        sort_entries(&mut entries, SortBy::Name, false);
                        st.entries = std::sync::Arc::new(entries);
                        st.error = None;
                        st.path = path;
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if e.is_auth() {
                            if let Some(slot) = self.drives.iter_mut().find(|s| s.id() == drive) {
                                slot.status = AccountStatus::LoggedOut;
                            }
                        }
                        st.error = Some(msg.clone());
                        log_error!("网盘", "列目录失败 {path}：{msg}");
                    }
                }
            }
            Event::AccountChecked { drive, result } => {
                if let Some(slot) = self.drives.iter_mut().find(|s| s.id() == drive) {
                    match result {
                        Ok(info) => {
                            slot.status = AccountStatus::LoggedIn;
                            slot.account = Some(info);
                        }
                        Err(e) => {
                            slot.account = None;
                            if e.is_auth() {
                                slot.status = AccountStatus::LoggedOut;
                            } else {
                                slot.status = AccountStatus::Failed(e.to_string());
                            }
                        }
                    }
                }
            }
            Event::QrStarted { drive, result } => {
                if let Some(Dialog::Login(login)) = self.dialog.as_mut() {
                    if login.drive != drive {
                        return;
                    }
                    login.busy = false;
                    match result {
                        Ok(ch) => {
                            if let Some(png) = &ch.image_png {
                                if let Some(ctx) = &self.ctx {
                                    login.qr_texture = fonts::png_texture(ctx, "qr", png);
                                }
                            }
                            login.qr_message.clear();
                            login.qr = Some(ch);
                            login.qr_state = QrState::Waiting;
                            login.qr_timer = Instant::now();
                        }
                        Err(e) => login.error = e.to_string(),
                    }
                }
            }
            Event::QrPolled { drive, result } => {
                if let Some(Dialog::Login(login)) = self.dialog.as_mut() {
                    if login.drive != drive {
                        return;
                    }
                    login.poll_inflight = false;
                    match result {
                        Ok(poll) => {
                            login.qr_state = poll.state;
                            login.qr_message = poll.message.clone();
                            if poll.state == QrState::Success {
                                login.busy = true;
                                self.refresh_account(&drive);
                                self.finalize_login_async(&drive);
                            } else {
                                login.busy = false;
                            }
                        }
                        Err(e) => login.error = e.to_string(),
                    }
                }
            }
            Event::LoggedIn { drive, result } => match result {
                Ok(info) => {
                    self.toast(
                        ToastKind::Success,
                        format!("登录成功：{}", info.display_name),
                    );
                    if let Some(slot) = self.drives.iter_mut().find(|s| s.id() == drive) {
                        slot.status = AccountStatus::LoggedIn;
                        slot.account = Some(info);
                    }
                    self.dialog = None;
                    let path = self
                        .browse
                        .get(&drive)
                        .map(|s| s.path.clone())
                        .unwrap_or_else(|| "/".into());
                    self.load_dir_for(&drive, &path);
                }
                Err(e) => {
                    if let Some(Dialog::Login(login)) = self.dialog.as_mut() {
                        login.busy = false;
                        login.error = e.to_string();
                    }
                }
            },
            Event::LoggedOut { drive, result } => match result {
                Ok(()) => {
                    self.toast(ToastKind::Info, "已退出登录（仅清除本地凭证）");
                    if let Some(slot) = self.drives.iter_mut().find(|s| s.id() == drive) {
                        slot.status = AccountStatus::LoggedOut;
                        slot.account = None;
                    }
                    if let Some(st) = self.browse.get_mut(&drive) {
                        st.entries = std::sync::Arc::new(Vec::new());
                        st.selected.clear();
                    }
                }
                Err(e) => self.toast(ToastKind::Error, format!("退出登录失败：{e}")),
            },
            Event::OpDone {
                message,
                refresh,
                result,
            } => match result {
                Ok(msg) => {
                    if message.contains("验证码") {
                        if let Some(Dialog::Login(login)) = self.dialog.as_mut() {
                            login.busy = false;
                            login.sms_hint = msg;
                        }
                        return;
                    }
                    let text = if msg.is_empty() {
                        message
                    } else {
                        format!("{message}：{msg}")
                    };
                    self.toast(ToastKind::Success, text);
                    if refresh {
                        let id = self.active_drive_id();
                        let path = self
                            .browse
                            .get(&id)
                            .map(|s| s.path.clone())
                            .unwrap_or_else(|| "/".into());
                        self.load_dir_for(&id, &path);
                    }
                }
                Err(e) => self.toast(ToastKind::Error, format!("{message}失败：{e}")),
            },
            Event::ShareCreated(result) => {
                if let Some(Dialog::Share(d)) = self.dialog.as_mut() {
                    d.busy = false;
                    match result {
                        Ok(info) => {
                            d.result = Some(info);
                            d.error.clear();
                        }
                        Err(e) => d.error = e.to_string(),
                    }
                }
            }
            Event::DirectLink { path, result } => match result {
                Ok(link) => {
                    let text = if link.headers.is_empty() {
                        link.url.clone()
                    } else {
                        format!(
                            "{}\n（需要请求头：{}）",
                            link.url,
                            link.headers
                                .iter()
                                .map(|(k, v)| format!("{k}: {v}"))
                                .collect::<Vec<_>>()
                                .join("; ")
                        )
                    };
                    if let Some(ctx) = &self.ctx {
                        ctx.copy_text(text.clone());
                    }
                    self.toast(
                        ToastKind::Success,
                        format!(
                            "已复制直链（有效期至 {}）",
                            link.expire_at
                                .map(wp_core::util::format_time)
                                .unwrap_or_else(|| "未知".into())
                        ),
                    );
                    log_info!("网盘", "直链 {path}：{}", link.url);
                }
                Err(e) => self.toast(ToastKind::Error, format!("获取直链失败：{e}")),
            },
            Event::RecycleListed { drive, result } => {
                if drive != self.active_drive_id() && !self.tools.recycle.loading {
                    return;
                }
                self.tools.recycle.loading = false;
                match result {
                    Ok(list) => {
                        self.tools.recycle.data = list;
                        self.tools.recycle.error.clear();
                        self.tools.recycle_selected.clear();
                    }
                    Err(e) => self.tools.recycle.error = e.to_string(),
                }
            }
            Event::RecycleOp { message, result } => {
                self.tools.recycle.loading = false;
                match result {
                    Ok(msg) => {
                        self.toast(
                            ToastKind::Success,
                            if msg.is_empty() {
                                message
                            } else {
                                format!("{message}：{msg}")
                            },
                        );
                        let id = self.active_drive_id();
                        self.recycle_load(&id);
                    }
                    Err(e) => self.toast(ToastKind::Error, format!("{message}失败：{e}")),
                }
            }
            Event::CloudTasksListed { drive, result } => {
                if drive != self.active_drive_id() && !self.tools.cloud_tasks.loading {
                    return;
                }
                self.tools.cloud_tasks.loading = false;
                match result {
                    Ok(list) => {
                        self.tools.cloud_tasks.data = list;
                        self.tools.cloud_tasks.error.clear();
                    }
                    Err(e) => self.tools.cloud_tasks.error = e.to_string(),
                }
            }
            Event::CloudAdded { result } => {
                self.tools.cloud_busy = false;
                match result {
                    Ok(task) => {
                        self.toast(ToastKind::Success, format!("云任务已创建：{task}"));
                        let id = self.active_drive_id();
                        self.cloud_tasks_load(&id);
                    }
                    Err(e) => self.toast(ToastKind::Error, format!("云添加失败：{e}")),
                }
            }
            Event::RecordsListed { drive, result } => {
                if drive != self.active_drive_id() && !self.tools.records.loading {
                    return;
                }
                self.tools.records.loading = false;
                match result {
                    Ok(list) => {
                        self.tools.records.data = list;
                        self.tools.records.error.clear();
                    }
                    Err(e) => self.tools.records.error = e.to_string(),
                }
            }
            Event::SharesListed { drive, result } => {
                if drive != self.active_drive_id() && !self.tools.shares.loading {
                    return;
                }
                self.tools.shares.loading = false;
                match result {
                    Ok(list) => {
                        self.tools.shares.data = list;
                        self.tools.shares.error.clear();
                    }
                    Err(e) => self.tools.shares.error = e.to_string(),
                }
            }
            Event::ShareInfoGot { result } => {
                self.tools.share_busy = false;
                match result {
                    Ok(detail) => {
                        self.toast(
                            ToastKind::Success,
                            format!("分享校验通过：{}", detail.title),
                        );
                        self.tools.share_detail = Some(detail);
                    }
                    Err(e) => self.toast(ToastKind::Error, format!("分享校验失败：{e}")),
                }
            }
            Event::ShareSaved { result } => {
                self.tools.share_busy = false;
                match result {
                    Ok(msg) => self.toast(ToastKind::Success, msg),
                    Err(e) => self.toast(ToastKind::Error, format!("转存失败：{e}")),
                }
            }
            Event::CategoriesListed { drive, result } => {
                if drive != self.active_drive_id() && !self.tools.categories.loading {
                    return;
                }
                self.tools.categories.loading = false;
                match result {
                    Ok(list) => {
                        self.tools.categories.data = list;
                        self.tools.categories.error.clear();
                    }
                    Err(e) => self.tools.categories.error = e.to_string(),
                }
            }
            Event::ThumbLoaded { key, result } => match result {
                Ok(bytes) => self.thumbs.put_raw(&key, bytes),
                Err(_) => self.thumbs.put_failed(&key),
            },
            Event::PickerListed { path, result } => {
                if let Some(Dialog::DirPicker(d)) = self.dialog.as_mut() {
                    d.loading = false;
                    match result {
                        Ok(mut entries) => {
                            entries.retain(|e| e.is_dir);
                            d.entries = entries;
                            d.path = path;
                            d.error.clear();
                        }
                        Err(e) => d.error = e.to_string(),
                    }
                }
            }
        }
    }
}

/// 从配置生成设置页草稿。
pub fn settings_from_config(cfg: &AppConfig) -> SettingsDraft {
    SettingsDraft {
        small_threshold_mb: cfg.transfer.small_file_threshold / (1024 * 1024),
        small_concurrency: cfg.transfer.small_file_concurrency,
        large_concurrency: cfg.transfer.large_file_concurrency,
        retry: cfg.transfer.retry,
        adaptive: cfg.transfer.adaptive,
        speed_limit_mbps: cfg.transfer.max_speed_bps / (1024 * 1024),
        segments: cfg.download.segments,
        segment_threshold_mb: cfg.download.segment_threshold / (1024 * 1024),
        resume: cfg.download.resume,
        theme: cfg.ui.theme.clone(),
        font_path: cfg.ui.font_path.clone(),
        download_dir: cfg.download_dir.clone(),
        guard_enabled: cfg.guard.enabled,
        guard_action: cfg.guard.action,
        keep_failed_temp: cfg.keep_failed_temp,
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.ctx.is_none() {
            self.ctx = Some(ctx.clone());
        }
        self.frames += 1;
        self.poll_events();
        self.tick_transfers(false);
        self.poll_logs();
        self.thumbs.pump(&ctx);
        // 验收模式：第 4 帧阻塞等待网络数据（真实账号截图时让列表/缩略图先加载完）
        if self.frames == 4 {
            if let Some(ms) = self.dev.wait_ms {
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
        }

        // 主题切换
        if self.applied_theme != self.theme.name {
            theme::apply(&ctx, self.theme);
            self.applied_theme = self.theme.name.to_string();
        }

        self.handle_shortcuts(&ctx);

        // ── 顶栏：网盘导航 + 账号区
        egui::Panel::top("topbar")
            .exact_size(theme::size::TOPBAR_H)
            .frame(
                egui::Frame::NONE
                    .fill(self.theme.nav_bg)
                    .inner_margin(egui::Margin::symmetric(10, 6)),
            )
            .show(ui, |ui| self.topbar(ui));

        // ── 底栏：状态
        egui::Panel::bottom("statusbar")
            .exact_size(theme::size::STATUSBAR_H)
            .frame(
                egui::Frame::NONE
                    .fill(self.theme.card)
                    .inner_margin(egui::Margin::symmetric(10, 2)),
            )
            .show(ui, |ui| self.statusbar(ui));

        // ── 左栏：功能导航
        egui::Panel::left("sidebar")
            .exact_size(theme::size::SIDEBAR_W)
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(self.theme.card)
                    .inner_margin(egui::Margin::symmetric(8, 10)),
            )
            .show(ui, |ui| self.sidebar(ui));

        // ── 中央内容
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(self.theme.bg)
                    .inner_margin(egui::Margin::same(10)),
            )
            .show(ui, |ui| match self.page {
                Page::Browser => pages::browser::show(self, ui),
                Page::Transfers => pages::transfers::show(self, ui),
                Page::Tools => pages::tools::show(self, ui),
                Page::Guard => pages::guard_page::show(self, ui),
                Page::Logs => pages::logs::show(self, ui),
                Page::Settings => pages::settings::show(self, ui),
                Page::About => pages::about::show(self, ui),
            });

        // ── 对话框
        dialogs::show(self, &ctx);
        self.show_file_picker(&ctx);
        self.show_toasts(&ctx);

        // 开发/验收截图模式
        self.maybe_screenshot(&ctx);

        // 有任务在跑 / 有加载中 → 保持刷新
        let busy = self
            .transfer_stats
            .active
            .saturating_add(self.transfer_stats.queued)
            > 0
            || self.browse.values().any(|b| b.loading)
            || self.dialog.is_some()
            || self.file_picker.is_some();
        if busy {
            ctx.request_repaint_after(Duration::from_millis(120));
        } else {
            ctx.request_repaint_after(Duration::from_millis(600));
        }
    }
}

impl App {
    fn poll_logs(&mut self) {
        let new_lines = wp_core::logging::recent_since(self.log_last_seq);
        if let Some(last) = new_lines.last() {
            self.log_last_seq = last.seq;
        }
        if !new_lines.is_empty() {
            self.log_lines.extend(new_lines);
            if self.log_lines.len() > 5000 {
                let keep = self.log_lines.len() - 5000;
                self.log_lines.drain(0..keep);
            }
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if self.dialog.is_some() || self.file_picker.is_some() {
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.dialog = None;
                self.file_picker = None;
            }
            return;
        }
        let (f5, del, enter, backspace, ctrl_f, ctrl_u, ctrl_1_6) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::F5),
                i.key_pressed(egui::Key::Delete),
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Backspace),
                i.modifiers.ctrl && i.key_pressed(egui::Key::F),
                i.modifiers.ctrl && i.key_pressed(egui::Key::U),
                (
                    i.modifiers.ctrl && i.key_pressed(egui::Key::Num1),
                    i.modifiers.ctrl && i.key_pressed(egui::Key::Num2),
                    i.modifiers.ctrl && i.key_pressed(egui::Key::Num3),
                    i.modifiers.ctrl && i.key_pressed(egui::Key::Num4),
                    i.modifiers.ctrl && i.key_pressed(egui::Key::Num5),
                    i.modifiers.ctrl && i.key_pressed(egui::Key::Num6),
                ),
            )
        });
        if ctrl_1_6.0 {
            self.page = Page::Browser;
        }
        if ctrl_1_6.1 {
            self.page = Page::Transfers;
        }
        if ctrl_1_6.2 {
            self.page = Page::Guard;
        }
        if ctrl_1_6.3 {
            self.page = Page::Logs;
        }
        if ctrl_1_6.4 {
            self.page = Page::Settings;
        }
        if ctrl_1_6.5 {
            self.page = Page::About;
        }
        if ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Num7)) {
            self.page = Page::Tools;
        }
        let _ = ctrl_f;
        if f5 {
            let id = self.active_drive_id();
            let path = self
                .browse
                .get(&id)
                .map(|s| s.path.clone())
                .unwrap_or_default();
            if !path.is_empty() {
                self.load_dir_for(&id, &path);
            }
        }
        if backspace {
            self.go_back();
        }
        if self.page == Page::Browser {
            if del {
                if let Some(a) = self.pending_delete_action() {
                    self.dialog = Some(Dialog::Confirm(a));
                }
            }
            if enter {
                let entries = self.selected_entries();
                if entries.len() == 1 {
                    self.open_entry(&entries[0]);
                }
            }
            let _ = ctrl_u;
        }
    }

    pub fn pending_delete_action(&mut self) -> Option<ConfirmDialog> {
        let entries = self.selected_entries();
        if entries.is_empty() {
            return None;
        }
        let id = self.active_drive_id();
        let paths: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
        Some(ConfirmDialog {
            title: "删除确认".into(),
            message: format!(
                "确认删除选中的 {} 项？\n{}",
                paths.len(),
                paths.iter().take(6).cloned().collect::<Vec<_>>().join("\n")
            ),
            danger: true,
            action: ConfirmAction::Delete { drive: id, paths },
        })
    }

    fn topbar(&mut self, ui: &mut egui::Ui) {
        let t = self.theme;
        ui.horizontal_centered(|ui| {
            ui.label(
                egui::RichText::new("☁ 网盘管理")
                    .size(17.0)
                    .strong()
                    .color(t.accent),
            );
            ui.label(
                egui::RichText::new(format!("v{}", wp_core::APP_VERSION))
                    .size(11.0)
                    .color(t.text_weak),
            );
            ui.add_space(10.0);
            ui.separator();
            ui.add_space(6.0);

            let drives: Vec<(usize, String, String, DriveKind, bool, AccountStatus)> = self
                .drives
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    (
                        i,
                        s.id().to_string(),
                        s.display().to_string(),
                        s.kind(),
                        s.drive.is_some(),
                        s.status.clone(),
                    )
                })
                .collect();
            let mut switch_to: Option<usize> = None;
            for (i, _id, name, kind, available, status) in drives {
                let selected = i == self.active;
                let dot_color = match &status {
                    AccountStatus::LoggedIn => t.success,
                    AccountStatus::LoggedOut => t.warning,
                    AccountStatus::Failed(_) => t.error,
                    _ => t.text_weak,
                };
                let brand = egui::Color32::from_rgb(
                    kind.brand_color()[0],
                    kind.brand_color()[1],
                    kind.brand_color()[2],
                );
                let text_color = if selected {
                    if t.dark {
                        egui::Color32::from_rgb(18, 18, 22)
                    } else {
                        egui::Color32::WHITE
                    }
                } else {
                    t.text
                };
                let fill = if selected { brand } else { t.card };
                let mut btn = egui::Button::new(
                    egui::RichText::new(format!("{} {}", kind.icon(), name))
                        .size(13.0)
                        .color(text_color),
                )
                .fill(fill)
                .corner_radius(theme::radius(14));
                if !selected {
                    btn = btn.stroke(egui::Stroke::new(1.0, t.border));
                }
                let resp = ui.add(btn);
                // 状态点画在按钮左侧
                let dot_center = resp.rect.left_top() + egui::vec2(9.0, resp.rect.height() / 2.0);
                ui.painter().circle_filled(dot_center, 3.0, dot_color);
                if resp.clicked() && available {
                    switch_to = Some(i);
                }
                if !available {
                    resp.on_hover_text("该网盘未初始化成功");
                }
            }
            if let Some(i) = switch_to {
                self.active = i;
                let id = self.drives[i].id().to_string();
                self.page = Page::Browser;
                if !self.browse.contains_key(&id) {
                    self.load_dir_for(&id, "/");
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // 主题快速切换
                let mut theme_name = self.theme.name.to_string();
                egui::ComboBox::from_id_salt("theme_quick")
                    .selected_text(egui::RichText::new("🎨").size(14.0))
                    .width(60.0)
                    .show_ui(ui, |ui| {
                        for th in theme::THEMES.iter() {
                            if ui
                                .selectable_label(theme_name == th.name, th.display)
                                .clicked()
                            {
                                theme_name = th.name.to_string();
                            }
                        }
                    });
                if theme_name != self.theme.name {
                    self.theme = theme::by_name(&theme_name);
                    self.cfg.ui.theme = theme_name.clone();
                    self.settings_draft.theme = theme_name;
                    self.save_config();
                }
                // 账号区
                if let Some(slot) = self.drives.get(self.active) {
                    let name = slot.display().to_string();
                    let status = slot.status.clone();
                    let quota = slot.account.as_ref().and_then(|a| a.quota);
                    if let Some(q) = quota {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} / {}",
                                wp_core::util::format_size(q.used),
                                wp_core::util::format_size(q.total)
                            ))
                            .size(11.0)
                            .color(t.text_weak),
                        );
                        ui.add(
                            egui::ProgressBar::new(q.used_ratio())
                                .desired_width(90.0)
                                .fill(t.accent)
                                .corner_radius(theme::radius(3)),
                        );
                    }
                    let (status_text, status_color) = match &status {
                        AccountStatus::LoggedIn => (name, t.success),
                        AccountStatus::Checking => ("检查中…".to_string(), t.warning),
                        AccountStatus::LoggedOut => ("未登录".to_string(), t.warning),
                        AccountStatus::Failed(_) => ("状态异常".to_string(), t.error),
                        AccountStatus::Unknown => ("未知".to_string(), t.text_weak),
                    };
                    let mut do_login = false;
                    let mut do_account = false;
                    ui.menu_button(
                        egui::RichText::new(format!("👤 {status_text}")).color(status_color),
                        |ui| {
                            if ui.button("🔐 登录 / 管理").clicked() {
                                do_login = true;
                                ui.close();
                            }
                            if ui.button("🔄 刷新状态").clicked() {
                                do_account = true;
                                ui.close();
                            }
                            if ui.button("🚪 退出登录").clicked() {
                                do_account = false;
                                do_login = false;
                                if let Some(slot) = self.drives.get(self.active) {
                                    let id = slot.id().to_string();
                                    if let Some(drive) = slot.drive.clone() {
                                        self.spawn(async move {
                                            Event::LoggedOut {
                                                drive: id,
                                                result: drive.logout().await,
                                            }
                                        });
                                    }
                                }
                                ui.close();
                            }
                        },
                    );
                    if do_login {
                        self.open_login_dialog();
                    }
                    if do_account {
                        let id = self.active_drive_id();
                        self.refresh_account(&id);
                    }
                }
            });
        });
    }

    pub fn open_login_dialog(&mut self) {
        let id = self.active_drive_id();
        if let Some(slot) = self.slot_by_id(&id) {
            let caps = slot.caps().map(|c| c.auth).unwrap_or_default();
            let mut dlg = LoginDialog::new(id.clone());
            dlg.caps = caps;
            dlg.tab = if caps.qr {
                LoginTab::Qr
            } else if caps.cookie {
                LoginTab::Cookie
            } else if caps.sms {
                LoginTab::Sms
            } else {
                LoginTab::Token
            };
            self.dialog = Some(Dialog::Login(dlg));
            if caps.qr {
                self.start_qr_login(&id);
            }
        }
    }

    pub fn start_qr_login(&mut self, drive_id: &str) {
        if let Some(slot) = self.slot_by_id(drive_id) {
            if let Some(drive) = slot.drive.clone() {
                let id = drive_id.to_string();
                self.spawn(async move {
                    Event::QrStarted {
                        drive: id,
                        result: drive.begin_qr().await,
                    }
                });
            }
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let t = self.theme;
        ui.add_space(2.0);
        for page in [
            Page::Browser,
            Page::Transfers,
            Page::Tools,
            Page::Guard,
            Page::Logs,
            Page::Settings,
            Page::About,
        ] {
            let selected = self.page == page;
            let badge_text = if page == Page::Transfers {
                let n = self.transfer_stats.active + self.transfer_stats.queued;
                if n > 0 {
                    format!(" {n}")
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            let text = format!("{}  {}{}", page.icon(), page.label(), badge_text);
            let color = if selected { t.accent } else { t.text };
            let fill = if selected {
                t.hover
            } else {
                egui::Color32::TRANSPARENT
            };
            let resp = ui.add_sized(
                [theme::size::SIDEBAR_W - 20.0, 34.0],
                egui::Button::new(egui::RichText::new(text).size(14.0).color(color))
                    .fill(fill)
                    .corner_radius(theme::radius(6)),
            );
            if resp.clicked() {
                self.page = page;
            }
            ui.add_space(2.0);
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
            if ui
                .button(
                    egui::RichText::new("📂 下载目录")
                        .size(12.0)
                        .color(t.text_weak),
                )
                .clicked()
            {
                let dir = self.download_dir();
                if let Err(e) = wp_core::paths::open_path(&dir) {
                    self.toast(ToastKind::Error, format!("打开目录失败：{e}"));
                }
            }
            if ui
                .button(
                    egui::RichText::new("🗂 数据目录")
                        .size(12.0)
                        .color(t.text_weak),
                )
                .clicked()
            {
                let dir = self.paths.root.clone();
                if let Err(e) = wp_core::paths::open_path(&dir) {
                    self.toast(ToastKind::Error, format!("打开目录失败：{e}"));
                }
            }
        });
    }

    fn statusbar(&mut self, ui: &mut egui::Ui) {
        let t = self.theme;
        ui.horizontal_centered(|ui| {
            let active = self
                .active_slot()
                .map(|s| s.display().to_string())
                .unwrap_or_default();
            ui.label(
                egui::RichText::new(format!("当前：{active}"))
                    .size(11.0)
                    .color(t.text_weak),
            );
            ui.separator();
            let n_active = self.transfer_stats.active;
            let n_queued = self.transfer_stats.queued;
            if n_active + n_queued > 0 {
                ui.label(
                    egui::RichText::new(format!(
                        "⇅ {} 进行中 / {} 排队 · {}",
                        n_active,
                        n_queued,
                        wp_core::util::format_speed(self.transfer_stats.speed)
                    ))
                    .size(11.0)
                    .color(t.accent),
                );
            } else {
                ui.label(egui::RichText::new("⇅ 空闲").size(11.0).color(t.text_weak));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(if self.paths.portable {
                        "绿色版"
                    } else {
                        "用户数据目录"
                    })
                    .size(11.0)
                    .color(t.text_weak),
                );
                ui.separator();
                ui.label(
                    egui::RichText::new(format!("数据：{}", self.paths.root.display()))
                        .size(11.0)
                        .color(t.text_weak),
                );
            });
        });
    }

    fn show_toasts(&mut self, ctx: &egui::Context) {
        self.toasts
            .retain(|t| t.at.elapsed() < Duration::from_secs(6));
        if self.toasts.is_empty() {
            return;
        }
        let t = self.theme;
        let mut area = egui::Area::new(modal_id("toasts"))
            .anchor(
                egui::Align2::RIGHT_TOP,
                [-16.0, theme::size::TOPBAR_H + 12.0],
            )
            .order(egui::Order::Foreground);
        area = area.interactable(false);
        area.show(ctx, |ui| {
            for toast in &self.toasts {
                let (fg, icon) = match toast.kind {
                    ToastKind::Info => (t.accent, "ℹ"),
                    ToastKind::Success => (t.success, "✓"),
                    ToastKind::Warn => (t.warning, "⚠"),
                    ToastKind::Error => (t.error, "✕"),
                };
                egui::Frame::NONE
                    .fill(t.card)
                    .stroke(egui::Stroke::new(1.0, fg))
                    .corner_radius(theme::radius(8))
                    .inner_margin(egui::Margin::symmetric(14, 8))
                    .shadow(egui::epaint::Shadow {
                        offset: [0, 3],
                        blur: 12,
                        spread: 0,
                        color: egui::Color32::from_black_alpha(90),
                    })
                    .show(ui, |ui| {
                        ui.set_max_width(420.0);
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(icon).color(fg).size(14.0));
                            ui.label(egui::RichText::new(&toast.text).size(13.0).color(t.text));
                        });
                    });
                ui.add_space(4.0);
            }
        });
    }

    fn maybe_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.dev.screenshot.clone() else {
            return;
        };
        if self.frames == 8 {
            let p = path.clone();
            ctx.request_screenshot(move |img| {
                let (w, h) = (img.size[0], img.size[1]);
                let mut buf = Vec::with_capacity(w * h * 4);
                for px in &img.pixels {
                    buf.extend_from_slice(&[px.r(), px.g(), px.b(), px.a()]);
                }
                if let Some(im) = image::RgbaImage::from_raw(w as u32, h as u32, buf) {
                    match im.save(&p) {
                        Ok(()) => println!("SCREENSHOT_SAVED {} {}x{}", p.display(), w, h),
                        Err(e) => println!("SCREENSHOT_SAVE_FAILED {e}"),
                    }
                }
            });
        }
        let exit_at = self.dev.exit_after_frames.unwrap_or(20);
        if self.frames >= exit_at {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint();
    }

    pub fn show_file_picker(&mut self, ctx: &egui::Context) {
        let Some(mut picker) = self.file_picker.take() else {
            return;
        };
        let mut action: Option<FilePickerAction> = None;
        let t = self.theme;
        egui::Modal::new(modal_id("file_picker")).show(ctx, |ui| {
            ui.set_width(720.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(if picker.dir_mode {
                        "选择目录"
                    } else {
                        "选择上传文件"
                    })
                    .size(16.0)
                    .strong()
                    .color(t.text),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("✕ 关闭").clicked() {
                        action = Some(FilePickerAction::Close);
                    }
                });
            });
            ui.separator();
            // 路径栏
            ui.horizontal(|ui| {
                if ui.button("⬆ 上级").clicked() {
                    if let Some(parent) = picker.path.parent() {
                        action = Some(FilePickerAction::Navigate(parent.to_path_buf()));
                    }
                }
                if ui.button("🏠 主目录").clicked() {
                    if let Some(home) = dirs::home_dir() {
                        action = Some(FilePickerAction::Navigate(home));
                    }
                }
                let mut manual = picker.manual.clone();
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut manual)
                        .desired_width(380.0)
                        .hint_text("可直接输入路径后回车"),
                );
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    action = Some(FilePickerAction::Navigate(PathBuf::from(manual.trim())));
                }
                picker.manual = manual;
                if ui.button("跳转").clicked() {
                    action = Some(FilePickerAction::Navigate(PathBuf::from(
                        picker.manual.trim(),
                    )));
                }
            });
            ui.label(
                egui::RichText::new(picker.path.display().to_string())
                    .size(12.0)
                    .color(t.text_weak),
            );
            if !picker.error.is_empty() {
                ui.label(egui::RichText::new(&picker.error).size(12.0).color(t.error));
            }
            ui.separator();
            let mut navigate: Option<PathBuf> = None;
            egui::ScrollArea::vertical()
                .max_height(340.0)
                .show(ui, |ui| {
                    for e in &picker.entries {
                        ui.horizontal(|ui| {
                            if e.is_dir {
                                if ui
                                    .add(
                                        egui::Button::new(
                                            egui::RichText::new(format!("📁 {}", e.name))
                                                .color(t.text),
                                        )
                                        .fill(egui::Color32::TRANSPARENT),
                                    )
                                    .clicked()
                                {
                                    navigate = Some(e.path.clone());
                                }
                            } else if picker.dir_mode {
                                ui.label(
                                    egui::RichText::new(format!("📄 {}", e.name))
                                        .size(13.0)
                                        .color(t.text_weak),
                                );
                            } else {
                                let checked = picker.selected.contains(&e.path);
                                let mut c = checked;
                                if ui.checkbox(&mut c, "").changed() {
                                    if c {
                                        picker.selected.insert(e.path.clone());
                                    } else {
                                        picker.selected.remove(&e.path);
                                    }
                                }
                                let icon = crate::widgets::file_icon(false, &e.name);
                                ui.label(
                                    egui::RichText::new(format!("{icon} {}", e.name))
                                        .size(13.0)
                                        .color(t.text),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.label(
                                            egui::RichText::new(wp_core::util::format_size(e.size))
                                                .size(11.0)
                                                .color(t.text_weak),
                                        );
                                    },
                                );
                            }
                        });
                    }
                    if picker.entries.is_empty() {
                        ui.label(
                            egui::RichText::new("（空目录）")
                                .size(12.0)
                                .color(t.text_weak),
                        );
                    }
                });
            if let Some(p) = navigate {
                action = Some(FilePickerAction::Navigate(p));
            }
            ui.separator();
            ui.horizontal(|ui| {
                if picker.dir_mode {
                    if ui.button("✓ 选择此目录").clicked() {
                        action = Some(FilePickerAction::PickDir(picker.path.clone()));
                    }
                } else {
                    if ui
                        .button("✓ 上传所选（0）".replace("0", &picker.selected.len().to_string()))
                        .clicked()
                    {
                        action = Some(FilePickerAction::PickFiles(
                            picker.selected.iter().cloned().collect(),
                        ));
                    }
                }
                ui.label(
                    egui::RichText::new(format!("已选 {} 项", picker.selected.len()))
                        .size(12.0)
                        .color(t.text_weak),
                );
            });
        });

        match action {
            Some(FilePickerAction::Close) => {}
            Some(FilePickerAction::Navigate(p)) => {
                load_local_dir(&mut picker, &p);
                self.file_picker = Some(picker);
            }
            Some(FilePickerAction::PickDir(p)) => {
                self.pick_dir_result(p);
            }
            Some(FilePickerAction::PickFiles(files)) => {
                self.file_picker = None;
                self.enqueue_upload_files(&files);
            }
            None => self.file_picker = Some(picker),
        }
    }

    fn pick_dir_result(&mut self, path: PathBuf) {
        // 目录选择器的用途由打开时决定：目前仅用于「修改下载目录」
        self.settings_draft.download_dir = path.to_string_lossy().to_string();
        self.toast(
            ToastKind::Info,
            format!("已选择下载目录：{}", path.display()),
        );
    }

    pub fn open_file_picker(&mut self, dir_mode: bool) {
        let start = if dir_mode {
            self.download_dir()
        } else {
            dirs::home_dir().unwrap_or_else(|| self.paths.root.clone())
        };
        let mut picker = FilePicker {
            path: start.clone(),
            entries: Vec::new(),
            selected: BTreeSet::new(),
            manual: start.to_string_lossy().to_string(),
            error: String::new(),
            dir_mode,
        };
        load_local_dir(&mut picker, &start);
        self.file_picker = Some(picker);
    }

    // ───────────── 工具页异步封装 ─────────────

    pub fn recycle_load(&mut self, drive_id: &str) {
        let Some(slot) = self.slot_by_id(drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            return;
        };
        self.tools.recycle.loading = true;
        self.tools.recycle.error.clear();
        let id = drive_id.to_string();
        self.spawn(async move {
            Event::RecycleListed {
                drive: id,
                result: drive.recycle_list().await,
            }
        });
    }

    pub fn recycle_restore(&mut self, drive_id: &str, ids: Vec<String>) {
        let Some(slot) = self.slot_by_id(drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            return;
        };
        self.tools.recycle.loading = true;
        self.spawn(async move {
            Event::RecycleOp {
                message: format!("恢复 {} 项", ids.len()),
                result: drive.recycle_restore(&ids).await.map(|r| r.summary()),
            }
        });
    }

    pub fn recycle_delete(&mut self, drive_id: &str, ids: Vec<String>) {
        let Some(slot) = self.slot_by_id(drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            return;
        };
        self.tools.recycle.loading = true;
        self.spawn(async move {
            Event::RecycleOp {
                message: format!("彻底删除 {} 项", ids.len()),
                result: drive.recycle_delete(&ids).await.map(|r| r.summary()),
            }
        });
    }

    pub fn recycle_clear(&mut self, drive_id: &str) {
        let Some(slot) = self.slot_by_id(drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            return;
        };
        self.tools.recycle.loading = true;
        self.spawn(async move {
            Event::RecycleOp {
                message: "清空回收站".into(),
                result: drive.recycle_clear().await.map(|r| r.summary()),
            }
        });
    }

    pub fn cloud_tasks_load(&mut self, drive_id: &str) {
        let Some(slot) = self.slot_by_id(drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            return;
        };
        self.tools.cloud_tasks.loading = true;
        self.tools.cloud_tasks.error.clear();
        let id = drive_id.to_string();
        self.spawn(async move {
            Event::CloudTasksListed {
                drive: id,
                result: drive.cloud_tasks().await,
            }
        });
    }

    pub fn cloud_add(&mut self, drive_id: &str, url: String, dest: String) {
        let Some(slot) = self.slot_by_id(drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            return;
        };
        self.tools.cloud_busy = true;
        self.spawn(async move {
            Event::CloudAdded {
                result: drive.cloud_add(&url, &dest).await,
            }
        });
    }

    pub fn records_load(&mut self, drive_id: &str) {
        let Some(slot) = self.slot_by_id(drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            return;
        };
        self.tools.records.loading = true;
        self.tools.records.error.clear();
        let id = drive_id.to_string();
        self.spawn(async move {
            Event::RecordsListed {
                drive: id,
                result: drive.upload_records(100).await,
            }
        });
    }

    pub fn shares_load(&mut self, drive_id: &str) {
        let Some(slot) = self.slot_by_id(drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            return;
        };
        self.tools.shares.loading = true;
        self.tools.shares.error.clear();
        let id = drive_id.to_string();
        self.spawn(async move {
            Event::SharesListed {
                drive: id,
                result: drive.list_shares().await,
            }
        });
    }

    pub fn share_info(&mut self, drive_id: &str, url: String, pwd: String) {
        let Some(slot) = self.slot_by_id(drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            return;
        };
        self.tools.share_busy = true;
        self.spawn(async move {
            Event::ShareInfoGot {
                result: drive.share_info(&url, &pwd).await,
            }
        });
    }

    pub fn share_save(&mut self, drive_id: &str, url: String, pwd: String, dest: String) {
        let Some(slot) = self.slot_by_id(drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            return;
        };
        self.tools.share_busy = true;
        self.spawn(async move {
            Event::ShareSaved {
                result: drive
                    .save_shared(&url, &pwd, &dest)
                    .await
                    .map(|r| format!("转存完成：{}", r.summary())),
            }
        });
    }

    pub fn categories_load(&mut self, drive_id: &str, category: String) {
        let Some(slot) = self.slot_by_id(drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            return;
        };
        self.tools.categories.loading = true;
        self.tools.categories.error.clear();
        let id = drive_id.to_string();
        self.spawn(async move {
            Event::CategoriesListed {
                drive: id,
                result: drive.category_list(&category).await,
            }
        });
    }

    /// 请求条目缩略图（异步；已缓存/加载中/不支持则忽略）。
    pub fn request_thumbnail(&mut self, entry: &RemoteEntry) {
        if !crate::thumbs::ThumbCache::is_candidate(entry) {
            return;
        }
        let drive_id = self.active_drive_id();
        if drive_id.is_empty() {
            return;
        }
        let key = crate::thumbs::ThumbCache::key(&drive_id, entry);
        if !self.thumbs.need(&key) {
            return;
        }
        let Some(slot) = self.slot_by_id(&drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            return;
        };
        self.thumbs.mark_loading(&key);
        let entry = entry.clone();
        self.spawn(async move {
            Event::ThumbLoaded {
                key,
                result: drive.fetch_thumbnail(&entry).await,
            }
        });
    }

    // ───────────── 动态重建网盘实例 ─────────────

    /// 按最新配置重建网盘实例（启用/停用/新增/删除后调用）。
    pub fn rebuild_drives(&mut self) {
        let active_id = self.active_drive_id();
        self.drives.clear();
        for dcfg in self.cfg.drives.clone() {
            if dcfg.enabled {
                self.drives.push(Self::build_slot(&self.paths, &dcfg));
            }
        }
        // 尽量保持原选中网盘
        self.active = self
            .slot_idx(&active_id)
            .unwrap_or(0)
            .min(self.drives.len().saturating_sub(1));
        self.browse.clear();
        self.refresh_all_accounts();
        if let Some(slot) = self.drives.get(self.active) {
            let id = slot.id().to_string();
            self.load_dir_for(&id, "/");
        }
    }

    // ───────────── 登录异步封装（对话框调用） ─────────────

    pub fn begin_qr_async(&mut self, drive_id: &str) {
        let Some(slot) = self.slot_by_id(drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            return;
        };
        let id = drive_id.to_string();
        self.spawn(async move {
            Event::QrStarted {
                drive: id,
                result: drive.begin_qr().await,
            }
        });
    }

    pub fn poll_qr_async(&mut self, drive_id: &str) {
        let Some(slot) = self.slot_by_id(drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            return;
        };
        let id = drive_id.to_string();
        self.spawn(async move {
            Event::QrPolled {
                drive: id,
                result: drive.poll_qr().await,
            }
        });
    }

    pub fn login_cookie_async(&mut self, drive_id: &str, text: String) {
        let Some(slot) = self.slot_by_id(drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            return;
        };
        let id = drive_id.to_string();
        self.spawn(async move {
            Event::LoggedIn {
                drive: id,
                result: drive.login_cookie(&text).await,
            }
        });
    }

    pub fn login_sms_send_async(&mut self, drive_id: &str, phone: String) {
        let Some(slot) = self.slot_by_id(drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            return;
        };
        let id = drive_id.to_string();
        self.spawn(async move {
            Event::OpDone {
                message: "发送验证码".into(),
                refresh: false,
                result: drive.send_sms(&phone).await,
            }
        });
        let _ = id;
    }

    pub fn login_sms_verify_async(&mut self, drive_id: &str, phone: String, code: String) {
        let Some(slot) = self.slot_by_id(drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            return;
        };
        let id = drive_id.to_string();
        self.spawn(async move {
            Event::LoggedIn {
                drive: id,
                result: drive.verify_sms(&phone, &code).await,
            }
        });
    }

    pub fn login_token_async(&mut self, drive_id: &str, token: String) {
        let Some(slot) = self.slot_by_id(drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            return;
        };
        let id = drive_id.to_string();
        self.spawn(async move {
            Event::LoggedIn {
                drive: id,
                result: drive.login_token(&token, "").await,
            }
        });
    }

    /// 扫码轮询确认成功后的收尾：取账号信息 → Event::LoggedIn（关闭对话框）。
    pub fn finalize_login_async(&mut self, drive_id: &str) {
        let Some(slot) = self.slot_by_id(drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            return;
        };
        let id = drive_id.to_string();
        self.spawn(async move {
            Event::LoggedIn {
                drive: id,
                result: drive.account().await,
            }
        });
    }

    // ───────────── 目录选择 / 移动复制 ─────────────

    pub fn load_picker_dir(&mut self, drive_id: &str, path: &str) {
        let Some(slot) = self.slot_by_id(drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            return;
        };
        let p = norm_path(path);
        self.spawn(async move {
            let result = drive.list(&p).await;
            Event::PickerListed { path: p, result }
        });
    }

    pub fn do_move_copy(
        &mut self,
        drive_id: &str,
        paths: &[String],
        dest_dir: &str,
        is_move: bool,
    ) {
        let Some(slot) = self.slot_by_id(drive_id) else {
            return;
        };
        let Some(drive) = slot.drive.clone() else {
            return;
        };
        let paths2 = paths.to_vec();
        let dest = dest_dir.to_string();
        let verb = if is_move { "移动" } else { "复制" };
        let n = paths.len();
        self.spawn(async move {
            let result = if is_move {
                drive.move_entries(&paths2, &dest).await
            } else {
                drive.copy_entries(&paths2, &dest).await
            };
            Event::OpDone {
                message: format!("{verb} {n} 项到 {dest}"),
                refresh: true,
                result: result.map(|r| r.summary()),
            }
        });
    }
}

enum FilePickerAction {
    Close,
    Navigate(PathBuf),
    PickDir(PathBuf),
    PickFiles(Vec<PathBuf>),
}

/// 读取本地目录（同步，用于文件选择器；单目录条目数有限）。
fn load_local_dir(picker: &mut FilePicker, path: &std::path::Path) {
    let path = path.to_path_buf();
    let mut entries = Vec::new();
    match std::fs::read_dir(&path) {
        Ok(rd) => {
            for item in rd.flatten() {
                let name = item.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                let Ok(meta) = item.metadata() else { continue };
                entries.push(LocalEntry {
                    path: item.path(),
                    name,
                    is_dir: meta.is_dir(),
                    size: if meta.is_dir() { 0 } else { meta.len() },
                });
            }
            entries.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            picker.entries = entries;
            picker.path = path;
            picker.manual = picker.path.to_string_lossy().to_string();
            picker.error.clear();
        }
        Err(e) => picker.error = format!("无法打开目录：{e}"),
    }
}

/// 稳定 ID（egui 0.37 把 `Id::new` 标为 deprecated，这里集中处理）。
#[allow(deprecated)]
pub fn modal_id(salt: &str) -> egui::Id {
    egui::Id::new(salt)
}
