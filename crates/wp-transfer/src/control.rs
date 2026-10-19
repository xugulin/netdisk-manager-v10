//! 任务控制位与并发闸门。
//!
//! * [`TaskControl`]：每任务一个，原子暂停/取消位 + `Notify`（唤醒信号量等待、
//!   重试退避等待）；
//! * [`GatePool`]：小文件 / 大文件两套 `tokio::sync::Semaphore`，并在此实现
//!   「失败降并发、稳定回升」的简单自适应。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use wp_core::config::TransferConfig;

/// 并发许可上限：防止配置异常（如 usize::MAX）导致 tokio 信号量构造 panic。
const MAX_CONCURRENCY: usize = 64;

fn clamp_limit(value: usize) -> usize {
    value.clamp(1, MAX_CONCURRENCY)
}

/// 每任务控制位。
pub(crate) struct TaskControl {
    paused: AtomicBool,
    canceled: AtomicBool,
    /// 暂停/继续/取消时唤醒等待者（信号量排队、重试退避）。
    pub(crate) notify: Notify,
}

impl TaskControl {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            paused: AtomicBool::new(false),
            canceled: AtomicBool::new(false),
            notify: Notify::new(),
        })
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub(crate) fn is_canceled(&self) -> bool {
        self.canceled.load(Ordering::SeqCst)
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.is_paused() || self.is_canceled()
    }

    /// 请求暂停（不清取消位）。
    pub(crate) fn request_pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// 清除暂停位（取消位不动）。
    pub(crate) fn request_resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// 请求取消（同时清暂停位，确保等待者会被唤醒后按取消处理）。
    pub(crate) fn request_cancel(&self) {
        self.canceled.store(true, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

/// 并发闸门池：小文件 / 大文件各一套信号量，另有按网盘实例的信号量。
pub(crate) struct GatePool {
    threshold: u64,
    small: Gate,
    large: Gate,
    /// 网盘实例级闸门（实例 id → Gate）：避免目录展开成 N 个任务后打爆单盘限流。
    instances: std::collections::HashMap<String, Gate>,
}

struct Gate {
    /// 信号量按「当前许可数」创建；自适应增减时同步维护 `limit`。
    sem: Arc<Semaphore>,
    /// 当前逻辑许可数（≤ max）。
    limit: usize,
    /// 配置上限，自适应回升不超过它。
    max: usize,
    /// 待收回的许可（失败降并发时置位，在任务结束归还许可时 `forget` 掉）。
    pending_reduce: usize,
    /// 连续成功计数（每 4 次成功尝试回升 1 个许可）。
    streak: usize,
}

impl Gate {
    fn new(limit: usize) -> Self {
        let max = clamp_limit(limit);
        Self {
            sem: Arc::new(Semaphore::new(max)),
            limit: max,
            max,
            pending_reduce: 0,
            streak: 0,
        }
    }

    fn note_success(&mut self, adaptive: bool) {
        if !adaptive {
            return;
        }
        self.streak = self.streak.saturating_add(1);
        if self.streak >= 4 && self.limit < self.max && self.pending_reduce == 0 {
            self.sem.add_permits(1);
            self.limit += 1;
            self.streak = 0;
        }
    }

    fn note_failure(&mut self, adaptive: bool) {
        self.streak = 0;
        if !adaptive || self.limit <= 1 {
            return;
        }
        // 最多降到 1 个许可；多余请求合并。
        let room = self.limit.saturating_sub(1);
        self.pending_reduce = self.pending_reduce.saturating_add(1).min(room);
    }

    /// 配置变更：在原信号量上调整许可，保证在跑任务的归还仍然有效。
    fn update(&mut self, max: usize) {
        let max = clamp_limit(max);
        self.max = max;
        if max > self.limit {
            let extra = max - self.limit;
            self.sem.add_permits(extra);
            self.limit = max;
            self.pending_reduce = 0;
            self.streak = 0;
        } else if max < self.limit {
            self.pending_reduce = self.limit - max;
            self.streak = 0;
        } else {
            self.pending_reduce = 0;
        }
    }

    fn release(&mut self, permit: OwnedSemaphorePermit) {
        if self.pending_reduce > 0 && self.limit > 1 {
            // forget 后这个许可永久消失，等效把并发上限降 1。
            permit.forget();
            self.limit = self.limit.saturating_sub(1);
            self.pending_reduce = self.pending_reduce.saturating_sub(1);
        }
        // 未 forget 时 permit 在此 drop，正常归还。
    }
}

/// 一次已获取的并发许可。
pub(crate) struct GatePermit {
    pub(crate) small: bool,
    pub(crate) permit: OwnedSemaphorePermit,
}

/// 归还许可时的结果分类（决定自适应升降）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateOutcome {
    Success,
    Failure,
    /// 暂停/取消/代际过期：不影响自适应。
    Neutral,
}

impl GatePool {
    pub(crate) fn new(cfg: &TransferConfig) -> Self {
        Self {
            threshold: cfg.small_file_threshold,
            small: Gate::new(cfg.small_file_concurrency),
            large: Gate::new(cfg.large_file_concurrency),
            instances: std::collections::HashMap::new(),
        }
    }

    /// 是否走小文件闸门。0 字节（或未知）按小文件处理；阈值 0 视为关闭小文件通道。
    pub(crate) fn is_small(&self, total: u64) -> bool {
        self.threshold > 0 && total < self.threshold
    }

    pub(crate) fn sem(&self, small: bool) -> Arc<Semaphore> {
        if small {
            self.small.sem.clone()
        } else {
            self.large.sem.clone()
        }
    }

    pub(crate) fn update(&mut self, cfg: &TransferConfig) {
        self.threshold = cfg.small_file_threshold;
        self.small.update(cfg.small_file_concurrency);
        self.large.update(cfg.large_file_concurrency);
    }

    pub(crate) fn note_success(&mut self, small: bool, adaptive: bool) {
        if small {
            self.small.note_success(adaptive);
        } else {
            self.large.note_success(adaptive);
        }
    }

    pub(crate) fn note_failure(&mut self, small: bool, adaptive: bool) {
        if small {
            self.small.note_failure(adaptive);
        } else {
            self.large.note_failure(adaptive);
        }
    }

    pub(crate) fn release(&mut self, small: bool, permit: OwnedSemaphorePermit) {
        if small {
            self.small.release(permit);
        } else {
            self.large.release(permit);
        }
    }

    // ── 网盘实例级闸门（限流根治疗）─────────────

    /// 取某实例的信号量；首次出现时按 `limit` 建 Gate（≤64）。
    pub(crate) fn instance_sem(&mut self, key: &str, limit: usize) -> Arc<Semaphore> {
        self.instances
            .entry(key.to_string())
            .or_insert_with(|| Gate::new(limit))
            .sem
            .clone()
    }

    pub(crate) fn note_success_instance(&mut self, key: &str, adaptive: bool) {
        if let Some(g) = self.instances.get_mut(key) {
            g.note_success(adaptive);
        }
    }

    pub(crate) fn note_failure_instance(&mut self, key: &str, adaptive: bool) {
        if let Some(g) = self.instances.get_mut(key) {
            g.note_failure(adaptive);
        }
    }

    pub(crate) fn release_instance(&mut self, key: &str, permit: OwnedSemaphorePermit) {
        if let Some(g) = self.instances.get_mut(key) {
            g.release(permit);
        } else {
            drop(permit);
        }
    }
}

/// 一次已获取的实例级许可（记录 key 以便归还到正确闸门）。
pub(crate) struct InstancePermit {
    pub(crate) key: String,
    pub(crate) permit: OwnedSemaphorePermit,
}

/// 一次任务执行持有的全部许可：全局小/大闸门 + 0..N 个实例闸门。
pub(crate) struct RunPermits {
    pub(crate) gate: GatePermit,
    pub(crate) instances: Vec<InstancePermit>,
}

#[cfg(test)]
mod instance_gate_tests {
    use super::*;

    #[test]
    fn instance_gate_reduces_on_failure_and_recovers() {
        let cfg = TransferConfig::default();
        let mut pool = GatePool::new(&cfg);
        let key = "guangya";
        let sem = pool.instance_sem(key, 2);
        assert_eq!(sem.available_permits(), 2);

        // 失败降并发：释放两个许可后 pending_reduce 生效，有效上限降到 1
        let p1 = sem.clone().try_acquire_owned().unwrap();
        let p2 = sem.clone().try_acquire_owned().unwrap();
        pool.note_failure_instance(key, true);
        pool.release_instance(key, p1);
        pool.release_instance(key, p2);
        assert_eq!(sem.available_permits(), 1, "失败后实例上限应降到 1");

        // 连续成功 4 次回升 1 个许可
        for _ in 0..4 {
            let p = sem.clone().try_acquire_owned().unwrap();
            pool.note_success_instance(key, true);
            pool.release_instance(key, p);
        }
        assert_eq!(sem.available_permits(), 2, "稳定成功后应回升到 2");
    }

    #[test]
    fn instance_gate_isolated_per_instance() {
        let cfg = TransferConfig::default();
        let mut pool = GatePool::new(&cfg);
        let g = pool.instance_sem("guangya", 2);
        let q = pool.instance_sem("quark", 4);
        assert_eq!(g.available_permits(), 2);
        assert_eq!(q.available_permits(), 4);
        let p = g.clone().try_acquire_owned().unwrap();
        pool.note_failure_instance("guangya", true);
        pool.release_instance("guangya", p);
        assert_eq!(g.available_permits(), 1, "光鸭降并发不影响夸克");
        assert_eq!(q.available_permits(), 4);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(small: usize, large: usize) -> TransferConfig {
        TransferConfig {
            small_file_concurrency: small,
            large_file_concurrency: large,
            ..TransferConfig::default()
        }
    }

    #[test]
    fn adaptive_gate_reduces_on_failure_and_recovers() {
        let mut pool = GatePool::new(&config(3, 1));
        assert!(pool.is_small(1024));
        assert!(!pool.is_small(8 * 1024 * 1024));

        let sem = pool.sem(true);
        let p1 = sem.clone().try_acquire_owned().expect("许可1");
        let p2 = sem.clone().try_acquire_owned().expect("许可2");
        let p3 = sem.clone().try_acquire_owned().expect("许可3");
        assert_eq!(sem.available_permits(), 0);

        // 一次失败 → 归还时收回一个许可（forget），有效并发 3 → 2。
        pool.note_failure(true, true);
        pool.release(true, p1);
        assert_eq!(pool.small.limit, 2);
        drop(p2);
        drop(p3);
        assert_eq!(sem.available_permits(), 2);

        // 连续 4 次成功 → 回升到配置上限 3。
        for _ in 0..4 {
            pool.note_success(true, true);
        }
        assert_eq!(pool.small.limit, 3);
        assert_eq!(sem.available_permits(), 3);
    }

    #[test]
    fn gate_update_keeps_semaphore_valid() {
        let mut pool = GatePool::new(&config(4, 2));
        let sem = pool.sem(false);
        let permit = sem.clone().try_acquire_owned().expect("许可");
        pool.update(&config(1, 1));
        assert_eq!(pool.large.max, 1);
        // 降并发在许可归还时生效，不会误伤别的信号量。
        pool.release(false, permit);
        assert_eq!(pool.large.limit, 1);
        pool.update(&config(2, 2));
        assert_eq!(pool.large.max, 2);
        assert_eq!(sem.available_permits(), 2);
    }

    #[test]
    fn limits_are_clamped() {
        let pool = GatePool::new(&config(usize::MAX, 0));
        assert_eq!(pool.small.limit, MAX_CONCURRENCY);
        assert_eq!(pool.large.limit, 1);
    }

    #[test]
    fn control_flags_are_independent() {
        let control = TaskControl::new();
        assert!(!control.is_paused());
        control.request_pause();
        assert!(control.is_paused() && !control.is_canceled());
        control.request_resume();
        assert!(!control.is_stopped());
        control.request_cancel();
        assert!(control.is_canceled() && !control.is_paused());
    }
}
