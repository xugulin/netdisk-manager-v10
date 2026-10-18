//! wp-transfer —— 传输引擎（GUI 版网盘管理的核心调度层）。
//!
//! 职责：
//! * 任务模型：上传 / 下载 / 跨盘复制三类任务，统一 [`TransferTask`] 台账；
//! * 并发调度：小文件 / 大文件分别走 `tokio::sync::Semaphore`，任务在 GUI 提供的
//!   tokio runtime 上 spawn；
//! * 控制：任务级暂停 / 继续 / 取消（`TaskControl`，进度回调即安全点）；
//! * 重试：指数退避；认证错误不自动重试；取消不算失败；
//! * 跨盘直传：源盘下载到本地中转文件 → 目标盘上传，进度按下载/上传各 50%；
//! * 持久化：`tasks_file` 原子写（节流 ≥1s），启动时 `load()` 恢复台账。
//!
//! # 线程模型
//! 引擎本身不创建 runtime。GUI 启动时调用 [`TransferEngine::set_runtime`] 注入
//! `tokio::runtime::Handle`，之后 [`TransferEngine::enqueue_upload`] 等方法才会把
//! 任务 spawn 出去；未设置 runtime 时任务保持 `Queued`，不会丢。
//!
//! # 适配器契约
//! 所有 I/O 都走 [`wp_drive::Drive`]：进度回调返回 `false` 时适配器应中止并返回
//! `wp_core::Error::Canceled`；引擎据此区分暂停 / 取消（不算失败）。

mod control;
mod engine;
mod store;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use wp_core::config::{DownloadConfig, TransferConfig};
use wp_core::guard::GuardDecision;
use wp_core::model::{DriveKind, TransferStats, TransferTask};
use wp_drive::Drive;

/// 上传前敏感词守卫钩子（GUI 用 `wp_core::guard::GuardManager` 实现）。
///
/// `instance` 是目标网盘实例标识；返回值第二项是最终文件名（未改名时原样返回）。
pub trait UploadGuardHook: Send + Sync {
    fn precheck(&self, instance: &str, kind: DriveKind, name: &str) -> (GuardDecision, String);
}

/// 引擎构造参数。
#[derive(Clone)]
pub struct EngineOptions {
    pub transfer: TransferConfig,
    pub download: DownloadConfig,
    /// 跨盘复制的中转目录（引擎会确保存在）。
    pub cache_dir: PathBuf,
    /// 任务台账文件（JSON，原子写）。
    pub tasks_file: PathBuf,
    /// 默认下载目录。
    pub download_dir: PathBuf,
    /// 失败/取消后是否保留跨盘中转文件。
    pub keep_failed_temp: bool,
    /// 上传前守卫（None = 不检查）。
    pub guard: Option<Arc<dyn UploadGuardHook>>,
}

/// 传输引擎。
///
/// 内部全部是 `Arc<Mutex<...>>`，控制类方法只取 `&self`，可由 GUI 任意线程调用；
/// `set_download_dir` / `update_config` / `load` / `set_runtime` 按签名取 `&mut self`。
pub struct TransferEngine {
    inner: Arc<engine::Inner>,
    /// 默认下载目录的镜像，用于 `download_dir()` 返回稳定引用。
    download_dir: PathBuf,
}

impl TransferEngine {
    /// 创建引擎（不启动工作线程；runtime 由 GUI 传入）。
    pub fn new(opts: EngineOptions) -> Self {
        let download_dir = opts.download_dir.clone();
        Self {
            inner: Arc::new(engine::Inner::new(opts)),
            download_dir,
        }
    }

    /// 设置 tokio runtime handle（GUI 启动时调用一次；之后 enqueue 才能调度）。
    ///
    /// 重复调用会替换 handle；已在跑的任务不受影响。
    pub fn set_runtime(&mut self, rt: tokio::runtime::Handle) {
        self.inner.set_runtime(rt);
    }

    /// 当前任务快照（含运行时速率；顺序与入队/加载顺序一致）。
    pub fn tasks(&self) -> Vec<TransferTask> {
        self.inner.tasks()
    }

    /// 传输页顶部统计。
    pub fn stats(&self) -> TransferStats {
        self.inner.stats()
    }

    pub fn download_dir(&self) -> &Path {
        &self.download_dir
    }

    pub fn set_download_dir(&mut self, dir: PathBuf) {
        self.inner.set_download_dir(dir.clone());
        self.download_dir = dir;
    }

    pub fn update_config(&mut self, transfer: TransferConfig, download: DownloadConfig) {
        self.inner.update_config(transfer, download);
    }

    // ───────────── 入队 ─────────────

    /// 本地上传到网盘目录，返回任务 id。
    pub fn enqueue_upload(
        &self,
        drive: Arc<dyn Drive>,
        local: PathBuf,
        remote_dir: String,
    ) -> String {
        self.inner.enqueue_upload(drive, local, remote_dir)
    }

    /// 网盘下载到本地：`save_as` 为空时落到下载目录（同名自动加 ` (n)`）。
    pub fn enqueue_download(
        &self,
        drive: Arc<dyn Drive>,
        remote_path: String,
        save_as: Option<PathBuf>,
    ) -> String {
        self.inner.enqueue_download(drive, remote_path, save_as)
    }

    /// 跨盘复制：源盘 → 本地缓存 `<cache_dir>/<task_id>.tmp` → 目标盘。
    pub fn enqueue_cross_copy(
        &self,
        src: Arc<dyn Drive>,
        src_path: String,
        dst: Arc<dyn Drive>,
        dst_dir: String,
    ) -> String {
        self.inner.enqueue_cross_copy(src, src_path, dst, dst_dir)
    }

    // ───────────── 控制 ─────────────

    /// 暂停：排队任务立即转暂停且不会开跑；运行中任务在下一个进度回调安全点退出。
    pub fn pause(&self, id: &str) {
        self.inner.pause(id);
    }

    /// 继续：暂停任务重新排队并调度；运行中任务清除暂停标记。
    pub fn resume(&self, id: &str) {
        self.inner.resume(id);
    }

    /// 取消：排队/暂停任务立即取消；运行中任务在下一个安全检查点退出。
    pub fn cancel(&self, id: &str) {
        self.inner.cancel(id);
    }

    /// 失败/取消的任务重新排队（重试计数清零）。
    pub fn retry(&self, id: &str) {
        self.inner.retry(id);
    }

    /// 从列表移除（仅已结束任务）。
    pub fn remove(&self, id: &str) {
        self.inner.remove(id);
    }

    pub fn clear_finished(&self) {
        self.inner.clear_finished();
    }

    pub fn pause_all(&self) {
        self.inner.pause_all();
    }

    pub fn resume_all(&self) {
        self.inner.resume_all();
    }

    pub fn cancel_all(&self) {
        self.inner.cancel_all();
    }

    // ───────────── 持久化 ─────────────

    /// 原子写 `tasks_file`；内部节流（≥1s），可频繁调用。
    pub fn save(&self) {
        self.inner.save();
    }

    /// 启动恢复：Running → Paused，Queued 保留，Done/Failed/Canceled 原样保留。
    ///
    /// 注意：任务台账只存实例标识，不存驱动对象。启动后旧任务需要重新选择
    /// 网盘实例才能继续（当前公开 API 没有驱动解析钩子，见汇报中的已知限制）。
    pub fn load(&mut self) {
        self.inner.load();
    }
}
