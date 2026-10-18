//! AI 结果缓存：按（文件指纹 + 模型 + 目标语言 + 任务类型）存 JSON 文件。
//!
//! 设计（架构 §7.1-4：跨次播放零成本）：
//! - 键 [`CacheKey`] → 文件名 `sha256(key_json).json`，目录可配置；
//! - 值任意 `serde_json::Value`（翻译是 `Vec<String>`，后续 ASR/摘要可存结构化对象）；
//! - **原子写**：先写同目录临时文件再 `rename`，避免半截 JSON；
//! - **LRU 上限**：条目数超过 `max_entries` 时按文件 mtime 淘汰最旧；命中时 touch mtime；
//! - 读损坏/非法条目视为未命中（不会让坏缓存阻塞翻译）；
//! - 同步小文件 IO（单条 kB 级，耗时在微秒级），由调用方在异步任务中直接调用。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wp_core::{Error, Result};
use wp_subtitle::SubtitleDoc;

use crate::AiTaskKind;

/// 缓存键：文件指纹 + 模型 + 目标语言 + 任务类型。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CacheKey {
    /// 文件指纹（建议 `大小:mtime` 或内容 sha256，见 [`fingerprint_file_metadata`]）。
    pub file_fingerprint: String,
    /// 模型名（同文件不同模型翻译结果分开缓存）。
    pub model: String,
    /// 目标语言（`zh`/`en`…）。
    pub target_lang: String,
    /// 任务类型（翻译/ASR/摘要…）。
    pub task_kind: AiTaskKind,
}

impl CacheKey {
    /// 构造缓存键。
    pub fn new(
        file_fingerprint: impl Into<String>,
        model: impl Into<String>,
        target_lang: impl Into<String>,
        task_kind: AiTaskKind,
    ) -> Self {
        Self {
            file_fingerprint: file_fingerprint.into(),
            model: model.into(),
            target_lang: target_lang.into(),
            task_kind,
        }
    }

    /// 键的稳定摘要（sha256 hex），作缓存文件名。
    pub fn digest(&self) -> String {
        // 结构体字段顺序固定，serde_json 输出确定；对序列化失败保守用 Debug 兜底。
        let bytes = serde_json::to_vec(self).unwrap_or_else(|_| format!("{self:?}").into_bytes());
        sha256_hex(&bytes)
    }
}

/// JSON 文件缓存（原子写 + LRU 上限）。
#[derive(Debug, Clone)]
pub struct AiCache {
    dir: PathBuf,
    max_entries: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    key: CacheKey,
    value: serde_json::Value,
    created_unix_ms: u64,
}

impl AiCache {
    /// 默认最多缓存 200 条。
    pub const DEFAULT_MAX_ENTRIES: usize = 200;

    /// 用目录构造（目录按需创建）。
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            max_entries: Self::DEFAULT_MAX_ENTRIES,
        }
    }

    /// 设置最多保留条目数（`0` = 每次写入后清空，等价禁用缓存）。
    pub fn with_max_entries(mut self, max_entries: usize) -> Self {
        self.max_entries = max_entries;
        self
    }

    /// 缓存目录。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 最多保留条目数。
    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    fn ensure_dir(&self) -> Result<()> {
        if !self.dir.exists() {
            std::fs::create_dir_all(&self.dir)?;
        }
        Ok(())
    }

    /// 某键对应的缓存文件路径。
    pub fn entry_path(&self, key: &CacheKey) -> PathBuf {
        self.dir.join(format!("{}.json", key.digest()))
    }

    /// 读取原始 JSON 值；不存在/损坏/键不匹配返回 None。
    pub fn get(&self, key: &CacheKey) -> Option<serde_json::Value> {
        let path = self.entry_path(key);
        let data = std::fs::read_to_string(&path).ok()?;
        let entry: CacheEntry = serde_json::from_str(&data).ok()?;
        if entry.key != *key {
            return None;
        }
        touch_mtime(&path);
        Some(entry.value)
    }

    /// 读翻译结果（`Vec<String>`，与 cue 等长）。
    pub fn get_translations(&self, key: &CacheKey) -> Option<Vec<String>> {
        let value = self.get(key)?;
        serde_json::from_value(value).ok()
    }

    /// 写入原始 JSON 值（原子写 + 超限淘汰）。
    pub fn put(&self, key: &CacheKey, value: serde_json::Value) -> Result<()> {
        self.ensure_dir()?;
        let entry = CacheEntry {
            key: key.clone(),
            value,
            created_unix_ms: now_unix_ms(),
        };
        let json = serde_json::to_string_pretty(&entry)?;
        let path = self.entry_path(key);
        atomic_write(&path, json.as_bytes())?;
        self.evict_if_needed()?;
        Ok(())
    }

    /// 写入翻译结果。
    pub fn put_translations(&self, key: &CacheKey, translations: &[String]) -> Result<()> {
        let value = serde_json::to_value(translations)?;
        self.put(key, value)
    }

    /// 读取转写字幕文档（ASR，P2）。
    pub fn get_subtitle_doc(&self, key: &CacheKey) -> Option<SubtitleDoc> {
        let value = self.get(key)?;
        serde_json::from_value(value).ok()
    }

    /// 写入转写字幕文档（ASR，P2）。
    pub fn put_subtitle_doc(&self, key: &CacheKey, doc: &SubtitleDoc) -> Result<()> {
        let value = serde_json::to_value(doc)?;
        self.put(key, value)
    }

    /// 当前缓存条目数（`*.json` 文件数）。
    pub fn entry_count(&self) -> usize {
        list_entry_files(&self.dir).len()
    }

    /// 清空缓存目录下的条目文件（返回删除数）。
    pub fn clear(&self) -> Result<usize> {
        let files = list_entry_files(&self.dir);
        let mut removed = 0;
        for path in files {
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// 条目数超过上限时按 mtime 旧→新淘汰。
    fn evict_if_needed(&self) -> Result<()> {
        let mut files = list_entry_files(&self.dir);
        if files.len() <= self.max_entries {
            return Ok(());
        }
        // (mtime, path)：mtime 缺失的排最前（优先淘汰）。
        files.sort_by_key(|path| {
            std::fs::metadata(path)
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH)
        });
        let excess = files.len() - self.max_entries;
        for path in files.into_iter().take(excess) {
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }
}

/// `*.json` 条目文件列表（忽略临时文件/子目录）。
fn list_entry_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().map(|x| x == "json").unwrap_or(false))
        .collect()
}

/// 原子写：同目录临时文件 + rename。
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".tmp-{}-{}", std::process::id(), now_unix_nanos()));
    std::fs::write(&tmp, bytes)?;
    // Windows 上 rename 不能覆盖已存在文件；缓存非关键，删旧再改名（Linux 上 rename 本身原子）。
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Io(e));
    }
    Ok(())
}

/// 命中时 touch mtime，维持 LRU（失败忽略，只影响淘汰顺序）。
fn touch_mtime(path: &Path) {
    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn now_unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// sha256 hex。
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// 按内容计算文件指纹（小文件；视频请用 [`fingerprint_file_metadata`]）。
pub fn fingerprint_bytes(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

/// 按元数据计算文件指纹（推荐用于大视频：`大小:mtime` 的 sha256，纳秒级 MTime 更稳）。
pub fn fingerprint_file_metadata(size: u64, mtime_ms: i64) -> String {
    sha256_hex(format!("{size}:{mtime_ms}").as_bytes())
}
