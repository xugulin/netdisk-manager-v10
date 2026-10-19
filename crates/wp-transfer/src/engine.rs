//! 引擎内部：台账、调度、执行、进度、持久化。
//!
//! 目录（含多重嵌套）支持：上传/下载/跨盘复制在 `prepare_total` 递归统计总大小，
//! 在 `execute_once` 递归建目录并逐文件传输（目标端保持相对目录结构）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::runtime::Handle;
use wp_core::config::{DownloadConfig, TransferConfig};
use wp_core::guard::GuardDecision;
use wp_core::model::{
    DownloadOptions, DriveKind, ProgressFn, TaskKind, TaskState, TransferStats, TransferTask,
    UploadOptions,
};
use wp_core::Error;
use wp_drive::Drive;

use crate::control::{GateOutcome, GatePermit, GatePool, InstancePermit, RunPermits, TaskControl};
use crate::store;
use crate::{EngineOptions, UploadGuardHook};

/// 保存节流窗口。
const SAVE_THROTTLE: Duration = Duration::from_secs(1);
/// 退避上限（毫秒）。
const MAX_BACKOFF_MS: u64 = 60_000;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ts() -> i64 {
    wp_core::util::now_ts()
}

// ───────────────────────── 选项 / 台账条目 ─────────────────────────

struct Options {
    transfer: TransferConfig,
    download: DownloadConfig,
    cache_dir: PathBuf,
    tasks_file: PathBuf,
    download_dir: PathBuf,
    keep_failed_temp: bool,
    guard: Option<Arc<dyn UploadGuardHook>>,
}

struct TaskEntry {
    task: TransferTask,
    src: Option<Arc<dyn Drive>>,
    dst: Option<Arc<dyn Drive>>,
    control: Arc<TaskControl>,
    /// 运行代际：每次重新调度 +1，旧 spawn 结束后不得覆盖新状态。
    generation: u64,
    last_tick: Instant,
    last_done: u64,
}

impl TaskEntry {
    fn new(task: TransferTask, src: Option<Arc<dyn Drive>>, dst: Option<Arc<dyn Drive>>) -> Self {
        let last_done = task.done;
        Self {
            task,
            src,
            dst,
            control: TaskControl::new(),
            generation: 0,
            last_tick: Instant::now(),
            last_done,
        }
    }
}

/// 任务执行所需快照（避免持锁跨 await）。
struct RunCtx {
    id: String,
    kind: TaskKind,
    control: Arc<TaskControl>,
    src: Option<Arc<dyn Drive>>,
    dst: Option<Arc<dyn Drive>>,
    src_path: String,
    dst_dir: String,
    local: PathBuf,
    file_name: String,
    instance: String,
    dst_kind: DriveKind,
    /// 源实例 id / 类型（下载、跨盘的源侧限流用）
    src_instance: String,
    src_kind: DriveKind,
    guard: Option<Arc<dyn UploadGuardHook>>,
    download: DownloadConfig,
    transfer: TransferConfig,
    cache_dir: PathBuf,
}

/// 单次执行结果。
struct ExecOutcome {
    remote_name: Option<String>,
    total: Option<u64>,
    done: u64,
}

/// 执行错误分类：认证 / 普通失败 / 取消。
enum ExecError {
    Auth(Error),
    Failed(Error),
    Canceled,
}

impl ExecError {
    fn from_error(e: Error) -> Self {
        if e.is_auth() {
            ExecError::Auth(e)
        } else if e.is_canceled() {
            // 适配器返回取消（进度回调返回 false / 内部取消）：不重试、不算失败。
            ExecError::Canceled
        } else {
            ExecError::Failed(e)
        }
    }
}

/// 任务终止方式。
enum Finish {
    State(TaskState),
    Done(ExecOutcome),
    Failed(String),
}

/// 进度加权方式。
#[derive(Clone, Copy)]
enum ProgressMode {
    Direct,
    CrossDownload { total: u64 },
    CrossUpload { total: u64 },
}

enum AcquireError {
    Stop(TaskState),
    Closed,
}

// ───────────────────────── 引擎内部 ─────────────────────────

pub(crate) struct Inner {
    opts: Mutex<Options>,
    data: Mutex<Vec<TaskEntry>>,
    gates: Mutex<GatePool>,
    rt: Mutex<Option<Handle>>,
    /// 限流冷却截止时间（实例 id → Instant）：127/频繁错误后该实例暂停新请求。
    cooldowns: Mutex<std::collections::HashMap<String, Instant>>,
    dirty: AtomicBool,
    last_save: Mutex<Option<Instant>>,
    save_lock: Mutex<()>,
}

impl Inner {
    pub(crate) fn new(opts: EngineOptions) -> Self {
        let options = Options {
            transfer: opts.transfer.clone(),
            download: opts.download.clone(),
            cache_dir: opts.cache_dir.clone(),
            tasks_file: opts.tasks_file.clone(),
            download_dir: opts.download_dir.clone(),
            keep_failed_temp: opts.keep_failed_temp,
            guard: opts.guard.clone(),
        };
        let gates = GatePool::new(&options.transfer);
        for dir in [&options.cache_dir, &options.download_dir] {
            if let Err(e) = std::fs::create_dir_all(dir) {
                wp_core::log_warn!("传输", "创建目录失败（{}）：{}", dir.display(), e);
            }
        }
        if let Some(parent) = options.tasks_file.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    wp_core::log_warn!("传输", "创建任务库目录失败（{}）：{}", parent.display(), e);
                }
            }
        }
        Self {
            opts: Mutex::new(options),
            data: Mutex::new(Vec::new()),
            gates: Mutex::new(gates),
            rt: Mutex::new(None),
            cooldowns: Mutex::new(std::collections::HashMap::new()),
            dirty: AtomicBool::new(false),
            last_save: Mutex::new(None),
            save_lock: Mutex::new(()),
        }
    }

    // ───────────── 查询 ─────────────

    pub(crate) fn tasks(&self) -> Vec<TransferTask> {
        lock(&self.data).iter().map(|e| e.task.clone()).collect()
    }

    pub(crate) fn stats(&self) -> TransferStats {
        let mut stats = TransferStats::default();
        for e in lock(&self.data).iter() {
            match e.task.state {
                TaskState::Running => stats.active += 1,
                TaskState::Queued => stats.queued += 1,
                TaskState::Done => stats.done += 1,
                TaskState::Failed => stats.failed += 1,
                _ => {}
            }
            stats.total_done_bytes = stats.total_done_bytes.saturating_add(e.task.done);
            if e.task.state == TaskState::Running {
                stats.speed += e.task.speed;
            }
        }
        stats
    }

    pub(crate) fn set_download_dir(&self, dir: PathBuf) {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            wp_core::log_warn!("传输", "创建下载目录失败（{}）：{}", dir.display(), e);
        }
        lock(&self.opts).download_dir = dir.clone();
        wp_core::log_info!("传输", "下载目录已切换为 {}", dir.display());
    }

    pub(crate) fn update_config(&self, transfer: TransferConfig, download: DownloadConfig) {
        {
            let mut opts = lock(&self.opts);
            opts.transfer = transfer.clone();
            opts.download = download;
        }
        // 原地调整信号量：在跑任务归还许可时仍落在同一个闸门上。
        lock(&self.gates).update(&transfer);
        wp_core::log_info!(
            "传输",
            "传输参数已更新：小文件并发 {} / 大文件并发 {} / 重试 {} / 自适应 {}",
            transfer.small_file_concurrency,
            transfer.large_file_concurrency,
            transfer.retry,
            transfer.adaptive
        );
    }

    pub(crate) fn set_runtime(self: &Arc<Self>, rt: Handle) {
        *lock(&self.rt) = Some(rt);
        wp_core::log_info!("传输", "已绑定 tokio runtime，开始调度排队任务");
        let ids: Vec<String> = {
            let data = lock(&self.data);
            data.iter()
                .filter(|e| e.task.state == TaskState::Queued && can_run(e))
                .filter(|e| !e.control.is_stopped())
                .map(|e| e.task.id.clone())
                .collect()
        };
        for id in ids {
            self.schedule(&id);
        }
    }

    // ───────────── 入队 ─────────────

    fn next_id(&self) -> String {
        for _ in 0..16 {
            let id = wp_core::util::gen_task_id();
            if !lock(&self.data).iter().any(|e| e.task.id == id) {
                return id;
            }
        }
        format!(
            "{}-{}",
            wp_core::util::gen_task_id(),
            wp_core::util::random_hex(4)
        )
    }

    pub(crate) fn enqueue_upload(
        self: &Arc<Self>,
        drive: Arc<dyn Drive>,
        local: PathBuf,
        remote_dir: String,
    ) -> String {
        let id = self.next_id();
        let file_name = local
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "未命名".to_string());
        let total = std::fs::metadata(&local)
            .map(|m| if m.is_dir() { 0 } else { m.len() })
            .unwrap_or(0);
        let now = now_ts();
        let task = TransferTask {
            id: id.clone(),
            kind: TaskKind::Upload,
            src_drive: String::new(),
            dst_drive: drive.id().to_string(),
            src_path: String::new(),
            dst_dir: wp_core::util::norm_path(&remote_dir),
            local_path: local.to_string_lossy().to_string(),
            file_name,
            total,
            done: 0,
            state: TaskState::Queued,
            error: String::new(),
            retries: 0,
            created: now,
            updated: now,
            speed: 0.0,
            note: String::new(),
        };
        wp_core::log_info!(
            "传输",
            "新增上传任务 {}：{} → {}",
            id,
            task.file_name,
            task.dst_dir
        );
        lock(&self.data).push(TaskEntry::new(task, None, Some(drive)));
        self.mark_dirty();
        self.schedule(&id);
        id
    }

    pub(crate) fn enqueue_download(
        self: &Arc<Self>,
        drive: Arc<dyn Drive>,
        remote_path: String,
        save_as: Option<PathBuf>,
    ) -> String {
        let id = self.next_id();
        let remote = wp_core::util::norm_path(&remote_path);
        let mut name = wp_core::util::sanitize_file_name(&wp_core::util::base_name(&remote));
        if name.trim().is_empty() {
            name = "未命名".to_string();
        }
        let local = self.reserve_download_path(save_as, &name);
        let now = now_ts();
        let task = TransferTask {
            id: id.clone(),
            kind: TaskKind::Download,
            src_drive: drive.id().to_string(),
            dst_drive: String::new(),
            src_path: remote,
            dst_dir: String::new(),
            local_path: local.to_string_lossy().to_string(),
            file_name: name,
            total: 0,
            done: 0,
            state: TaskState::Queued,
            error: String::new(),
            retries: 0,
            created: now,
            updated: now,
            speed: 0.0,
            note: String::new(),
        };
        wp_core::log_info!(
            "传输",
            "新增下载任务 {}：{} → {}",
            id,
            task.src_path,
            local.display()
        );
        lock(&self.data).push(TaskEntry::new(task, Some(drive), None));
        self.mark_dirty();
        self.schedule(&id);
        id
    }

    pub(crate) fn enqueue_cross_copy(
        self: &Arc<Self>,
        src: Arc<dyn Drive>,
        src_path: String,
        dst: Arc<dyn Drive>,
        dst_dir: String,
    ) -> String {
        let id = self.next_id();
        let remote = wp_core::util::norm_path(&src_path);
        let mut name = wp_core::util::base_name(&remote);
        if name.trim().is_empty() || name == "/" {
            name = "未命名".to_string();
        }
        let cache_dir = lock(&self.opts).cache_dir.clone();
        if let Err(e) = std::fs::create_dir_all(&cache_dir) {
            wp_core::log_warn!("传输", "创建缓存目录失败（{}）：{}", cache_dir.display(), e);
        }
        let temp = cache_dir.join(format!("{id}.tmp"));
        let now = now_ts();
        let task = TransferTask {
            id: id.clone(),
            kind: TaskKind::CrossCopy,
            src_drive: src.id().to_string(),
            dst_drive: dst.id().to_string(),
            src_path: remote,
            dst_dir: wp_core::util::norm_path(&dst_dir),
            local_path: temp.to_string_lossy().to_string(),
            file_name: name,
            total: 0,
            done: 0,
            state: TaskState::Queued,
            error: String::new(),
            retries: 0,
            created: now,
            updated: now,
            speed: 0.0,
            note: String::new(),
        };
        wp_core::log_info!(
            "传输",
            "新增跨盘任务 {}：{}:{} → {}:{}",
            id,
            task.src_drive,
            task.src_path,
            task.dst_drive,
            task.dst_dir
        );
        lock(&self.data).push(TaskEntry::new(task, Some(src), Some(dst)));
        self.mark_dirty();
        self.schedule(&id);
        id
    }

    /// 下载落点：`save_as` 显式文件路径优先；目录 / 默认下载目录内自动避让同名。
    fn reserve_download_path(&self, save_as: Option<PathBuf>, name: &str) -> PathBuf {
        let download_dir = lock(&self.opts).download_dir.clone();
        let base = match save_as {
            Some(path) => {
                let as_text = path.to_string_lossy();
                let looks_dir = path.is_dir() || as_text.ends_with('/') || as_text.ends_with('\\');
                if looks_dir {
                    path
                } else {
                    // 显式文件路径：按用户指定，同名由适配器断点续传/覆盖。
                    return path;
                }
            }
            None => download_dir,
        };
        if let Err(e) = std::fs::create_dir_all(&base) {
            wp_core::log_warn!("传输", "创建下载目录失败（{}）：{}", base.display(), e);
        }
        let reserved: Vec<PathBuf> = lock(&self.data)
            .iter()
            .filter(|e| !e.task.state.is_finished() && !e.task.local_path.is_empty())
            .map(|e| PathBuf::from(&e.task.local_path))
            .collect();
        unique_destination(&base, name, &reserved)
    }

    // ───────────── 调度 ─────────────

    pub(crate) fn schedule(self: &Arc<Self>, id: &str) {
        let rt = lock(&self.rt).clone();
        let Some(rt) = rt else {
            wp_core::log_warn!("传输", "未设置 tokio runtime，任务 {} 保持排队", id);
            return;
        };
        let generation = {
            let mut data = lock(&self.data);
            let Some(entry) = data.iter_mut().find(|e| e.task.id == id) else {
                return;
            };
            if entry.control.is_stopped() || entry.task.state.is_finished() {
                return;
            }
            entry.generation = entry.generation.wrapping_add(1);
            entry.generation
        };
        rt.spawn(run_task(self.clone(), id.to_string(), generation));
    }

    fn run_context(&self, id: &str, generation: u64) -> Option<RunCtx> {
        let (task, src, dst, control) = {
            let data = lock(&self.data);
            let entry = data.iter().find(|e| e.task.id == id)?;
            if entry.generation != generation {
                return None;
            }
            (
                entry.task.clone(),
                entry.src.clone(),
                entry.dst.clone(),
                entry.control.clone(),
            )
        };
        if !can_run_parts(task.kind, src.is_some(), dst.is_some()) {
            return None;
        }
        let opts = lock(&self.opts);
        let dst_kind = dst.as_ref().map(|d| d.kind()).unwrap_or(DriveKind::Local);
        let instance = dst.as_ref().map(|d| d.id().to_string()).unwrap_or_default();
        let src_kind = src.as_ref().map(|d| d.kind()).unwrap_or(DriveKind::Local);
        let src_instance = src.as_ref().map(|d| d.id().to_string()).unwrap_or_default();
        Some(RunCtx {
            id: task.id.clone(),
            kind: task.kind,
            control,
            src,
            dst,
            src_path: task.src_path.clone(),
            dst_dir: task.dst_dir.clone(),
            local: PathBuf::from(&task.local_path),
            file_name: task.file_name.clone(),
            instance,
            dst_kind,
            src_instance,
            src_kind,
            guard: opts.guard.clone(),
            download: opts.download.clone(),
            transfer: opts.transfer.clone(),
            cache_dir: opts.cache_dir.clone(),
        })
    }

    // ───────────── 控制 ─────────────

    pub(crate) fn pause(&self, id: &str) {
        let mut changed = false;
        {
            let mut data = lock(&self.data);
            if let Some(e) = data.iter_mut().find(|e| e.task.id == id) {
                match e.task.state {
                    TaskState::Queued => {
                        e.control.request_pause();
                        e.task.state = TaskState::Paused;
                        e.task.updated = now_ts();
                        changed = true;
                    }
                    TaskState::Running => {
                        e.control.request_pause();
                        changed = true;
                    }
                    _ => {}
                }
            }
        }
        if changed {
            self.mark_dirty();
            wp_core::log_info!("传输", "任务 {} 已请求暂停", id);
            self.save();
        }
    }

    pub(crate) fn resume(self: &Arc<Self>, id: &str) {
        let mut schedule = false;
        let mut missing = false;
        {
            let mut data = lock(&self.data);
            if let Some(e) = data.iter_mut().find(|e| e.task.id == id) {
                match e.task.state {
                    TaskState::Paused => {
                        if !can_run(e) {
                            e.task.state = TaskState::Failed;
                            e.task.error = "缺少网盘实例，无法恢复；请重新创建该任务".to_string();
                            e.task.updated = now_ts();
                            missing = true;
                        } else {
                            e.control.request_resume();
                            e.task.state = TaskState::Queued;
                            e.task.error.clear();
                            e.task.retries = 0;
                            e.task.updated = now_ts();
                            schedule = true;
                        }
                    }
                    TaskState::Running => {
                        e.control.request_resume();
                    }
                    TaskState::Queued if e.control.is_paused() => {
                        if can_run(e) {
                            e.control.request_resume();
                            e.task.updated = now_ts();
                            schedule = true;
                        } else {
                            e.task.state = TaskState::Failed;
                            e.task.error = "缺少网盘实例，无法恢复；请重新创建该任务".to_string();
                            e.task.updated = now_ts();
                            missing = true;
                        }
                    }
                    _ => {}
                }
            }
        }
        if missing {
            self.mark_dirty();
            wp_core::log_warn!("传输", "任务 {} 无法恢复：缺少网盘实例", id);
            self.save();
            return;
        }
        if schedule {
            self.mark_dirty();
            wp_core::log_info!("传输", "任务 {} 已继续", id);
            self.schedule(id);
            self.save();
        }
    }

    pub(crate) fn cancel(&self, id: &str) {
        let keep_temp = lock(&self.opts).keep_failed_temp;
        let mut changed = false;
        let mut cleanup: Option<PathBuf> = None;
        {
            let mut data = lock(&self.data);
            if let Some(e) = data.iter_mut().find(|e| e.task.id == id) {
                match e.task.state {
                    TaskState::Queued | TaskState::Paused => {
                        e.control.request_cancel();
                        e.task.state = TaskState::Canceled;
                        e.task.error.clear();
                        e.task.speed = 0.0;
                        e.task.updated = now_ts();
                        if e.task.kind == TaskKind::CrossCopy && !keep_temp {
                            cleanup = Some(PathBuf::from(&e.task.local_path));
                        }
                        changed = true;
                    }
                    TaskState::Running => {
                        e.control.request_cancel();
                        changed = true;
                    }
                    _ => {}
                }
            }
        }
        if changed {
            if let Some(path) = cleanup {
                remove_temp(&path);
            }
            self.mark_dirty();
            wp_core::log_info!("传输", "任务 {} 已请求取消", id);
            self.save();
        }
    }

    pub(crate) fn retry(self: &Arc<Self>, id: &str) {
        let mut schedule = false;
        let mut missing = false;
        {
            let mut data = lock(&self.data);
            if let Some(e) = data.iter_mut().find(|e| e.task.id == id) {
                if !e.task.state.is_finished() {
                    return;
                }
                if !can_run(e) {
                    e.task.state = TaskState::Failed;
                    e.task.error = "缺少网盘实例，无法重试；请重新创建该任务".to_string();
                    e.task.updated = now_ts();
                    missing = true;
                } else {
                    e.control = TaskControl::new();
                    e.task.state = TaskState::Queued;
                    e.task.error.clear();
                    e.task.retries = 0;
                    e.task.done = 0;
                    e.task.speed = 0.0;
                    e.task.updated = now_ts();
                    schedule = true;
                }
            }
        }
        if missing {
            self.mark_dirty();
            wp_core::log_warn!("传输", "任务 {} 无法重试：缺少网盘实例", id);
            self.save();
            return;
        }
        if schedule {
            self.mark_dirty();
            wp_core::log_info!("传输", "任务 {} 已重新排队", id);
            self.schedule(id);
            self.save();
        }
    }

    pub(crate) fn remove(&self, id: &str) {
        let removed = {
            let mut data = lock(&self.data);
            let before = data.len();
            data.retain(|e| !(e.task.id == id && e.task.state.is_finished()));
            before != data.len()
        };
        if removed {
            self.mark_dirty();
            wp_core::log_info!("传输", "已从列表移除任务 {}", id);
            self.save();
        } else {
            wp_core::log_warn!("传输", "任务 {} 未结束，不能移出列表", id);
        }
    }

    pub(crate) fn clear_finished(&self) {
        let removed = {
            let mut data = lock(&self.data);
            let before = data.len();
            data.retain(|e| !e.task.state.is_finished());
            before - data.len()
        };
        if removed > 0 {
            self.mark_dirty();
            wp_core::log_info!("传输", "已清理 {} 个已结束任务", removed);
            self.save();
        }
    }

    pub(crate) fn pause_all(&self) {
        let mut touched = 0usize;
        {
            let mut data = lock(&self.data);
            for e in data.iter_mut() {
                match e.task.state {
                    TaskState::Queued => {
                        e.control.request_pause();
                        e.task.state = TaskState::Paused;
                        e.task.updated = now_ts();
                        touched += 1;
                    }
                    TaskState::Running => {
                        e.control.request_pause();
                        touched += 1;
                    }
                    _ => {}
                }
            }
        }
        if touched > 0 {
            self.mark_dirty();
            wp_core::log_info!("传输", "已暂停全部任务（{} 个）", touched);
            self.save();
        }
    }

    pub(crate) fn resume_all(self: &Arc<Self>) {
        let ids: Vec<String> = {
            let data = lock(&self.data);
            data.iter()
                .filter(|e| !e.task.state.is_finished())
                .filter(|e| {
                    e.task.state == TaskState::Paused
                        || e.control.is_paused()
                        || e.task.state == TaskState::Running
                })
                .map(|e| e.task.id.clone())
                .collect()
        };
        let count = ids.len();
        for id in ids {
            self.resume(&id);
        }
        if count > 0 {
            wp_core::log_info!("传输", "已继续 {} 个任务", count);
        }
    }

    pub(crate) fn cancel_all(&self) {
        let keep_temp = lock(&self.opts).keep_failed_temp;
        let mut touched = 0usize;
        let mut cleanup: Vec<PathBuf> = Vec::new();
        {
            let mut data = lock(&self.data);
            for e in data.iter_mut() {
                if e.task.state.is_finished() {
                    continue;
                }
                e.control.request_cancel();
                if e.task.state != TaskState::Running {
                    e.task.state = TaskState::Canceled;
                    e.task.error.clear();
                    e.task.speed = 0.0;
                    e.task.updated = now_ts();
                    if e.task.kind == TaskKind::CrossCopy && !keep_temp {
                        cleanup.push(PathBuf::from(&e.task.local_path));
                    }
                }
                touched += 1;
            }
        }
        for path in cleanup {
            remove_temp(&path);
        }
        if touched > 0 {
            self.mark_dirty();
            wp_core::log_info!("传输", "已取消全部任务（{} 个）", touched);
            self.save();
        }
    }

    // ───────────── 持久化 ─────────────

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::SeqCst);
    }

    pub(crate) fn save(&self) {
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let _write_guard = lock(&self.save_lock);

        {
            let last = lock(&self.last_save);
            if let Some(prev) = *last {
                if prev.elapsed() < SAVE_THROTTLE {
                    drop(last);
                    self.dirty.store(true, Ordering::SeqCst);
                    return;
                }
            }
        }

        let path = lock(&self.opts).tasks_file.clone();
        if path.as_os_str().is_empty() {
            self.dirty.store(true, Ordering::SeqCst);
            wp_core::log_warn!("传输", "任务库路径为空，跳过保存");
            return;
        }
        let tasks = self.tasks();
        let bytes = match store::encode(&tasks) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.dirty.store(true, Ordering::SeqCst);
                wp_core::log_error!("传输", "任务库序列化失败：{}", e);
                return;
            }
        };
        match wp_core::paths::atomic_write(&path, &bytes) {
            Ok(()) => {
                *lock(&self.last_save) = Some(Instant::now());
            }
            Err(e) => {
                self.dirty.store(true, Ordering::SeqCst);
                wp_core::log_error!("传输", "任务库写入失败（{}）：{}", path.display(), e);
            }
        }
    }

    pub(crate) fn load(&self) {
        let path = lock(&self.opts).tasks_file.clone();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                wp_core::log_info!("传输", "未找到任务库（{}），从空列表开始", path.display());
                return;
            }
            Err(e) => {
                wp_core::log_warn!("传输", "读取任务库失败（{}）：{}", path.display(), e);
                return;
            }
        };
        let tasks = match store::decode(&text) {
            Ok(tasks) => tasks,
            Err(e) => {
                wp_core::log_error!("传输", "任务库解析失败（{}）：{}", path.display(), e);
                return;
            }
        };
        let now = now_ts();
        let entries: Vec<TaskEntry> = tasks
            .into_iter()
            .map(|mut task| {
                task.speed = 0.0;
                match task.state {
                    TaskState::Running => {
                        task.state = TaskState::Paused;
                        task.updated = now;
                        task.note = append_note(&task.note, "应用重启，任务已暂停");
                    }
                    TaskState::Queued => {
                        task.updated = now;
                    }
                    _ => {}
                }
                TaskEntry::new(task, None, None)
            })
            .collect();
        let count = entries.len();
        *lock(&self.data) = entries;
        self.mark_dirty();
        wp_core::log_info!("传输", "已恢复 {} 个任务（运行中任务转为暂停）", count);
    }

    // ───────────── 进度 / 状态（执行期） ─────────────

    fn is_current(&self, id: &str, generation: u64) -> bool {
        lock(&self.data)
            .iter()
            .find(|e| e.task.id == id)
            .map(|e| e.generation == generation)
            .unwrap_or(false)
    }

    fn is_small(&self, total: u64) -> bool {
        lock(&self.gates).is_small(total)
    }

    fn update_progress(
        &self,
        id: &str,
        generation: u64,
        reported_done: u64,
        reported_total: u64,
        mode: ProgressMode,
    ) -> bool {
        let mut data = lock(&self.data);
        let Some(entry) = data.iter_mut().find(|e| e.task.id == id) else {
            return false;
        };
        if entry.generation != generation || entry.control.is_stopped() {
            return false;
        }
        let current_total = entry.task.total;
        let full_total = match mode {
            ProgressMode::Direct => {
                if reported_total > 0 {
                    reported_total
                } else {
                    current_total
                }
            }
            ProgressMode::CrossDownload { total } | ProgressMode::CrossUpload { total } => {
                if total > 0 {
                    total
                } else if reported_total > 0 {
                    reported_total
                } else {
                    current_total
                }
            }
        };
        let logical_done = match mode {
            ProgressMode::Direct => reported_done,
            ProgressMode::CrossDownload { .. } => reported_done / 2,
            ProgressMode::CrossUpload { .. } => full_total / 2 + reported_done / 2,
        };
        if full_total > 0 {
            entry.task.total = full_total;
        }
        let cap = if entry.task.total > 0 {
            entry.task.total
        } else {
            logical_done
        };
        entry.task.done = logical_done.min(cap);
        update_meter(entry, logical_done);
        true
    }

    fn set_half_progress(&self, id: &str, generation: u64, total: u64) {
        let mut data = lock(&self.data);
        let Some(entry) = data.iter_mut().find(|e| e.task.id == id) else {
            return;
        };
        if entry.generation != generation {
            return;
        }
        if total > 0 {
            entry.task.total = total;
            entry.task.done = total / 2;
            update_meter(entry, total / 2);
        }
    }

    fn set_total_and_name(&self, id: &str, generation: u64, total: u64, name: &str) {
        let mut data = lock(&self.data);
        let Some(entry) = data.iter_mut().find(|e| e.task.id == id) else {
            return;
        };
        if entry.generation != generation {
            return;
        }
        entry.task.total = total;
        if !name.trim().is_empty() {
            entry.task.file_name = name.to_string();
        }
    }

    fn set_note(&self, id: &str, generation: u64, note: String) {
        let mut data = lock(&self.data);
        let Some(entry) = data.iter_mut().find(|e| e.task.id == id) else {
            return;
        };
        if entry.generation != generation {
            return;
        }
        entry.task.note = note;
        entry.task.updated = now_ts();
        self.mark_dirty();
    }

    fn begin_running(&self, id: &str, generation: u64) -> bool {
        let mut data = lock(&self.data);
        let Some(entry) = data.iter_mut().find(|e| e.task.id == id) else {
            return false;
        };
        if entry.generation != generation
            || entry.control.is_stopped()
            || entry.task.state.is_finished()
        {
            return false;
        }
        entry.task.state = TaskState::Running;
        entry.task.updated = now_ts();
        self.mark_dirty();
        true
    }

    fn record_retry(&self, id: &str, generation: u64, attempt: u32, error: &str) {
        let mut data = lock(&self.data);
        let Some(entry) = data.iter_mut().find(|e| e.task.id == id) else {
            return;
        };
        if entry.generation != generation {
            return;
        }
        entry.task.retries = attempt;
        entry.task.error = error.to_string();
        entry.task.state = TaskState::Running;
        entry.task.speed = 0.0;
        entry.task.updated = now_ts();
        entry.last_tick = Instant::now();
        entry.last_done = entry.task.done;
        self.mark_dirty();
    }

    /// 获取执行许可：先按「网盘实例」限流（防目录展开成 N 个任务打爆单盘），
    /// 再取全局小/大文件闸门。实例按 id 排序获取，避免跨盘任务互相死锁。
    async fn acquire(
        &self,
        control: &Arc<TaskControl>,
        small: bool,
        instances: &[(String, usize)],
    ) -> std::result::Result<RunPermits, AcquireError> {
        let mut held: Vec<InstancePermit> = Vec::new();
        for (key, limit) in instances {
            loop {
                if let Some(state) = stop_state(control) {
                    let mut gates = lock(&self.gates);
                    for p in held.drain(..) {
                        gates.release_instance(&p.key, p.permit);
                    }
                    return Err(AcquireError::Stop(state));
                }
                let sem = lock(&self.gates).instance_sem(key, *limit);
                let notified = control.notify.notified();
                tokio::pin!(notified);
                tokio::select! {
                    acquired = sem.acquire_owned() => {
                        match acquired {
                            Ok(permit) => {
                                held.push(InstancePermit { key: key.clone(), permit });
                                break;
                            }
                            Err(_) => {
                                let mut gates = lock(&self.gates);
                                for p in held.drain(..) {
                                    gates.release_instance(&p.key, p.permit);
                                }
                                return Err(AcquireError::Closed);
                            }
                        }
                    }
                    _ = &mut notified => {
                        continue;
                    }
                }
            }
        }
        loop {
            if let Some(state) = stop_state(control) {
                let mut gates = lock(&self.gates);
                for p in held.drain(..) {
                    gates.release_instance(&p.key, p.permit);
                }
                return Err(AcquireError::Stop(state));
            }
            let sem = lock(&self.gates).sem(small);
            let notified = control.notify.notified();
            tokio::pin!(notified);
            tokio::select! {
                acquired = sem.acquire_owned() => {
                    return match acquired {
                        Ok(permit) => Ok(RunPermits {
                            gate: GatePermit { small, permit },
                            instances: held,
                        }),
                        Err(_) => {
                            let mut gates = lock(&self.gates);
                            for p in held.drain(..) {
                                gates.release_instance(&p.key, p.permit);
                            }
                            Err(AcquireError::Closed)
                        }
                    };
                }
                _ = &mut notified => {
                    continue;
                }
            }
        }
    }

    fn release_gate(&self, permits: RunPermits, outcome: GateOutcome, adaptive: bool) {
        let mut gates = lock(&self.gates);
        match outcome {
            GateOutcome::Success => {
                gates.note_success(permits.gate.small, adaptive);
                for p in &permits.instances {
                    gates.note_success_instance(&p.key, adaptive);
                }
            }
            GateOutcome::Failure => {
                gates.note_failure(permits.gate.small, adaptive);
                for p in &permits.instances {
                    gates.note_failure_instance(&p.key, adaptive);
                }
            }
            GateOutcome::Neutral => {}
        }
        gates.release(permits.gate.small, permits.gate.permit);
        for p in permits.instances {
            gates.release_instance(&p.key, p.permit);
        }
    }

    /// 设置实例限流冷却（127/频繁错误后，同实例的新请求先等待）。
    fn set_rate_limit_cooldown(&self, keys: &[String], wait_ms: u64) {
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        let mut cd = lock(&self.cooldowns);
        for k in keys {
            let e = cd.entry(k.clone()).or_insert(deadline);
            if *e < deadline {
                *e = deadline;
            }
        }
    }

    /// 等待本任务涉及实例的限流冷却结束（可被暂停/取消打断）。
    async fn wait_rate_limit_cooldown(&self, ctx: &RunCtx) {
        let keys: Vec<String> = instance_limits(ctx).into_iter().map(|(k, _)| k).collect();
        if keys.is_empty() {
            return;
        }
        loop {
            let until = {
                let cd = lock(&self.cooldowns);
                keys.iter().filter_map(|k| cd.get(k).copied()).max()
            };
            let Some(until) = until else {
                return;
            };
            let now = Instant::now();
            if now >= until {
                return;
            }
            let ms = until.duration_since(now).as_millis() as u64;
            if sleep_with_control(&ctx.control, ms).await.is_some() {
                return;
            }
        }
    }

    fn finish(&self, id: &str, generation: u64, outcome: Finish) -> bool {
        let keep_failed_temp = lock(&self.opts).keep_failed_temp;
        let now = now_ts();
        let mut cleanup: Option<PathBuf> = None;
        let log_state: TaskState;
        let log_name: String;
        let log_error: String;
        {
            let mut data = lock(&self.data);
            let Some(entry) = data.iter_mut().find(|e| e.task.id == id) else {
                return false;
            };
            if entry.generation != generation {
                return false;
            }
            let outcome = if entry.control.is_canceled() {
                Finish::State(TaskState::Canceled)
            } else if entry.control.is_paused() {
                Finish::State(TaskState::Paused)
            } else {
                outcome
            };
            entry.task.updated = now;
            entry.task.speed = 0.0;
            match outcome {
                Finish::State(state) => {
                    entry.task.state = state;
                    entry.task.error.clear();
                }
                Finish::Done(out) => {
                    entry.task.state = TaskState::Done;
                    entry.task.error.clear();
                    if let Some(total) = out.total {
                        if total > 0 {
                            entry.task.total = total;
                        }
                    }
                    if let Some(name) = out.remote_name {
                        if !name.is_empty() {
                            entry.task.file_name = name;
                        }
                    }
                    entry.task.done = if entry.task.total > 0 {
                        entry.task.total
                    } else {
                        out.done
                    };
                }
                Finish::Failed(error) => {
                    entry.task.state = TaskState::Failed;
                    entry.task.error = error;
                }
            }
            log_state = entry.task.state;
            log_name = entry.task.file_name.clone();
            log_error = entry.task.error.clone();

            if entry.task.kind == TaskKind::CrossCopy {
                let temp = PathBuf::from(&entry.task.local_path);
                match entry.task.state {
                    TaskState::Done => cleanup = Some(temp),
                    TaskState::Failed | TaskState::Canceled => {
                        if keep_failed_temp {
                            let extra = format!("中转文件已保留：{}", temp.display());
                            entry.task.note = append_note(&entry.task.note, &extra);
                        } else {
                            cleanup = Some(temp);
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(temp) = cleanup {
            remove_temp(&temp);
        }
        self.mark_dirty();
        match log_state {
            TaskState::Done => {
                wp_core::log_info!("传输", "任务完成：{}（{}）", id, log_name)
            }
            TaskState::Failed => {
                wp_core::log_error!("传输", "任务失败：{}（{}）：{}", id, log_name, log_error)
            }
            TaskState::Canceled => {
                wp_core::log_info!("传输", "任务已取消：{}（{}）", id, log_name)
            }
            TaskState::Paused => {
                wp_core::log_info!("传输", "任务已暂停：{}（{}）", id, log_name)
            }
            _ => {}
        }
        self.save();
        true
    }
}

// ───────────────────────── 任务执行 ─────────────────────────

async fn run_task(inner: Arc<Inner>, id: String, generation: u64) {
    let Some(ctx) = inner.run_context(&id, generation) else {
        return;
    };
    if let Some(state) = stop_state(&ctx.control) {
        inner.finish(&id, generation, Finish::State(state));
        return;
    }

    // ── 上传守卫（上传 / 跨盘都要过目标盘守卫）──
    let mut rename_to: Option<String> = None;
    if ctx.kind != TaskKind::Download {
        if let Some(hook) = ctx.guard.as_ref() {
            let (decision, final_name) = hook.precheck(&ctx.instance, ctx.dst_kind, &ctx.file_name);
            match decision {
                GuardDecision::Pass => {}
                GuardDecision::Warn { hit } => {
                    inner.set_note(
                        &id,
                        generation,
                        format!("敏感词警告：命中「{hit}」（已继续上传）"),
                    );
                    wp_core::log_warn!("传输", "任务 {} 命中敏感词「{}」，按警告策略继续", id, hit);
                }
                GuardDecision::Block { hit } => {
                    inner.finish(
                        &id,
                        generation,
                        Finish::Failed(format!("上传已拦截：命中敏感词「{hit}」")),
                    );
                    return;
                }
                GuardDecision::Rename { hit, .. } => {
                    let new_name = if final_name.trim().is_empty() {
                        ctx.file_name.clone()
                    } else {
                        final_name
                    };
                    inner.set_note(
                        &id,
                        generation,
                        format!("命中敏感词「{hit}」，已改名为「{new_name}」"),
                    );
                    wp_core::log_warn!(
                        "传输",
                        "任务 {} 命中敏感词「{}」，改名为「{}」",
                        id,
                        hit,
                        new_name
                    );
                    rename_to = Some(new_name);
                }
            }
        }
    }
    if !inner.is_current(&id, generation) {
        return;
    }

    let retry_limit = ctx.transfer.retry;
    let backoff_base = ctx.transfer.retry_backoff_ms;
    let adaptive = ctx.transfer.adaptive;
    let mut attempt: u32 = 0;

    loop {
        if let Some(state) = stop_state(&ctx.control) {
            inner.finish(&id, generation, Finish::State(state));
            return;
        }
        if !inner.is_current(&id, generation) {
            return;
        }

        // 1) 取总大小（下载/跨盘需要 stat）——不占并发许可。
        let total = match prepare_total(&inner, &ctx, generation).await {
            Ok(total) => total,
            Err(ExecError::Auth(e)) => {
                inner.finish(
                    &id,
                    generation,
                    Finish::Failed(format!("{e}（需要重新登录）")),
                );
                return;
            }
            Err(ExecError::Canceled) => {
                inner.finish(&id, generation, Finish::State(TaskState::Canceled));
                return;
            }
            Err(ExecError::Failed(e)) => {
                if !on_transfer_error(
                    &inner,
                    &ctx,
                    generation,
                    &mut attempt,
                    e,
                    retry_limit,
                    backoff_base,
                )
                .await
                {
                    return;
                }
                continue;
            }
        };
        if !inner.is_current(&id, generation) {
            return;
        }

        // 2) 按大小获取并发许可（全局闸门 + 网盘实例闸门；暂停/取消可打断等待）。
        let small = inner.is_small(total);
        let permits = match inner
            .acquire(&ctx.control, small, &instance_limits(&ctx))
            .await
        {
            Ok(permit) => permit,
            Err(AcquireError::Stop(state)) => {
                inner.finish(&id, generation, Finish::State(state));
                return;
            }
            Err(AcquireError::Closed) => {
                inner.finish(
                    &id,
                    generation,
                    Finish::Failed("并发闸门已关闭".to_string()),
                );
                return;
            }
        };
        // 2.1) 若该实例刚被限流，先冷却（避免同一实例的任务一起撞墙）。
        inner.wait_rate_limit_cooldown(&ctx).await;
        if let Some(state) = stop_state(&ctx.control) {
            inner.release_gate(permits, GateOutcome::Neutral, adaptive);
            inner.finish(&id, generation, Finish::State(state));
            return;
        }
        if !inner.begin_running(&id, generation) {
            inner.release_gate(permits, GateOutcome::Neutral, adaptive);
            return;
        }
        wp_core::log_info!(
            "传输",
            "开始{}任务 {}：{}",
            ctx.kind.label(),
            id,
            ctx.file_name
        );

        // 3) 执行一次（含跨盘两段）。
        let result = execute_once(&inner, &ctx, generation, total, rename_to.as_deref()).await;

        // 执行期间可能被暂停/取消，或换代（resume/retry 重新调度）。
        if !inner.is_current(&id, generation) {
            inner.release_gate(permits, GateOutcome::Neutral, adaptive);
            return;
        }
        if let Some(state) = stop_state(&ctx.control) {
            inner.release_gate(permits, GateOutcome::Neutral, adaptive);
            inner.finish(&id, generation, Finish::State(state));
            return;
        }

        match result {
            Ok(outcome) => {
                inner.release_gate(permits, GateOutcome::Success, adaptive);
                inner.finish(&id, generation, Finish::Done(outcome));
                return;
            }
            Err(ExecError::Auth(e)) => {
                inner.release_gate(permits, GateOutcome::Neutral, adaptive);
                inner.finish(
                    &id,
                    generation,
                    Finish::Failed(format!("{e}（请重新登录该网盘）")),
                );
                return;
            }
            Err(ExecError::Canceled) => {
                inner.release_gate(permits, GateOutcome::Neutral, adaptive);
                inner.finish(&id, generation, Finish::State(TaskState::Canceled));
                return;
            }
            Err(ExecError::Failed(e)) => {
                inner.release_gate(permits, GateOutcome::Failure, adaptive);
                if !on_transfer_error(
                    &inner,
                    &ctx,
                    generation,
                    &mut attempt,
                    e,
                    retry_limit,
                    backoff_base,
                )
                .await
                {
                    return;
                }
            }
        }
    }
}

/// 处理一次普通失败：认证直接失败；否则按次数指数退避重试。返回 true 表示继续循环。
async fn on_transfer_error(
    inner: &Arc<Inner>,
    ctx: &RunCtx,
    generation: u64,
    attempt: &mut u32,
    error: Error,
    retry_limit: u32,
    backoff_base: u64,
) -> bool {
    if let Some(state) = stop_state(&ctx.control) {
        inner.finish(&ctx.id, generation, Finish::State(state));
        return false;
    }
    if error.is_auth() {
        inner.finish(
            &ctx.id,
            generation,
            Finish::Failed(format!("{error}（需要重新登录）")),
        );
        return false;
    }
    if error.is_canceled() {
        // 兜底：取消不是失败，不消耗重试次数。
        inner.finish(&ctx.id, generation, Finish::State(TaskState::Canceled));
        return false;
    }
    let rate_limited = is_rate_limited(&error);
    // 限流是瞬态拥塞：多给 3 次机会，并降低该实例并发+冷却，别和限流窗口硬碰。
    let allowed_retries = retry_limit + if rate_limited { 3 } else { 0 };
    if *attempt < allowed_retries {
        *attempt += 1;
        let wait = if rate_limited {
            let keys: Vec<String> = instance_limits(ctx).into_iter().map(|(k, _)| k).collect();
            let w = backoff_ms(
                backoff_base.saturating_mul(3).max(1_500),
                attempt.saturating_sub(1),
            )
            .min(30_000);
            inner.set_rate_limit_cooldown(&keys, w);
            wp_core::log_warn!(
                "传输",
                "任务 {} 触发限流，实例冷却 {}ms 并降低并发后重试：{}",
                ctx.id,
                w,
                error
            );
            w
        } else {
            backoff_ms(backoff_base, attempt.saturating_sub(1))
        };
        inner.record_retry(&ctx.id, generation, *attempt, &error.to_string());
        wp_core::log_warn!(
            "传输",
            "任务 {} 第 {} 次重试（等待 {}ms）：{}",
            ctx.id,
            *attempt,
            wait,
            error
        );
        if let Some(state) = sleep_with_control(&ctx.control, wait).await {
            inner.finish(&ctx.id, generation, Finish::State(state));
            return false;
        }
        return true;
    }
    inner.finish(
        &ctx.id,
        generation,
        Finish::Failed(format!("{error}（已重试 {} 次）", attempt)),
    );
    false
}

/// 供 GUI 入队前预扫描的本地目录结果（公开 API 的中间结构）。
pub(crate) fn scan_local_for_task(
    root: &Path,
) -> wp_core::Result<(Vec<String>, Vec<(String, PathBuf, u64)>)> {
    walk_local_tree(root)
        .map(|t| (t.dirs, t.files))
        .map_err(|e| match e {
            ExecError::Failed(er) | ExecError::Auth(er) => er,
            ExecError::Canceled => Error::other("扫描本地目录已取消"),
        })
}

/// 供 GUI 入队前预扫描的远端目录结果。
pub(crate) async fn scan_remote_for_task(
    drive: &Arc<dyn Drive>,
    root: &str,
) -> wp_core::Result<(Vec<String>, Vec<(String, u64)>)> {
    walk_remote_tree(drive, root)
        .await
        .map(|t| (t.dirs, t.files))
        .map_err(|e| match e {
            ExecError::Failed(er) | ExecError::Auth(er) => er,
            ExecError::Canceled => Error::other("扫描远端目录已取消"),
        })
}

/// 本地目录递归扫描结果。
struct LocalTree {
    /// 子目录相对路径（`a/b`，按字典序）
    dirs: Vec<String>,
    /// 文件：(相对路径, 绝对路径, 大小)
    files: Vec<(String, PathBuf, u64)>,
}

/// 远端目录递归扫描结果。
struct RemoteTree {
    dirs: Vec<String>,
    /// 文件：(相对路径, 大小)
    files: Vec<(String, u64)>,
}

fn walk_local_tree(root: &Path) -> std::result::Result<LocalTree, ExecError> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let rd = std::fs::read_dir(&dir).map_err(|e| ExecError::Failed(Error::Io(e)))?;
        for item in rd {
            let item = item.map_err(|e| ExecError::Failed(Error::Io(e)))?;
            let name = item.file_name().to_string_lossy().to_string();
            let path = item.path();
            let child_rel = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let meta =
                std::fs::symlink_metadata(&path).map_err(|e| ExecError::Failed(Error::Io(e)))?;
            if meta.is_dir() {
                dirs.push(child_rel.clone());
                stack.push((path, child_rel));
            } else if meta.is_file() {
                files.push((child_rel, path, meta.len()));
            }
        }
    }
    dirs.sort();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(LocalTree { dirs, files })
}

/// 递归扫描远端目录（逐层 `list`；含空目录）。
async fn walk_remote_tree(
    drive: &Arc<dyn Drive>,
    root: &str,
) -> std::result::Result<RemoteTree, ExecError> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut stack: Vec<(String, String)> = vec![(wp_core::util::norm_path(root), String::new())];
    let mut guard = 0usize;
    while let Some((dir, rel)) = stack.pop() {
        guard += 1;
        if guard > 200_000 {
            return Err(ExecError::Failed(Error::other(
                "目录层级/条目过多（>20 万），无法递归",
            )));
        }
        let entries = drive.list(&dir).await.map_err(ExecError::from_error)?;
        for e in entries {
            let child_rel = if rel.is_empty() {
                e.name.clone()
            } else {
                format!("{rel}/{}", e.name)
            };
            if e.is_dir {
                dirs.push(child_rel.clone());
                stack.push((e.path.clone(), child_rel));
            } else {
                files.push((child_rel, e.size));
            }
        }
    }
    dirs.sort();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(RemoteTree { dirs, files })
}

/// 把「单个文件内部进度」映射成「整个任务进度」。
///
/// * `base`：该文件之前已完成的字节（目录里前面文件之和）；
/// * `file_size`：当前文件大小；
/// * `cross`：跨盘传输时一个文件占两阶段（下载 0→50%，上传 50%→100%）；
/// * `phase`：0=下载阶段，1=上传阶段（cross=false 时忽略）。
#[allow(clippy::too_many_arguments)]
/// 取任务总大小：上传看本地 metadata；下载/跨盘先 stat 源（目录则递归统计）。
/// 取任务总大小（仅单文件任务；目录传输已由 wp-transfer 计划层展开为文件任务）。
async fn prepare_total(
    inner: &Arc<Inner>,
    ctx: &RunCtx,
    generation: u64,
) -> std::result::Result<u64, ExecError> {
    match ctx.kind {
        TaskKind::Upload => {
            let meta =
                std::fs::metadata(&ctx.local).map_err(|e| ExecError::Failed(Error::Io(e)))?;
            if meta.is_dir() {
                return Err(ExecError::Failed(Error::unsupported(
                    "目录上传已迁移到传输计划层：请先用 wp_transfer::plan::plan_upload 展开为文件任务",
                )));
            }
            let total = meta.len();
            inner.set_total_and_name(&ctx.id, generation, total, &ctx.file_name);
            Ok(total)
        }
        TaskKind::Download | TaskKind::CrossCopy => {
            let drive = ctx
                .src
                .as_ref()
                .ok_or_else(|| ExecError::Failed(Error::other("缺少源网盘实例")))?;
            let entry = drive
                .stat(&ctx.src_path)
                .await
                .map_err(ExecError::from_error)?;
            if entry.is_dir {
                return Err(ExecError::Failed(Error::unsupported(
                    "目录传输已迁移到传输计划层：请先用 plan_download / plan_cross_copy 展开为文件任务",
                )));
            }
            inner.set_total_and_name(&ctx.id, generation, entry.size, &entry.name);
            Ok(entry.size)
        }
    }
}

async fn execute_once(
    inner: &Arc<Inner>,
    ctx: &RunCtx,
    generation: u64,
    total: u64,
    rename_to: Option<&str>,
) -> std::result::Result<ExecOutcome, ExecError> {
    match ctx.kind {
        TaskKind::Upload => upload_once(inner, ctx, generation, total, rename_to).await,
        TaskKind::Download => download_once(inner, ctx, generation, total).await,
        TaskKind::CrossCopy => cross_copy_once(inner, ctx, generation, total, rename_to).await,
    }
}

/// 上传单文件（目录已由计划层展开为文件任务）。
async fn upload_once(
    inner: &Arc<Inner>,
    ctx: &RunCtx,
    generation: u64,
    _total: u64,
    rename_to: Option<&str>,
) -> std::result::Result<ExecOutcome, ExecError> {
    let drive = ctx
        .dst
        .as_ref()
        .ok_or_else(|| ExecError::Failed(Error::other("缺少目标网盘实例")))?;
    let meta = std::fs::metadata(&ctx.local).map_err(|e| ExecError::Failed(Error::Io(e)))?;
    if meta.is_dir() {
        return Err(ExecError::Failed(Error::unsupported(
            "目录上传已迁移到传输计划层：请先用 wp_transfer::plan::plan_upload 展开为文件任务",
        )));
    }
    let opts = UploadOptions {
        rename_to: rename_to.map(|s| s.to_string()),
        chunk_size: 0,
        concurrency: 0,
        skip_guard: true,
    };
    let progress = make_progress(
        inner,
        &ctx.id,
        generation,
        &ctx.control,
        ProgressMode::Direct,
    );
    let entry = drive
        .upload(&ctx.local, &ctx.dst_dir, &opts, progress)
        .await
        .map_err(ExecError::from_error)?;
    Ok(ExecOutcome {
        remote_name: if entry.name.is_empty() {
            None
        } else {
            Some(entry.name)
        },
        total: Some(entry.size),
        done: entry.size,
    })
}

/// 下载单文件（目录已由计划层展开为文件任务；本地已存在同大小文件由 plan_download 跳过）。
async fn download_once(
    inner: &Arc<Inner>,
    ctx: &RunCtx,
    generation: u64,
    total: u64,
) -> std::result::Result<ExecOutcome, ExecError> {
    let drive = ctx
        .src
        .as_ref()
        .ok_or_else(|| ExecError::Failed(Error::other("缺少源网盘实例")))?;
    let entry = drive
        .stat(&ctx.src_path)
        .await
        .map_err(ExecError::from_error)?;
    if entry.is_dir {
        return Err(ExecError::Failed(Error::unsupported(
            "目录下载已迁移到传输计划层：请先用 wp_transfer::plan::plan_download 展开为文件任务",
        )));
    }
    if let Some(parent) = ctx.local.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ExecError::Failed(Error::Io(e)))?;
        }
    }
    let opts = DownloadOptions {
        save_as: Some(ctx.local.clone()),
        segments: ctx.download.segments,
        segment_threshold: ctx.download.segment_threshold,
        resume: ctx.download.resume,
        limit_bps: ctx.transfer.max_speed_bps,
    };
    let progress = make_progress(
        inner,
        &ctx.id,
        generation,
        &ctx.control,
        ProgressMode::Direct,
    );
    let done = drive
        .download(&ctx.src_path, &ctx.local, &opts, progress)
        .await
        .map_err(ExecError::from_error)?;
    Ok(ExecOutcome {
        remote_name: None,
        total: Some(total.max(done)),
        done,
    })
}

/// 跨盘复制单文件（目录已由计划层展开为文件任务；目标目录先 ensure，失败时不白下载）。
async fn cross_copy_once(
    inner: &Arc<Inner>,
    ctx: &RunCtx,
    generation: u64,
    total: u64,
    rename_to: Option<&str>,
) -> std::result::Result<ExecOutcome, ExecError> {
    let src = ctx
        .src
        .as_ref()
        .ok_or_else(|| ExecError::Failed(Error::other("缺少源网盘实例")))?;
    let dst = ctx
        .dst
        .as_ref()
        .ok_or_else(|| ExecError::Failed(Error::other("缺少目标网盘实例")))?;
    tokio::fs::create_dir_all(&ctx.cache_dir)
        .await
        .map_err(|e| ExecError::Failed(Error::Io(e)))?;
    let entry = src
        .stat(&ctx.src_path)
        .await
        .map_err(ExecError::from_error)?;
    if entry.is_dir {
        return Err(ExecError::Failed(Error::unsupported(
            "目录跨盘已迁移到传输计划层：请先用 wp_transfer::plan::plan_cross_copy 展开为文件任务",
        )));
    }
    // 目标目录先行创建：目标只读/空间超限时立刻报可读错误，避免白下载源文件。
    dst.ensure_dir(&ctx.dst_dir)
        .await
        .map_err(|e| ExecError::from_error(readable_write_error(e)))?;
    // 上次失败保留了完整中转文件时，跳过重复下载。
    let already = total > 0
        && std::fs::metadata(&ctx.local)
            .map(|m| m.is_file() && m.len() == total)
            .unwrap_or(false);
    if already {
        inner.set_half_progress(&ctx.id, generation, total);
    } else {
        let opts = DownloadOptions {
            save_as: Some(ctx.local.clone()),
            segments: ctx.download.segments,
            segment_threshold: ctx.download.segment_threshold,
            resume: ctx.download.resume,
            limit_bps: ctx.transfer.max_speed_bps,
        };
        let progress = make_progress(
            inner,
            &ctx.id,
            generation,
            &ctx.control,
            ProgressMode::CrossDownload { total },
        );
        src.download(&ctx.src_path, &ctx.local, &opts, progress)
            .await
            .map_err(ExecError::from_error)?;
    }
    if ctx.control.is_stopped() {
        return Err(ExecError::Canceled);
    }
    let target = rename_to
        .map(|s| s.to_string())
        .unwrap_or_else(|| ctx.file_name.clone());
    let opts = UploadOptions {
        rename_to: Some(target),
        chunk_size: 0,
        concurrency: 0,
        skip_guard: true,
    };
    let progress = make_progress(
        inner,
        &ctx.id,
        generation,
        &ctx.control,
        ProgressMode::CrossUpload { total },
    );
    let uploaded = dst
        .upload(&ctx.local, &ctx.dst_dir, &opts, progress)
        .await
        .map_err(|e| ExecError::from_error(readable_write_error(e)))?;
    Ok(ExecOutcome {
        remote_name: if uploaded.name.is_empty() {
            None
        } else {
            Some(uploaded.name)
        },
        total: Some(uploaded.size.max(total)),
        done: uploaded.size,
    })
}

fn make_progress(
    inner: &Arc<Inner>,
    id: &str,
    generation: u64,
    control: &Arc<TaskControl>,
    mode: ProgressMode,
) -> ProgressFn {
    let inner = inner.clone();
    let id = id.to_string();
    let control = control.clone();
    Box::new(move |done, total| {
        if control.is_stopped() {
            return false;
        }
        inner.update_progress(&id, generation, done, total, mode)
    })
}

// ───────────────────────── 小工具 ─────────────────────────

fn stop_state(control: &TaskControl) -> Option<TaskState> {
    if control.is_canceled() {
        Some(TaskState::Canceled)
    } else if control.is_paused() {
        Some(TaskState::Paused)
    } else {
        None
    }
}

fn can_run_parts(kind: TaskKind, has_src: bool, has_dst: bool) -> bool {
    match kind {
        TaskKind::Download => has_src,
        TaskKind::Upload => has_dst,
        TaskKind::CrossCopy => has_src && has_dst,
    }
}

fn can_run(entry: &TaskEntry) -> bool {
    can_run_parts(entry.task.kind, entry.src.is_some(), entry.dst.is_some())
}

/// 按网盘类型的安全默认实例并发（目录展开成 N 个任务后防止打爆单盘限流）。
fn default_instance_concurrency(kind: DriveKind) -> usize {
    match kind {
        DriveKind::Guangya => 2,
        DriveKind::Baidu => 4,
        DriveKind::Quark => 4,
        DriveKind::Local | DriveKind::Fake => 16,
    }
}

/// 某实例的有效并发上限：配置覆盖 > 网盘类型默认；夹在 1..=64。
fn effective_instance_limit(transfer: &TransferConfig, instance: &str, kind: DriveKind) -> usize {
    transfer
        .instance_concurrency
        .get(instance)
        .copied()
        .filter(|v| *v > 0)
        .unwrap_or_else(|| default_instance_concurrency(kind))
        .clamp(1, 64)
}

/// 是否是「被限流/操作过于频繁」类错误（需要降并发 + 长冷却，而不是普通快速重试）。
fn is_rate_limited(error: &Error) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    text.contains("127")
        || text.contains("过于频繁")
        || text.contains("频繁")
        || text.contains("too frequent")
        || text.contains("limiter")
        || text.contains("rate limit")
}

/// 该任务的实例限流键与上限（跨盘同时限源+目标，按 id 排序避免死锁）。
fn instance_limits(ctx: &RunCtx) -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = Vec::new();
    match ctx.kind {
        TaskKind::Download => {
            if !ctx.src_instance.is_empty() {
                out.push((
                    ctx.src_instance.clone(),
                    effective_instance_limit(&ctx.transfer, &ctx.src_instance, ctx.src_kind),
                ));
            }
        }
        TaskKind::Upload => {
            if !ctx.instance.is_empty() {
                out.push((
                    ctx.instance.clone(),
                    effective_instance_limit(&ctx.transfer, &ctx.instance, ctx.dst_kind),
                ));
            }
        }
        TaskKind::CrossCopy => {
            if !ctx.src_instance.is_empty() {
                out.push((
                    ctx.src_instance.clone(),
                    effective_instance_limit(&ctx.transfer, &ctx.src_instance, ctx.src_kind),
                ));
            }
            if !ctx.instance.is_empty() && ctx.instance != ctx.src_instance {
                out.push((
                    ctx.instance.clone(),
                    effective_instance_limit(&ctx.transfer, &ctx.instance, ctx.dst_kind),
                ));
            }
        }
    }
    out.sort();
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

/// 目标不可写（只读/空间超限）时转成可读的认证类错误：不重试并提示重新登录。
fn readable_write_error(e: Error) -> Error {
    let text = e.to_string();
    if text.contains("只读")
        || text.contains("-6")
        || text.contains("空间")
        || text.contains("未登录")
    {
        Error::Auth(format!(
            "目标账号只读或空间超限，无法写入（{text}）；请重新登录该网盘或清理/扩容空间"
        ))
    } else {
        e
    }
}

fn backoff_ms(base: u64, retry_index: u32) -> u64 {
    let factor = 1u64 << retry_index.min(20);
    base.saturating_mul(factor).min(MAX_BACKOFF_MS)
}

async fn sleep_with_control(control: &TaskControl, ms: u64) -> Option<TaskState> {
    let deadline = Instant::now() + Duration::from_millis(ms);
    loop {
        if let Some(state) = stop_state(control) {
            return Some(state);
        }
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        let remaining = deadline.saturating_duration_since(now);
        tokio::select! {
            _ = tokio::time::sleep(remaining) => return None,
            _ = control.notify.notified() => {
                // 暂停/取消/继续：回到顶部重新检查。
                continue;
            }
        }
    }
}

fn update_meter(entry: &mut TaskEntry, logical_done: u64) {
    let now = Instant::now();
    let dt = now.duration_since(entry.last_tick).as_secs_f64();
    if dt < 0.05 {
        return;
    }
    let delta = logical_done.saturating_sub(entry.last_done) as f64;
    let instant = delta / dt;
    entry.task.speed = if entry.task.speed <= 0.0 {
        instant
    } else {
        entry.task.speed * 0.7 + instant * 0.3
    };
    entry.last_tick = now;
    entry.last_done = logical_done;
}

fn append_note(old: &str, extra: &str) -> String {
    if old.trim().is_empty() {
        extra.to_string()
    } else {
        format!("{old}；{extra}")
    }
}

fn remove_temp(path: &Path) {
    if path.as_os_str().is_empty() {
        return;
    }
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            wp_core::log_warn!("传输", "删除中转文件失败（{}）：{}", path.display(), e);
        }
    }
}

/// 生成不冲突的下载路径（同时避开文件系统和本轮已排队任务占用的路径）。
fn unique_destination(dir: &Path, name: &str, reserved: &[PathBuf]) -> PathBuf {
    let name = wp_core::util::sanitize_file_name(name);
    let occupied = |p: &Path| p.exists() || reserved.iter().any(|r| r == p);
    let first = dir.join(&name);
    if !occupied(&first) {
        return first;
    }
    for i in 1..10_000usize {
        let candidate = dir.join(with_suffix(&name, i));
        if !occupied(&candidate) {
            return candidate;
        }
    }
    dir.join(format!("{}-{}", wp_core::util::gen_task_id(), name))
}

fn with_suffix(name: &str, index: usize) -> String {
    let path = Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    match path.extension().and_then(|s| s.to_str()) {
        Some(ext) if !ext.is_empty() => format!("{stem} ({index}).{ext}"),
        _ => format!("{stem} ({index})"),
    }
}

#[cfg(test)]
mod dir_tests {
    use super::*;

    fn temp_tree(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("wp-engine-dir-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub1").join("sub2")).unwrap();
        std::fs::create_dir_all(root.join("empty_dir")).unwrap();
        std::fs::write(root.join("a.txt"), b"hello").unwrap();
        std::fs::write(root.join("sub1").join("b.txt"), vec![1u8; 100]).unwrap();
        std::fs::write(root.join("sub1").join("sub2").join("c.bin"), vec![2u8; 200]).unwrap();
        root
    }

    #[test]
    fn rate_limit_and_instance_limits() {
        assert!(is_rate_limited(&Error::server(127, "操作过于频繁")));
        assert!(is_rate_limited(&Error::server(
            429,
            "too frequent requests"
        )));
        assert!(!is_rate_limited(&Error::NotFound("文件不存在".into())));
        assert_eq!(default_instance_concurrency(DriveKind::Guangya), 2);
        assert_eq!(default_instance_concurrency(DriveKind::Baidu), 4);
        assert_eq!(default_instance_concurrency(DriveKind::Quark), 4);
        assert_eq!(default_instance_concurrency(DriveKind::Local), 16);
        let mut cfg = TransferConfig::default();
        cfg.instance_concurrency.insert("guangya".to_string(), 1);
        assert_eq!(
            effective_instance_limit(&cfg, "guangya", DriveKind::Guangya),
            1
        );
        // 缺省回落到网盘类型安全默认
        assert_eq!(effective_instance_limit(&cfg, "baidu", DriveKind::Baidu), 4);
    }

    #[test]
    fn walk_local_tree_collects_nested_files_and_empty_dirs() {
        let root = temp_tree("walk");
        let tree = match walk_local_tree(&root) {
            Ok(t) => t,
            Err(_) => panic!("walk_local_tree 失败"),
        };
        let files: Vec<(String, u64)> = tree
            .files
            .iter()
            .map(|(rel, _, size)| (rel.clone(), *size))
            .collect();
        assert_eq!(
            files,
            vec![
                ("a.txt".to_string(), 5),
                ("sub1/b.txt".to_string(), 100),
                ("sub1/sub2/c.bin".to_string(), 200),
            ]
        );
        assert_eq!(
            tree.dirs,
            vec![
                "empty_dir".to_string(),
                "sub1".to_string(),
                "sub1/sub2".to_string()
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn walk_local_tree_missing_dir_is_error() {
        let missing = std::env::temp_dir().join("wp-engine-dir-missing-xyz");
        let _ = std::fs::remove_dir_all(&missing);
        assert!(walk_local_tree(&missing).is_err());
    }

    #[test]
    fn scan_local_dir_returns_relative_tree() {
        use crate::scan_local_dir;
        let root = temp_tree("scan-api");
        let scan = scan_local_dir(&root).expect("扫描本地目录");
        let files: Vec<(String, u64)> = scan
            .files
            .iter()
            .map(|(rel, _, size)| (rel.clone(), *size))
            .collect();
        assert_eq!(
            files,
            vec![
                ("a.txt".to_string(), 5),
                ("sub1/b.txt".to_string(), 100),
                ("sub1/sub2/c.bin".to_string(), 200),
            ]
        );
        assert!(scan.dirs.iter().any(|d| d == "empty_dir"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn scan_remote_dir_via_fake_drive() {
        let base = std::env::temp_dir().join(format!("wp-scan-remote-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let fake: std::sync::Arc<dyn Drive> = wp_drive::fake::create(wp_drive::DriveInit {
            id: "demo".into(),
            name: "演示网盘".into(),
            data_dir: base.join("data"),
            threads: 0,
            verbose: false,
        })
        .expect("fake drive");
        let scan = crate::scan_remote_dir(&fake, "/")
            .await
            .expect("扫描演示网盘");
        assert!(!scan.files.is_empty(), "演示网盘根目录应有文件");
        assert!(
            scan.files.iter().any(|(rel, _)| rel.contains('/')),
            "应有嵌套文件：{:?}",
            scan.files
                .iter()
                .map(|(r, _)| r.clone())
                .collect::<Vec<_>>()
        );
        // 再扫一个具体目录：/影视 下应有文件
        let scan = crate::scan_remote_dir(&fake, "/影视")
            .await
            .expect("扫描 /影视");
        assert!(!scan.files.is_empty(), "/影视 下应有文件");
        let _ = std::fs::remove_dir_all(&base);
    }
}
