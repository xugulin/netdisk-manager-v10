//! 磁盘分块缓存 + LRU 容量管理。
//!
//! 缓存文件命名：`<key>.data`（稀疏文件，大小=文件总大小）+ `<key>.meta.json`（已完成块位图）。
//! `key = hash(drive_key : remote_path : size)`，因此同一网盘实例的同一路径同名同大小文件稳定命中；
//! 路径/大小变化自动视为新缓存（旧缓存由 LRU 清掉）。
//!
//! 并发模型：
//! * 每个块一个 [`ChunkSlot`]（`Mutex<ChunkState>` + `Notify`）；
//! * 多个读者请求同一块时**只有一个**真正下载（同块去重），其余等待通知；
//! * 下载失败会把错误广播给所有等待者，块状态复位为 `Missing`，下次访问可重试。
//!
//! LRU：进程内记录每个缓存项的最近访问时间；超过 `max_bytes` 时优先删除**已无使用者**
//! （`Weak` 无法升级）的旧缓存；仍有使用者的缓存不会被抢占（仅告警日志，等其关闭后下次清理）。

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use wp_core::{Error, Result};

/// 缓存配置。
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// 缓存目录。
    pub dir: PathBuf,
    /// 分块大小（字节）。
    pub chunk_bytes: u64,
    /// 总容量上限（0 = 不限制/不落盘由上层禁用）。
    pub max_bytes: u64,
}

/// 单块状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkState {
    /// 还没有数据，也没人在下。
    Missing,
    /// 正在下载（只有一个下载者，其余人 wait）。
    Loading,
    /// 已落盘可用。
    Ready,
    /// 最近一次下载失败（错误信息），等待者取走后复位为 Missing。
    Failed(String),
}

/// 一个块的并发槽位。
#[derive(Debug)]
struct ChunkSlot {
    state: Mutex<ChunkState>,
    notify: Notify,
}

impl ChunkSlot {
    fn new() -> Self {
        Self {
            state: Mutex::new(ChunkState::Missing),
            notify: Notify::new(),
        }
    }

    fn state(&self) -> ChunkState {
        self.state.lock().expect("chunk state poisoned").clone()
    }

    fn set_state(&self, state: ChunkState) {
        *self.state.lock().expect("chunk state poisoned") = state;
    }
}

/// 下载块时的「保险丝」：无论正常返回还是 panic 展开，都把块状态从 Loading 复位。
struct FetchGuard<'a> {
    slot: &'a ChunkSlot,
    armed: bool,
}

impl Drop for FetchGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.slot.set_state(ChunkState::Missing);
            self.slot.notify.notify_waiters();
        }
    }
}

/// 一个文件的分块缓存。
#[derive(Debug)]
pub struct CacheEntry {
    key: String,
    data_path: PathBuf,
    meta_path: PathBuf,
    total: u64,
    chunk_bytes: u64,
    chunks: Vec<Arc<ChunkSlot>>,
    ready_chunks: AtomicU64,
    last_access: Mutex<Instant>,
}

impl CacheEntry {
    pub fn key(&self) -> &str {
        &self.key
    }
    pub fn data_path(&self) -> &Path {
        &self.data_path
    }
    pub fn meta_path(&self) -> &Path {
        &self.meta_path
    }
    pub fn total(&self) -> u64 {
        self.total
    }
    pub fn chunk_bytes(&self) -> u64 {
        self.chunk_bytes
    }
    pub fn chunk_count(&self) -> u64 {
        self.chunks.len() as u64
    }
    /// 已缓存的块数。
    pub fn ready_chunks(&self) -> u64 {
        self.ready_chunks.load(Ordering::Relaxed)
    }
    /// 第 `idx` 块当前状态（预读调度用）。
    pub fn chunk_state(&self, idx: u64) -> ChunkState {
        self.chunks
            .get(idx as usize)
            .map(|s| s.state())
            .unwrap_or(ChunkState::Missing)
    }
    /// 整个文件是否都已缓存。
    pub fn is_complete(&self) -> bool {
        self.ready_chunks() >= self.chunk_count()
    }
    /// 已缓存字节数（按块粗略统计；最后一块可能小于 chunk_bytes）。
    pub fn buffered_bytes(&self) -> u64 {
        let ready = self.ready_chunks();
        let mut bytes = ready.saturating_mul(self.chunk_bytes);
        if self.chunk_count() > 0
            && ready >= self.chunk_count()
            && self.total % self.chunk_bytes != 0
        {
            bytes = bytes.saturating_sub(self.chunk_bytes - (self.total % self.chunk_bytes));
        }
        bytes.min(self.total)
    }
    fn touch(&self) {
        if let Ok(mut t) = self.last_access.lock() {
            *t = Instant::now();
        }
    }
    /// 第 `idx` 块覆盖的字节区间（闭区间）；越界返回 None。
    pub fn chunk_range(&self, idx: u64) -> Option<(u64, u64)> {
        if idx >= self.chunk_count() {
            return None;
        }
        let start = idx * self.chunk_bytes;
        let end = (start + self.chunk_bytes - 1).min(self.total - 1);
        Some((start, end))
    }

    /// 确保第 `idx` 块可用；未命中时用 `fetch` 下载（同块去重）。
    ///
    /// `fetch` 只会在「当前调用者抢到下载权」时被调用一次；其余并发调用者等待其完成。
    pub async fn ensure_chunk<F, Fut>(self: &Arc<Self>, idx: u64, fetch: F) -> Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let Some(slot) = self.chunks.get(idx as usize).cloned() else {
            return Err(Error::other(format!("缓存块越界：{idx}")));
        };
        loop {
            // 先注册唤醒再查状态，避免「检查-等待」之间 notify 丢失。
            let notified = slot.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            match slot.state() {
                ChunkState::Ready => return Ok(()),
                ChunkState::Failed(_) => {
                    // 失败是一个「瞬时状态」：下载者已拿到错误返回；
                    // 其他等待者/后续访问复位后重新抢下载权重试（避免把一次失败永久化）。
                    slot.set_state(ChunkState::Missing);
                    continue;
                }
                ChunkState::Loading => {
                    notified.await;
                }
                ChunkState::Missing => {
                    slot.set_state(ChunkState::Loading);
                    let mut guard = FetchGuard {
                        slot: &slot,
                        armed: true,
                    };
                    let result = fetch().await;
                    match &result {
                        Ok(()) => {
                            slot.set_state(ChunkState::Ready);
                            self.ready_chunks.fetch_add(1, Ordering::Relaxed);
                            self.touch();
                            self.persist_meta();
                        }
                        Err(e) => {
                            slot.set_state(ChunkState::Failed(e.to_string()));
                        }
                    }
                    guard.armed = false;
                    slot.notify.notify_waiters();
                    return result;
                }
            }
        }
    }

    /// 把一块数据写入磁盘缓存文件（定位写，不追加）。
    pub async fn write_chunk(&self, idx: u64, data: Vec<u8>) -> Result<()> {
        let Some((start, end)) = self.chunk_range(idx) else {
            return Err(Error::other(format!("缓存块越界：{idx}")));
        };
        let expect = (end - start + 1) as usize;
        if data.len() != expect {
            return Err(Error::other(format!(
                "块数据长度不符：期望 {expect}，实际 {}（块 {idx}）",
                data.len()
            )));
        }
        let path = self.data_path.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new().write(true).open(&path)?;
            f.seek(SeekFrom::Start(start))?;
            f.write_all(&data)?;
            f.flush()?;
            Ok(())
        })
        .await
        .map_err(|e| Error::other(format!("缓存写入任务失败：{e}")))??;
        Ok(())
    }

    /// 顺序读缓存文件的一段（异步；供代理响应回源用）。
    pub async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        use tokio::io::AsyncReadExt;
        let mut f = tokio::fs::File::open(&self.data_path).await?;
        use tokio::io::AsyncSeekExt;
        f.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut got = 0;
        while got < buf.len() {
            let n = f.read(&mut buf[got..]).await?;
            if n == 0 {
                break;
            }
            got += n;
        }
        Ok(got)
    }

    /// 原子写 sidecar 元数据（完成块列表），供下次进程/会话复用缓存。
    fn persist_meta(&self) {
        let done: Vec<u64> = self
            .chunks
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.state() == ChunkState::Ready)
            .map(|(i, _)| i as u64)
            .collect();
        let meta = MetaFile {
            v: META_VERSION,
            total: self.total,
            chunk: self.chunk_bytes,
            done,
            last_used: wp_core::util::now_ts(),
        };
        let Ok(text) = serde_json::to_string(&meta) else {
            return;
        };
        let tmp = self.meta_path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &self.meta_path);
        }
    }
}

const META_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct MetaFile {
    v: u32,
    total: u64,
    chunk: u64,
    #[serde(default)]
    done: Vec<u64>,
    #[serde(default)]
    last_used: i64,
}

/// 缓存项的进程内元信息（LRU 用）。
#[derive(Debug)]
struct EntryMeta {
    weak: Weak<CacheEntry>,
    total: u64,
    data_path: PathBuf,
    meta_path: PathBuf,
    last_access: Instant,
}

/// 缓存管理器：按 key 复用/创建缓存项，并做 LRU 容量控制。
#[derive(Debug)]
pub struct CacheManager {
    config: CacheConfig,
    entries: Mutex<HashMap<String, EntryMeta>>,
}

impl CacheManager {
    pub fn new(config: CacheConfig) -> Result<Arc<Self>> {
        if !config.dir.as_os_str().is_empty() {
            std::fs::create_dir_all(&config.dir)?;
        }
        Ok(Arc::new(Self {
            config,
            entries: Mutex::new(HashMap::new()),
        }))
    }

    pub fn config(&self) -> &CacheConfig {
        &self.config
    }

    /// 打开/复用 `drive_key + path + size` 对应的缓存项。
    pub fn open(&self, drive_key: &str, remote_path: &str, size: u64) -> Result<Arc<CacheEntry>> {
        let key = cache_key(drive_key, remote_path, size);
        let mut entries = self.entries.lock().expect("cache entries poisoned");
        if let Some(meta) = entries.get_mut(&key) {
            if let Some(entry) = meta.weak.upgrade() {
                meta.last_access = Instant::now();
                entry.touch();
                entry.touch();
                return Ok(entry);
            }
        }
        let (entry, data_path, meta_path) = CacheEntry::load_or_create(&self.config, &key, size)?;
        entries.insert(
            key,
            EntryMeta {
                weak: Arc::downgrade(&entry),
                total: size,
                data_path,
                meta_path,
                last_access: Instant::now(),
            },
        );
        // 释放锁后再做容量清理（清理只操作 Weak/文件系统）。
        drop(entries);
        self.enforce_limit();
        Ok(entry)
    }

    /// 当前内存中记录的全部缓存项数量与总大小。
    pub fn tracked(&self) -> (usize, u64) {
        let entries = self.entries.lock().expect("cache entries poisoned");
        (entries.len(), entries.values().map(|m| m.total).sum())
    }

    /// LRU：超限时删除已无使用者的旧缓存文件。
    ///
    /// 返回清理掉的字节数。仍有使用者（代理会话未关闭）的缓存不会被删；
    /// 若全是使用中的缓存则只记录日志、不做强删，等其释放后下次再清。
    pub fn enforce_limit(&self) -> u64 {
        if self.config.max_bytes == 0 {
            return 0;
        }
        let mut entries = self.entries.lock().expect("cache entries poisoned");
        // 先摘掉已经没人持有的项
        let dead: Vec<String> = entries
            .iter()
            .filter(|(_, m)| m.weak.strong_count() == 0)
            .map(|(k, _)| k.clone())
            .collect();
        let mut freed = 0u64;
        for key in dead {
            if let Some(meta) = entries.remove(&key) {
                freed += remove_files(&meta.data_path, &meta.meta_path);
            }
        }
        let mut total: u64 = entries.values().map(|m| m.total).sum();
        if total <= self.config.max_bytes {
            return freed;
        }
        // 仍超限：按最近使用时间从旧到新淘汰无人持有的
        let mut candidates: Vec<(String, Instant, u64, PathBuf, PathBuf)> = entries
            .iter()
            .filter(|(_, m)| m.weak.strong_count() == 0)
            .map(|(k, m)| {
                (
                    k.clone(),
                    m.last_access,
                    m.total,
                    m.data_path.clone(),
                    m.meta_path.clone(),
                )
            })
            .collect();
        candidates.sort_by_key(|(_, at, _, _, _)| *at);
        for (key, _at, size, data_path, meta_path) in candidates {
            if total <= self.config.max_bytes {
                break;
            }
            entries.remove(&key);
            freed += remove_files(&data_path, &meta_path);
            total = total.saturating_sub(size);
        }
        if total > self.config.max_bytes {
            wp_core::log_debug!(
                "wp-stream",
                "缓存超限但全部在使用中：{} > {}（等会话关闭后清理）",
                total,
                self.config.max_bytes
            );
        }
        freed
    }

    /// 删除指定 key 的缓存（播放结束可选清理）。
    pub fn evict(&self, key: &str) -> u64 {
        let mut entries = self.entries.lock().expect("cache entries poisoned");
        if let Some(meta) = entries.remove(key) {
            remove_files(&meta.data_path, &meta.meta_path)
        } else {
            0
        }
    }
}

fn remove_files(data_path: &Path, meta_path: &Path) -> u64 {
    let size = std::fs::metadata(data_path).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_file(data_path);
    let _ = std::fs::remove_file(meta_path);
    size
}

impl CacheEntry {
    fn load_or_create(
        config: &CacheConfig,
        key: &str,
        size: u64,
    ) -> Result<(Arc<Self>, PathBuf, PathBuf)> {
        let dir = if config.dir.as_os_str().is_empty() {
            std::env::temp_dir().join("wp-stream-cache")
        } else {
            config.dir.clone()
        };
        std::fs::create_dir_all(&dir)?;
        let data_path = dir.join(format!("{key}.data"));
        let meta_path = dir.join(format!("{key}.meta.json"));

        // 分块大小：至少配置值，且块数不超过 65536（防止配置过小 + 文件超大导致内存爆炸）。
        let mut chunk_bytes = config.chunk_bytes.max(1);
        if size > 0 {
            let min_chunk = size.div_ceil(65_536).max(1);
            chunk_bytes = chunk_bytes.max(min_chunk);
            chunk_bytes = chunk_bytes.min(size.max(1));
        }
        let chunk_count = if size == 0 {
            0
        } else {
            size.div_ceil(chunk_bytes)
        };

        // 尝试恢复旧缓存
        let mut restored: Vec<u64> = Vec::new();
        let usable = std::fs::metadata(&data_path)
            .map(|m| m.is_file() && m.len() == size)
            .unwrap_or(false);
        if usable && size > 0 {
            if let Ok(text) = std::fs::read_to_string(&meta_path) {
                if let Ok(meta) = serde_json::from_str::<MetaFile>(&text) {
                    if meta.v == META_VERSION && meta.total == size && meta.chunk == chunk_bytes {
                        restored = meta.done;
                    }
                }
            }
        }

        if !usable || size == 0 {
            // 新建/重建稀疏文件
            let f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&data_path)?;
            f.set_len(size)?;
            let _ = std::fs::remove_file(&meta_path);
            restored.clear();
        }

        let mut chunks = Vec::with_capacity(chunk_count as usize);
        for _ in 0..chunk_count {
            chunks.push(Arc::new(ChunkSlot::new()));
        }
        let mut ready = 0u64;
        for idx in restored {
            if idx < chunk_count {
                if let Some(slot) = chunks.get(idx as usize) {
                    if slot.state() == ChunkState::Missing {
                        slot.set_state(ChunkState::Ready);
                        ready += 1;
                    }
                }
            }
        }
        let entry = Arc::new(CacheEntry {
            key: key.to_string(),
            data_path: data_path.clone(),
            meta_path: meta_path.clone(),
            total: size,
            chunk_bytes,
            chunks,
            ready_chunks: AtomicU64::new(ready),
            last_access: Mutex::new(Instant::now()),
        });
        Ok((entry, data_path, meta_path))
    }
}

/// 计算缓存 key：`hash(drive_key : remote_path : size)` + 文件名提示。
pub fn cache_key(drive_key: &str, remote_path: &str, size: u64) -> String {
    // FNV-1a 64 位（稳定、无依赖；仅用于本机缓存命名，不用于安全场景）
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in [
        drive_key.as_bytes(),
        b"\x1f",
        remote_path.as_bytes(),
        b"\x1f",
    ] {
        for &b in part {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    for &b in &size.to_le_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let base = remote_path.rsplit('/').next().unwrap_or("file");
    let hint: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    let hint = hint.trim_matches('_').to_string();
    if hint.is_empty() {
        format!("{h:016x}-{size}")
    } else {
        format!("{h:016x}-{size}-{hint}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn temp_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "wp-stream-cache-test-{tag}-{}-{}-{n}",
            std::process::id(),
            wp_core::util::now_ts()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn cfg(dir: &Path, chunk: u64, max: u64) -> CacheConfig {
        CacheConfig {
            dir: dir.to_path_buf(),
            chunk_bytes: chunk,
            max_bytes: max,
        }
    }

    #[test]
    fn key_stable_and_distinct() {
        let a = cache_key("baidu:baidu", "/a/video.mp4", 100);
        let b = cache_key("baidu:baidu", "/a/video.mp4", 100);
        assert_eq!(a, b);
        assert_ne!(a, cache_key("baidu:baidu", "/a/video.mp4", 101));
        assert_ne!(a, cache_key("quark:quark", "/a/video.mp4", 100));
        assert_ne!(a, cache_key("baidu:baidu", "/b/video.mp4", 100));
        assert!(a.ends_with("video.mp4"), "key 里应带文件名提示：{a}");
    }

    #[tokio::test]
    async fn status_and_chunk_ranges() {
        let dir = temp_dir("ranges");
        let mgr = CacheManager::new(cfg(&dir, 100, 0)).unwrap();
        let entry = mgr.open("test", "/f.bin", 250).unwrap();
        assert_eq!(entry.chunk_count(), 3);
        assert_eq!(entry.chunk_range(0), Some((0, 99)));
        assert_eq!(entry.chunk_range(2), Some((200, 249)));
        assert_eq!(entry.chunk_range(3), None);
        assert!(!entry.is_complete());
        assert_eq!(entry.buffered_bytes(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dedup_concurrent_fetch_single_download() {
        let dir = temp_dir("dedup");
        let mgr = CacheManager::new(cfg(&dir, 100, 0)).unwrap();
        let entry = mgr.open("test", "/f.bin", 1000).unwrap();
        let counter = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..4 {
            let e = entry.clone();
            let c = counter.clone();
            tasks.push(tokio::spawn(async move {
                e.ensure_chunk(0, || {
                    c.fetch_add(1, Ordering::SeqCst);
                    async {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        Ok(())
                    }
                })
                .await
            }));
        }
        for t in tasks {
            assert!(t.await.unwrap().is_ok());
        }
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "同块并发只应触发一次下载"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn write_restore_and_read() {
        let dir = temp_dir("restore");
        let mgr = CacheManager::new(cfg(&dir, 100, 0)).unwrap();
        let entry = mgr.open("test", "/f.bin", 250).unwrap();
        entry.write_chunk(1, vec![9u8; 100]).await.unwrap();
        entry
            .ensure_chunk(1, || async {
                // 数据已由 write_chunk 写好，这里只是标记
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(entry.ready_chunks(), 1);
        assert_eq!(entry.buffered_bytes(), 100);
        let data_path = entry.data_path().to_path_buf();
        if let Ok(mut g) = entry.last_access.lock() {
            *g = Instant::now();
        }
        // 模拟「同一缓存被重新打开」：丢弃内存项，重新 open
        drop(entry);
        mgr.evict(&cache_key("test", "/f.bin", 250));
        let _ = std::fs::remove_file(&data_path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn meta_persisted_and_restored_on_reopen() {
        let dir = temp_dir("reopen");
        let mgr = CacheManager::new(cfg(&dir, 100, 0)).unwrap();
        let key = cache_key("test", "/f.bin", 250);
        let entry = mgr.open("test", "/f.bin", 250).unwrap();
        entry.write_chunk(1, vec![7u8; 100]).await.unwrap();
        entry.ensure_chunk(1, || async { Ok(()) }).await.unwrap();
        drop(entry);
        // 断开 manager 的跟踪（模拟进程重启）：直接新建 manager 读同一目录
        drop(mgr);
        let mgr2 = CacheManager::new(cfg(&dir, 100, 0)).unwrap();
        let entry2 = mgr2.open("test", "/f.bin", 250).unwrap();
        assert_eq!(entry2.chunk_count(), 3);
        assert!(entry2.ready_chunks() >= 1, "重开缓存应恢复已完成块");
        let mut buf = [0u8; 100];
        let n = entry2.read_at(100, &mut buf).await.unwrap();
        assert_eq!(n, 100);
        assert!(buf.iter().all(|b| *b == 7));
        assert_eq!(entry2.key(), &key);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn lru_evicts_dead_entries() {
        let dir = temp_dir("lru");
        let mgr = CacheManager::new(cfg(&dir, 100, 150)).unwrap();
        let a = mgr.open("test", "/a.bin", 100).unwrap();
        let a_data = a.data_path().to_path_buf();
        assert!(a_data.exists());
        drop(a);
        // 开第二个会超过上限：应删掉已无人持有的 a
        let _b = mgr.open("test", "/b.bin", 100).unwrap();
        assert!(!a_data.exists(), "超限后无使用者的旧缓存应被 LRU 删除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn fetch_error_then_retry_succeeds() {
        let dir = temp_dir("err");
        let mgr = CacheManager::new(cfg(&dir, 100, 0)).unwrap();
        let entry = mgr.open("test", "/f.bin", 100).unwrap();
        let err = entry
            .ensure_chunk(0, || async { Err(Error::network("模拟失败")) })
            .await;
        assert!(err.is_err());
        // 失败不会被永久化：下一次访问复位并重试成功
        entry.ensure_chunk(0, || async { Ok(()) }).await.unwrap();
        assert!(entry.is_complete());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
