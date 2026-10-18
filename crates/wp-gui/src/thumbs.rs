//! 缩略图缓存：异步拉取（`Drive::fetch_thumbnail`）→ 解码 → egui 纹理。
//!
//! 设计要点：
//! * 只对「带 `thumbnail` 字段的文件条目」懒加载，目录/不支持的类型不请求；
//! * 每帧只并发少量请求（`MAX_INFLIGHT`），并且只请求当前视口内的行；
//! * 纹理数量有上限（`MAX_ENTRIES`），超限按插入顺序淘汰；
//! * 拉取失败静默回退到文件类型图标，不打扰用户。

use egui::{ColorImage, Context, TextureHandle, TextureOptions};
use std::collections::HashMap;
use wp_core::model::RemoteEntry;

/// 单个条目的缩略图状态。
pub enum Thumb {
    Loading,
    /// 已拿到原始字节，等待本帧解码成纹理
    Raw(Vec<u8>),
    Ready(TextureHandle),
    Failed,
}

/// 缩略图缓存（按 `drive_id:entry_id` 作键）。
#[derive(Default)]
pub struct ThumbCache {
    map: HashMap<String, Thumb>,
    order: Vec<String>,
    pub inflight: usize,
}

const MAX_ENTRIES: usize = 240;
const MAX_INFLIGHT: usize = 16;
const MAX_TEXTURE_SIDE: u32 = 192;

impl ThumbCache {
    /// 缓存键：同一网盘内按条目 id（缺失时用路径）。
    pub fn key(drive_id: &str, entry: &RemoteEntry) -> String {
        let id = if entry.id.is_empty() {
            &entry.path
        } else {
            &entry.id
        };
        format!("{drive_id}:{id}")
    }

    /// 该条目是否值得请求缩略图（文件 + 有 thumbnail 字段）。
    pub fn is_candidate(entry: &RemoteEntry) -> bool {
        !entry.is_dir
            && entry
                .thumbnail
                .as_deref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
    }

    /// 是否已有缓存（含加载中/失败）。
    pub fn has(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    /// 是否可以再发请求（未缓存 + 并发未满）。
    pub fn need(&self, key: &str) -> bool {
        !self.has(key) && self.inflight < MAX_INFLIGHT
    }

    /// 标记为加载中。
    pub fn mark_loading(&mut self, key: &str) {
        if self.map.contains_key(key) {
            return;
        }
        self.map.insert(key.to_string(), Thumb::Loading);
        self.order.push(key.to_string());
        self.inflight += 1;
        self.evict_if_needed();
    }

    /// 拉取成功：存原始字节，等 `pump` 解码。
    pub fn put_raw(&mut self, key: &str, bytes: Vec<u8>) {
        self.inflight = self.inflight.saturating_sub(1);
        self.map.insert(key.to_string(), Thumb::Raw(bytes));
    }

    /// 拉取失败：记住失败，避免反复请求。
    pub fn put_failed(&mut self, key: &str) {
        self.inflight = self.inflight.saturating_sub(1);
        self.map.insert(key.to_string(), Thumb::Failed);
    }

    /// 已就绪的纹理。
    pub fn texture(&self, key: &str) -> Option<&TextureHandle> {
        match self.map.get(key) {
            Some(Thumb::Ready(t)) => Some(t),
            _ => None,
        }
    }

    /// 每帧调用：把 Raw 字节解码成纹理（失败转 Failed）。
    pub fn pump(&mut self, ctx: &Context) {
        let pending: Vec<(String, Vec<u8>)> = self
            .map
            .iter()
            .filter_map(|(k, v)| match v {
                Thumb::Raw(b) => Some((k.clone(), b.clone())),
                _ => None,
            })
            .collect();
        for (key, bytes) in pending {
            match decode(&bytes) {
                Some(image) => {
                    let tex =
                        ctx.load_texture(format!("thumb:{key}"), image, TextureOptions::LINEAR);
                    self.map.insert(key, Thumb::Ready(tex));
                }
                None => {
                    self.map.insert(key, Thumb::Failed);
                }
            }
        }
    }

    fn evict_if_needed(&mut self) {
        while self.map.len() > MAX_ENTRIES {
            let Some(old) = self.order.first().cloned() else {
                break;
            };
            self.order.remove(0);
            self.map.remove(&old);
        }
    }
}

/// 解码图片并限制最大边长（缩略图一般很小，这里兜底防大图）。
fn decode(bytes: &[u8]) -> Option<ColorImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let img = if img.width() > MAX_TEXTURE_SIDE || img.height() > MAX_TEXTURE_SIDE {
        img.thumbnail(MAX_TEXTURE_SIDE, MAX_TEXTURE_SIDE)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    Some(ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        rgba.as_raw(),
    ))
}
