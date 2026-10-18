//! 示例：弹幕 egui 覆盖层几何（纯逻辑，不依赖 egui）。
//!
//! 演示 gui-player 接入方式：
//! 1. `layout()` 预计算车道 → `PlacedTimeline`；
//! 2. 每帧用当前播放时间 + 视频画面矩形调用 `active_geometry()` 拿到要画的文字；
//! 3. `hit_test()` 做悬停命中（暂停/复制）。
//!
//! 运行：
//! ```text
//! cargo run -p wp-danmaku --example overlay_geometry
//! ```

use wp_danmaku::{Comment, DanmakuMode, LayoutConfig, PlacedTimeline, VideoRect};

fn main() {
    // 一批 4K 弹幕（真实使用时应从 B 站 XML/JSON/本地弹幕库解析）
    let comments = vec![
        Comment::new(0, DanmakuMode::Scroll, "第一条滚动弹幕").with_color(0xFFFFFF),
        Comment::new(500, DanmakuMode::Scroll, "第二条比较长的滚动弹幕内容").with_color(0xFFD700),
        Comment::new(1000, DanmakuMode::Top, "顶部固定弹幕").with_color(0xFF0000),
        Comment::new(1500, DanmakuMode::Scroll, "绿色滚动").with_color(0x00FF00),
        Comment::new(2000, DanmakuMode::Bottom, "底部固定弹幕").with_color(0x00BFFF),
    ];
    let cfg = LayoutConfig::uhd4k();
    let timeline = PlacedTimeline::new(wp_danmaku::layout(&comments, &cfg));

    // 模拟 GUI：窗口 1600x1000 里按 16:9 contain 适配视频（上下黑边）
    let area = VideoRect::new(0.0, 0.0, 1600.0, 1000.0);
    let video = VideoRect::fit_video(area, cfg.video_width, cfg.video_height);
    println!(
        "窗口 {:?} → 视频画面矩形 x={:.1} y={:.1} w={:.1} h={:.1}（font_scale={:.3}）",
        (area.w, area.h),
        video.x,
        video.y,
        video.w,
        video.h,
        video.font_scale()
    );

    // 逐帧取几何（此处离散采样）
    for at_ms in [0_i64, 500, 1000, 2000, 3000, 4000, 8000] {
        let glyphs = timeline.active_geometry(at_ms, video);
        println!(
            "\nt={at_ms:>5}ms  同屏 {} 条（active_count={}）",
            glyphs.len(),
            timeline.active_count(at_ms)
        );
        for glyph in glyphs.iter().take(6) {
            let width = glyph.width_px(video);
            println!(
                "  x={:>7.1} y={:>6.1} font={:>4.1}px w≈{:>6.1} color={:?} alpha={:>3}  「{}」",
                glyph.x, glyph.y, glyph.font_px, width, glyph.color, glyph.alpha, glyph.text
            );
        }
    }

    // 命中测试（gui-player 悬停暂停用）：取 t=2000 第一条可见弹幕的中心点
    let at_ms = 2000;
    if let Some(glyph) = timeline.active_geometry(at_ms, video).first() {
        let pointer = (
            glyph.x + glyph.width_px(video) * 0.5,
            glyph.y + glyph.font_px * 0.5,
        );
        match timeline.hit_test_comment(at_ms, video, pointer) {
            Some(placed) => println!(
                "\nhit_test({:?}) → 「{}」mode={} lane={}",
                pointer,
                placed.comment.text,
                placed.comment.mode.label(),
                placed.lane
            ),
            None => println!("\nhit_test({pointer:?}) → None"),
        }
    }

    // 4K / 1080p 一致性：同一批弹幕同一窗口下几何应完全一致
    let hd = PlacedTimeline::new(wp_danmaku::layout(&comments, &LayoutConfig::hd1080()));
    let a = timeline.active_geometry(2000, video);
    let b = hd.active_geometry(2000, video);
    println!(
        "\n4K/1080p 几何一致: {}（glyph 数 {} vs {}）",
        a == b,
        a.len(),
        b.len()
    );
    // 窗口缩放一致性：窗口减半 → 相对坐标不变、font_px 减半
    let half = VideoRect::new(video.x * 0.5, video.y * 0.5, video.w * 0.5, video.h * 0.5);
    if let (Some(ga), Some(gb)) = (
        timeline.active_geometry(2000, video).first(),
        timeline.active_geometry(2000, half).first(),
    ) {
        println!(
            "窗口缩放: base x={:.1} font={:.1} / half x={:.1} font={:.1}（相对 x {:.4} vs {:.4}）",
            ga.x,
            ga.font_px,
            gb.x,
            gb.font_px,
            ga.x / video.w,
            gb.x / half.w
        );
    }
}
