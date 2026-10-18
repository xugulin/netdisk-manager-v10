//! 车道布局引擎（纯逻辑、可单测）。
//!
//! # 坐标体系
//! 内部一律用「视频高度为 1.0 的相对坐标」：`y_ratio` = 文本顶部相对高度，
//! `width_ratio` = 文本宽度相对视频宽度。字号 `font_size` 以 1080p 为基准，
//! 估算像素宽度时按 `video_height/1080` 自动换算，因此 **1080p 与 4K 的布局结果
//! （相对坐标）逐项一致**，渲染时只需按窗口/视频矩形映射。
//!
//! # 滚动弹幕防追尾（核心几何）
//! 同车道前一条弹幕 A `(startA, widthA)` 与后进入的 B `(startB, widthB)`，
//! 两者都以线性速度移动（穿越时长同为 `duration`）：
//! - A 左边界：`1 - (1+widthA) * (t-startA)/duration`
//! - B 左边界：`1 - (1+widthB) * (t-startB)/duration`
//!
//! 不重叠的充要条件（两条都成立）：
//! 1. B 刚进入时 A 已完全进入屏幕：`(1+widthA)*Δ/duration - widthA >= 0`
//! 2. A 即将离开时 B 尚未追上：`(1+widthB)*Δ/duration - widthB >= 0`
//!
//! 其中 `Δ = startB - startA`。见 [`scroll_overlap`]。
//!
//! # 密度上限
//! 同屏活跃弹幕数达到 `density_limit` 时按 [`DensityPolicy`] 处理：
//! 默认丢弃新弹幕；可选保高权重（替换同屏权重最低者）或稳定哈希保随机。
//! `density_limit == 0` 表示不限制。
//!
//! # 固定弹幕
//! 顶部从显示区上沿向下、底部从下沿向上分配车道，车道被占用 `fixed_duration_ms`。

use crate::model::{
    comment_weight, Comment, DanmakuMode, DensityPolicy, LayoutConfig, LayoutGeometry, LayoutStats,
    PlacedComment,
};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// 1080p 基准字号（像素）。
const BASE_FONT_SIZE: f32 = 25.0;
/// 行高相对字号的倍率。
const LINE_HEIGHT_RATIO: f32 = 1.2;
/// 1080p 基准高度（字号换算基准）。
const BASE_HEIGHT: f32 = 1080.0;

/// 计算布局几何（车道高度/数量、显示区、实际时长）。
pub fn geometry(cfg: &LayoutConfig) -> LayoutGeometry {
    let cfg = cfg.sanitized();
    // 车道高度：基准字号 * 1.2 * 缩放，相对视频高度（与分辨率无关）
    let lane_height_ratio =
        (BASE_FONT_SIZE * LINE_HEIGHT_RATIO * cfg.font_scale / BASE_HEIGHT).clamp(0.005, 0.5);
    let region_top = cfg.area_top.clamp(0.0, 1.0);
    let region_bottom = (1.0 - cfg.area_bottom).clamp(region_top, 1.0);
    let usable = (region_bottom - region_top).max(lane_height_ratio);
    let lane_count = ((usable / lane_height_ratio).floor() as usize).max(1);
    let scroll_duration_ms = ((cfg.scroll_duration_ms as f32) / cfg.speed).round() as i64;
    LayoutGeometry {
        lane_height_ratio,
        lane_count,
        region_top,
        region_bottom,
        scroll_duration_ms: scroll_duration_ms.max(100),
        fixed_duration_ms: cfg.fixed_duration_ms.max(100),
    }
}

/// 估算文本宽度（相对视频宽度 0~1）。
///
/// CJK/全角按 1 em、ASCII/半角按 0.5 em 估算；字号先按 `video_height/1080`
/// 换算成实际像素，再除以视频宽度 → **相对宽度与分辨率无关**。
pub fn text_width_ratio(text: &str, font_size: f32, cfg: &LayoutConfig) -> f32 {
    let cfg = cfg.sanitized();
    let em: f32 = text.chars().map(char_width_em).sum::<f32>().max(1.0);
    let font_size = if font_size.is_finite() && font_size > 0.0 {
        font_size
    } else {
        BASE_FONT_SIZE
    };
    // 相对视频高度的字号比例
    let font_ratio = font_size * cfg.font_scale / BASE_HEIGHT;
    let width_px = em * font_ratio * cfg.video_height as f32;
    (width_px / cfg.video_width as f32).clamp(0.002, 8.0)
}

fn char_width_em(ch: char) -> f32 {
    // 0x2E80 起为 CJK 部首/汉字/假名/全角符号
    if (ch as u32) < 0x2E80 {
        0.5
    } else {
        1.0
    }
}

/// 同车道滚动弹幕是否会重叠/追尾。
///
/// `prev` 为同车道前一条，`new` 为后进入的一条；`duration_ms` 为穿越屏幕时长。
/// 返回 `true` 表示冲突（不能放同车道）。
pub fn scroll_overlap(
    prev_start_ms: i64,
    prev_width: f32,
    new_start_ms: i64,
    new_width: f32,
    duration_ms: i64,
) -> bool {
    let duration = duration_ms.max(1) as f32;
    let prev_width = prev_width.max(0.0);
    let new_width = new_width.max(0.0);
    if new_start_ms >= prev_start_ms + duration_ms.max(1) {
        return false; // 前一条已完全离开
    }
    if new_start_ms <= prev_start_ms {
        return true; // 同/更早进入同一车道，必然重叠
    }
    let delta = (new_start_ms - prev_start_ms) as f32 / duration;
    let enter_ok = (1.0 + prev_width) * delta - prev_width >= -1e-6;
    let exit_ok = (1.0 + new_width) * delta - new_width >= -1e-6;
    !(enter_ok && exit_ok)
}

fn stable_hash(comment: &Comment) -> u64 {
    let mut hasher = DefaultHasher::new();
    comment.time_ms.hash(&mut hasher);
    comment.text.hash(&mut hasher);
    comment.mode.to_bilibili_code().hash(&mut hasher);
    hasher.finish()
}

/// 布局（默认密度策略：丢弃新弹幕）。
///
/// 输出按 `(start_ms, lane)` 排序；被密度/车道限制丢弃的弹幕不出现。
pub fn layout(comments: &[Comment], cfg: &LayoutConfig) -> Vec<PlacedComment> {
    layout_with_policy(comments, cfg, DensityPolicy::DropNew).0
}

/// 布局并返回统计。
pub fn layout_with_policy(
    comments: &[Comment],
    cfg: &LayoutConfig,
    policy: DensityPolicy,
) -> (Vec<PlacedComment>, LayoutStats) {
    let cfg = cfg.sanitized();
    let geom = geometry(&cfg);
    let mut stats = LayoutStats {
        input_count: comments.len(),
        lane_count: geom.lane_count,
        ..LayoutStats::default()
    };
    let density_limit = if cfg.density_limit == 0 {
        usize::MAX
    } else {
        cfg.density_limit
    };

    let mut ordered: Vec<Comment> = comments.iter().map(|c| c.normalized()).collect();
    ordered.sort_by_key(|c| c.time_ms);

    let mut scroll_lanes: Vec<Option<(i64, f32)>> = vec![None; geom.lane_count];
    let mut top_free: Vec<i64> = vec![i64::MIN; geom.lane_count];
    let mut bottom_free: Vec<i64> = vec![i64::MIN; geom.lane_count];

    let mut placed: Vec<Option<PlacedComment>> = Vec::with_capacity(ordered.len());
    // 同屏活跃弹幕：(placed 下标, 结束时间, 权重)
    let mut active: Vec<(usize, i64, f32)> = Vec::new();
    let mut expired_before = i64::MIN;

    for comment in ordered {
        if comment.text.is_empty() {
            stats.skipped_empty += 1;
            continue;
        }
        let start = comment.time_ms.max(0);
        if start > expired_before {
            active.retain(|(_, end, _)| *end > start);
            expired_before = start;
        }
        let width = text_width_ratio(&comment.text, comment.font_size, &cfg);

        // ---- 车道分配 ----
        let mut lane: Option<usize> = None;
        let mut y_ratio = geom.region_top;
        let end_ms;
        match comment.mode {
            DanmakuMode::Scroll => {
                end_ms = start + geom.scroll_duration_ms;
                let mut best_key: Option<(i64, i64, usize)> = None;
                for (index, slot) in scroll_lanes.iter().enumerate() {
                    let free = match slot {
                        None => true,
                        Some((prev_start, prev_width)) => !scroll_overlap(
                            *prev_start,
                            *prev_width,
                            start,
                            width,
                            geom.scroll_duration_ms,
                        ),
                    };
                    if !free {
                        continue;
                    }
                    let free_at = match slot {
                        None => i64::MIN,
                        Some((prev_start, _)) => prev_start + geom.scroll_duration_ms,
                    };
                    // 优先「已完全空闲」的车道 → 再比空闲时间 → 再比车道序号
                    let key = (if free_at <= start { 0 } else { 1 }, free_at, index);
                    if best_key.is_none_or(|k| key < k) {
                        best_key = Some(key);
                        lane = Some(index);
                    }
                }
                if let Some(index) = lane {
                    scroll_lanes[index] = Some((start, width));
                    y_ratio = geom.lane_top(index);
                }
            }
            DanmakuMode::Top => {
                end_ms = start + geom.fixed_duration_ms;
                let mut best: Option<(i64, usize)> = None;
                for (index, free_at) in top_free.iter().enumerate() {
                    if *free_at <= start && best.is_none_or(|(f, l)| (*free_at, index) < (f, l)) {
                        best = Some((*free_at, index));
                    }
                }
                if let Some((_, index)) = best {
                    top_free[index] = end_ms;
                    lane = Some(index);
                    y_ratio = geom.lane_top(index);
                }
            }
            DanmakuMode::Bottom => {
                end_ms = start + geom.fixed_duration_ms;
                let mut best: Option<(i64, usize)> = None;
                for (index, free_at) in bottom_free.iter().enumerate() {
                    let y = geom.bottom_lane_top(index);
                    if y < geom.region_top - 1e-6 {
                        continue; // 底部车道向上溢出显示区
                    }
                    if *free_at <= start && best.is_none_or(|(f, l)| (*free_at, index) < (f, l)) {
                        best = Some((*free_at, index));
                    }
                }
                if let Some((_, index)) = best {
                    bottom_free[index] = end_ms;
                    lane = Some(index);
                    y_ratio = geom.bottom_lane_top(index);
                }
            }
        }

        let Some(lane) = lane else {
            stats.dropped_no_lane += 1;
            continue;
        };

        // ---- 同屏密度限制 ----
        if active.len() >= density_limit {
            let weight = comment_weight(&comment);
            let evict = match policy {
                DensityPolicy::DropNew => None,
                DensityPolicy::KeepHighPriority => active
                    .iter()
                    .enumerate()
                    .min_by(|(_, a), (_, b)| {
                        a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .and_then(|(pos, entry)| (weight > entry.2 + 1e-6).then_some(pos)),
                DensityPolicy::KeepRandom => {
                    if stable_hash(&comment) % 2 == 0 {
                        // 随机保留新的：替换权重最低的活跃弹幕
                        active
                            .iter()
                            .enumerate()
                            .min_by(|(_, a), (_, b)| {
                                a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal)
                            })
                            .map(|(pos, _)| pos)
                    } else {
                        None
                    }
                }
            };
            match evict {
                Some(pos) => {
                    let (index, _, _) = active.remove(pos);
                    if let Some(slot) = placed.get_mut(index) {
                        *slot = None;
                    }
                }
                None => {
                    stats.dropped_density += 1;
                    continue;
                }
            }
        }

        let index = placed.len();
        placed.push(Some(PlacedComment {
            comment: comment.clone(),
            lane,
            y_ratio,
            width_ratio: width,
            start_ms: start,
            end_ms,
        }));
        active.push((index, end_ms, comment_weight(&comment)));
    }

    let mut result: Vec<PlacedComment> = placed.into_iter().flatten().collect();
    result.sort_by_key(|p| (p.start_ms, p.lane));
    stats.placed_count = result.len();
    (result, stats)
}

/// 布局后的时间轴索引（Mode A egui 覆盖层逐帧查询用，二分定位活跃弹幕）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlacedTimeline {
    placed: Vec<PlacedComment>,
    max_duration_ms: i64,
}

impl PlacedTimeline {
    /// 构造（内部按开始时间排序）。
    pub fn new(mut placed: Vec<PlacedComment>) -> Self {
        placed.sort_by_key(|p| (p.start_ms, p.end_ms));
        let max_duration_ms = placed.iter().map(|p| p.duration_ms()).max().unwrap_or(0);
        Self {
            placed,
            max_duration_ms,
        }
    }

    /// 全部已布局弹幕（按开始时间排序）。
    pub fn placed(&self) -> &[PlacedComment] {
        &self.placed
    }

    /// 条数。
    pub fn len(&self) -> usize {
        self.placed.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.placed.is_empty()
    }

    /// 某一时刻可见弹幕在 [`Self::placed`] 中的下标（O(log n + k)）。
    pub fn active_indices(&self, time_ms: i64) -> Vec<usize> {
        let hi = self.placed.partition_point(|p| p.start_ms <= time_ms);
        let lo = self
            .placed
            .partition_point(|p| p.start_ms <= time_ms - self.max_duration_ms)
            .min(hi);
        (lo..hi)
            .filter(|&i| self.placed[i].end_ms > time_ms)
            .collect()
    }

    /// 某一时刻可见的弹幕（O(log n + k)）。
    pub fn active(&self, time_ms: i64) -> Vec<&PlacedComment> {
        self.active_indices(time_ms)
            .into_iter()
            .map(|i| &self.placed[i])
            .collect()
    }

    /// 某一时刻同屏弹幕数（比 `active()` 少一次分配；密度诊断/热力图用）。
    pub fn active_count(&self, time_ms: i64) -> usize {
        self.active_indices(time_ms).len()
    }

    /// 某一时刻同屏弹幕数（`active_count` 的语义化别名）。
    pub fn density_at(&self, time_ms: i64) -> usize {
        self.active_count(time_ms)
    }

    /// 下一次画面变化的时间（没有则 None）——用于避免空转重绘。
    pub fn next_change_after(&self, time_ms: i64) -> Option<i64> {
        let mut next: Option<i64> = None;
        for p in self.active(time_ms) {
            if p.end_ms > time_ms {
                next = Some(next.map_or(p.end_ms, |n: i64| n.min(p.end_ms)));
            }
        }
        let idx = self.placed.partition_point(|p| p.start_ms <= time_ms);
        if let Some(p) = self.placed.get(idx) {
            next = Some(next.map_or(p.start_ms, |n| n.min(p.start_ms)));
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scroll_comment(ms: i64, text: &str) -> Comment {
        Comment::new(ms, DanmakuMode::Scroll, text)
    }

    fn fixed_comment(ms: i64, mode: DanmakuMode, text: &str) -> Comment {
        Comment::new(ms, mode, text)
    }

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn geometry_same_for_1080p_and_4k() {
        let hd = geometry(&LayoutConfig::hd1080());
        let uhd = geometry(&LayoutConfig::uhd4k());
        assert_eq!(hd, uhd);
        assert!(hd.lane_count > 1);
        assert!(approx(hd.region_top, 0.03));
        assert!(approx(hd.region_bottom, 0.75));
    }

    #[test]
    fn geometry_speed_changes_duration() {
        let mut cfg = LayoutConfig::hd1080();
        cfg.speed = 2.0;
        let fast = geometry(&cfg);
        assert_eq!(fast.scroll_duration_ms, 4000);
        cfg.speed = 0.5;
        assert_eq!(geometry(&cfg).scroll_duration_ms, 16000);
    }

    #[test]
    fn width_estimate_relative_and_cjk_wider() {
        let cfg = LayoutConfig::hd1080();
        let cjk = text_width_ratio("中文弹幕", 25.0, &cfg);
        let ascii = text_width_ratio("abcd", 25.0, &cfg);
        assert!(cjk > ascii);
        // 1080p 与 4K 的相对宽度一致
        let uhd = LayoutConfig::uhd4k();
        assert!(approx(cjk, text_width_ratio("中文弹幕", 25.0, &uhd)));
        assert!(cjk > 0.0 && cjk < 1.0);
    }

    #[test]
    fn scroll_overlap_geometry() {
        // 前一条很窄、间隔足够 → 不追尾
        assert!(!scroll_overlap(0, 0.05, 3000, 0.05, 8000));
        // 前一条很宽（占半屏）、后一条紧随 → 追尾
        assert!(scroll_overlap(0, 4.0, 1000, 0.05, 8000));
        // 后一条特别宽（飞快）、间隔不足 → A 未离开时 B 已追上
        assert!(scroll_overlap(0, 0.02, 2000, 4.0, 8000));
        // 前一条已完全离开
        assert!(!scroll_overlap(0, 0.05, 8000, 0.05, 8000));
        assert!(!scroll_overlap(0, 2.0, 8000, 2.0, 8000));
        // 同一时刻进入同车道 → 冲突
        assert!(scroll_overlap(1000, 0.1, 1000, 0.1, 8000));
    }

    #[test]
    fn layout_single_lane_no_tailgating() {
        let mut cfg = LayoutConfig::hd1080();
        // 显示区压到 0.45~0.50 → 只有 1 条车道
        cfg.area_top = 0.45;
        cfg.area_bottom = 0.5;
        let geom = geometry(&cfg);
        assert_eq!(geom.lane_count, 1);
        let comments = vec![
            scroll_comment(0, "第一条"),
            scroll_comment(3000, "第二条"),
            scroll_comment(6000, "第三条"),
        ];
        let placed = layout(&comments, &cfg);
        assert_eq!(placed.len(), 3);
        for (i, a) in placed.iter().enumerate() {
            assert_eq!(a.lane, 0);
            for b in placed.iter().skip(i + 1) {
                assert!(
                    !scroll_overlap(a.start_ms, a.width_ratio, b.start_ms, b.width_ratio, 8000),
                    "同车道追尾：{:?} / {:?}",
                    a.comment.text,
                    b.comment.text
                );
            }
        }
    }

    #[test]
    fn layout_dense_drops_but_no_overlap() {
        let mut cfg = LayoutConfig::hd1080();
        // 1 条车道
        cfg.area_top = 0.45;
        cfg.area_bottom = 0.5;
        cfg.density_limit = 0; // 不限制密度，只受车道限制
        let comments: Vec<Comment> = (0..20)
            .map(|i| scroll_comment(i * 100, "很长的弹幕文本内容"))
            .collect();
        let (placed, stats) = layout_with_policy(&comments, &cfg, DensityPolicy::DropNew);
        assert!(placed.len() < comments.len());
        assert!(stats.dropped_no_lane > 0);
        for (i, a) in placed.iter().enumerate() {
            for b in placed.iter().skip(i + 1) {
                assert!(!scroll_overlap(
                    a.start_ms,
                    a.width_ratio,
                    b.start_ms,
                    b.width_ratio,
                    geom_duration(&cfg)
                ));
            }
        }
    }

    fn geom_duration(cfg: &LayoutConfig) -> i64 {
        ((cfg.scroll_duration_ms as f32) / cfg.speed).round() as i64
    }

    #[test]
    fn density_limit_drop_new() {
        let mut cfg = LayoutConfig::hd1080();
        cfg.density_limit = 3;
        let comments: Vec<Comment> = (0..20)
            .map(|i| fixed_comment(0, DanmakuMode::Top, &format!("top{i}")))
            .collect();
        let (placed, stats) = layout_with_policy(&comments, &cfg, DensityPolicy::DropNew);
        assert_eq!(placed.len(), 3);
        assert_eq!(stats.dropped_density, 17);
        assert_eq!(stats.placed_count, 3);
    }

    #[test]
    fn density_keep_high_priority() {
        let mut cfg = LayoutConfig::hd1080();
        cfg.density_limit = 2;
        let mut big = fixed_comment(0, DanmakuMode::Scroll, "彩色大字号高权重");
        big.color = 0xFF0000;
        big.font_size = 40.0;
        let comments = vec![
            fixed_comment(0, DanmakuMode::Scroll, "普通一"),
            fixed_comment(0, DanmakuMode::Scroll, "普通二"),
            big,
        ];
        let (placed, _) = layout_with_policy(&comments, &cfg, DensityPolicy::KeepHighPriority);
        assert_eq!(placed.len(), 2);
        assert!(placed.iter().any(|p| p.comment.text == "彩色大字号高权重"));
    }

    #[test]
    fn fixed_lane_occupancy() {
        let mut cfg = LayoutConfig::hd1080();
        // 显示区压到 0.45~0.50 → 1 条车道
        cfg.area_top = 0.45;
        cfg.area_bottom = 0.5;
        let comments = vec![
            fixed_comment(0, DanmakuMode::Top, "第一条"),
            fixed_comment(1000, DanmakuMode::Top, "太近放不下"),
            fixed_comment(5000, DanmakuMode::Top, "足够远"),
        ];
        let (placed, stats) = layout_with_policy(&comments, &cfg, DensityPolicy::DropNew);
        // 0ms 占车道到 4000ms；1000ms 无车道 → 丢弃；5000ms 可再用
        assert_eq!(placed.len(), 2);
        assert_eq!(placed[0].lane, 0);
        assert_eq!(placed[1].lane, 0);
        assert_eq!(placed[1].start_ms, 5000);
        assert_eq!(stats.dropped_no_lane, 1);
    }

    #[test]
    fn bottom_lanes_from_bottom() {
        let mut cfg = LayoutConfig::hd1080();
        cfg.font_scale = 2.0; // 少一些车道但足够 2 条
        let geom = geometry(&cfg);
        assert!(geom.lane_count >= 2);
        let comments = vec![
            fixed_comment(0, DanmakuMode::Bottom, "底1"),
            fixed_comment(0, DanmakuMode::Bottom, "底2"),
        ];
        let placed = layout(&comments, &cfg);
        assert_eq!(placed.len(), 2);
        let ys: Vec<f32> = placed.iter().map(|p| p.y_ratio).collect();
        // 第二个底部弹幕在更上方
        assert!(ys[0] > ys[1]);
        for p in &placed {
            assert!(p.y_ratio >= geom.region_top - 1e-6);
            assert!(p.y_ratio + geom.lane_height_ratio <= geom.region_bottom + 1e-6);
            assert_eq!(p.end_ms - p.start_ms, geom.fixed_duration_ms);
        }
    }

    #[test]
    fn layout_4k_equals_1080p_relative() {
        let comments: Vec<Comment> = (0..40)
            .map(|i| {
                let mode = match i % 3 {
                    0 => DanmakuMode::Scroll,
                    1 => DanmakuMode::Top,
                    _ => DanmakuMode::Bottom,
                };
                Comment::new(i * 350, mode, format!("弹幕{i}"))
            })
            .collect();
        let hd = layout(&comments, &LayoutConfig::hd1080());
        let uhd = layout(&comments, &LayoutConfig::uhd4k());
        assert_eq!(hd.len(), uhd.len());
        for (a, b) in hd.iter().zip(uhd.iter()) {
            assert_eq!(a.comment.text, b.comment.text);
            assert_eq!(a.lane, b.lane);
            assert!((a.y_ratio - b.y_ratio).abs() < 1e-6);
            assert!((a.width_ratio - b.width_ratio).abs() < 1e-6);
            assert_eq!(a.start_ms, b.start_ms);
            assert_eq!(a.end_ms, b.end_ms);
        }
    }

    #[test]
    fn layout_negative_time_and_empty_text() {
        let cfg = LayoutConfig::hd1080();
        let comments = vec![
            Comment::new(-500, DanmakuMode::Scroll, "负时间"),
            Comment::new(1000, DanmakuMode::Scroll, ""),
            Comment::new(2000, DanmakuMode::Top, "   "),
        ];
        let (placed, stats) = layout_with_policy(&comments, &cfg, DensityPolicy::DropNew);
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].start_ms, 0);
        assert_eq!(stats.skipped_empty, 2);
    }

    #[test]
    fn layout_empty_input() {
        let (placed, stats) =
            layout_with_policy(&[], &LayoutConfig::default(), DensityPolicy::DropNew);
        assert!(placed.is_empty());
        assert_eq!(stats.input_count, 0);
        assert_eq!(stats.placed_count, 0);
    }

    #[test]
    fn scroll_duration_applies_speed() {
        let mut cfg = LayoutConfig::hd1080();
        cfg.speed = 2.0;
        let placed = layout(&[scroll_comment(0, "x")], &cfg);
        assert_eq!(placed[0].end_ms - placed[0].start_ms, 4000);
        cfg.speed = 0.5;
        let placed = layout(&[scroll_comment(0, "x")], &cfg);
        assert_eq!(placed[0].end_ms - placed[0].start_ms, 16000);
    }

    #[test]
    fn layout_respects_area_bounds() {
        let mut cfg = LayoutConfig::hd1080();
        cfg.area_top = 0.1;
        cfg.area_bottom = 0.2;
        let geom = geometry(&cfg);
        let comments: Vec<Comment> = (0..30)
            .flat_map(|i| {
                [
                    Comment::new(i * 200, DanmakuMode::Scroll, format!("s{i}")),
                    Comment::new(i * 200, DanmakuMode::Top, format!("t{i}")),
                ]
            })
            .collect();
        let placed = layout(&comments, &cfg);
        for p in &placed {
            assert!(p.y_ratio >= geom.region_top - 1e-6);
            assert!(p.y_ratio + geom.lane_height_ratio <= geom.region_bottom + 1e-6);
        }
    }

    #[test]
    fn layout_10k_comments_completes() {
        // T23 性能目标：10 万条布局 < 1s；单测跑 1 万条确保不退化
        let comments: Vec<Comment> = (0..10_000)
            .map(|i| {
                let mode = match i % 10 {
                    0 => DanmakuMode::Top,
                    1 => DanmakuMode::Bottom,
                    _ => DanmakuMode::Scroll,
                };
                Comment::new(i * 20, mode, format!("弹幕内容{i}"))
            })
            .collect();
        let (placed, stats) =
            layout_with_policy(&comments, &LayoutConfig::hd1080(), DensityPolicy::DropNew);
        assert_eq!(stats.input_count, 10_000);
        assert!(!placed.is_empty());
        // 输出必须有序，便于渲染逐帧二分
        assert!(placed.windows(2).all(|w| w[0].start_ms <= w[1].start_ms));
    }

    #[test]
    fn timeline_active_and_next_change() {
        let placed = layout(
            &[
                scroll_comment(0, "a"),
                scroll_comment(1000, "b"),
                fixed_comment(5000, DanmakuMode::Top, "c"),
            ],
            &LayoutConfig::hd1080(),
        );
        let timeline = PlacedTimeline::new(placed);
        assert_eq!(timeline.len(), 3);
        let active = timeline.active(500);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].comment.text, "a");
        assert_eq!(timeline.density_at(1500), 2);
        assert!(timeline.next_change_after(0).is_some());
        let timeline_empty = PlacedTimeline::new(vec![]);
        assert!(timeline_empty.is_empty());
        assert_eq!(timeline_empty.next_change_after(0), None);
    }
}
