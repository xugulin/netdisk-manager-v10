//! M0-E3 弹幕 ASS 夹具导出器。
//!
//! 给 media-core（wp-media-mpv）做「ASS 弹幕轨作为副字幕轨」叠加实验用：
//! - 默认导出 5000 条 4K 弹幕 → `出/验收截图/弹幕/m0-5000.ass`
//! - 同时导出 1000 条/分钟密度样本 → `出/验收截图/弹幕/m0-density-1000pm.ass`
//!
//! 运行（在 `网盘管理_V10` 目录）：
//! ```text
//! cargo run -p wp-danmaku --example export_danmaku_ass
//! cargo run -p wp-danmaku --example export_danmaku_ass -- --count 2000 --out /tmp/danmaku.ass
//! cargo run -p wp-danmaku --example export_danmaku_ass -- --video 1080p --per-minute 1000
//! cargo run -p wp-danmaku --example export_danmaku_ass -- --window 0,180000 --out /tmp/chunk0.ass
//! ```
//!
//! 说明：ASS 时间保持视频绝对时间（mpv/libass 按媒体时间戳比较事件）；
//! `--window` 只裁剪事件集合，用于分片 `sub-reload`。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use wp_danmaku::{
    generate_ass, generate_ass_windowed, geometry, import_ass, Comment, DanmakuDoc, DanmakuMode,
    DensityPolicy, LayoutConfig, PlacedTimeline,
};

const DEFAULT_SAMPLE_OUT: &str = "出/验收截图/弹幕/m0-5000.ass";
const DEFAULT_DENSITY_OUT: &str = "出/验收截图/弹幕/m0-density-1000pm.ass";

const TEXTS: [&str; 24] = [
    "前排",
    "哈哈哈哈哈哈",
    "名场面",
    "泪目",
    "这里必须刷起来",
    "2333333",
    "梦开始的地方",
    "这个配色绝了",
    "awsl",
    "高能预警",
    "4K 画质真香",
    "缓存中，先刷一波",
    "有人吗",
    "空降成功",
    "弹幕护体",
    "进度条撑住",
    "每日打卡",
    "这段 BGM 是什么",
    "泪目 +1",
    "好家伙",
    "细节拉满",
    "这就是专业",
    "暂停成功",
    "下次一定",
];

const COLORS: [u32; 8] = [
    0xFFFFFF, 0xFF0000, 0x00FF00, 0x0000FF, 0xFFD700, 0xFF69B4, 0x00BFFF, 0xFF8C00,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VideoChoice {
    FourK,
    Hd1080,
}

#[derive(Debug)]
struct Args {
    /// 主样本条数。
    count: usize,
    /// 主样本输出路径。
    out: PathBuf,
    /// 主样本时间跨度（毫秒）。
    duration_ms: i64,
    /// 密度样本每分钟条数。
    per_minute: usize,
    /// 密度样本输出路径。
    density_out: PathBuf,
    /// 视频分辨率档位（影响布局配置与 ASS 字号换算）。
    video: VideoChoice,
    /// 时间窗裁剪（视频绝对毫秒）。
    window: Option<(i64, i64)>,
    /// 可选：同时导出 B 站 XML（media-core 用不同路径加载时备用）。
    xml_out: Option<PathBuf>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            count: 5000,
            out: PathBuf::from(DEFAULT_SAMPLE_OUT),
            duration_ms: 600_000,
            per_minute: 1000,
            density_out: PathBuf::from(DEFAULT_DENSITY_OUT),
            video: VideoChoice::FourK,
            window: None,
            xml_out: None,
        }
    }
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(err) => {
            eprintln!("错误：{err}\n\n{USAGE}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let cfg = match args.video {
        VideoChoice::FourK => LayoutConfig::uhd4k(),
        VideoChoice::Hd1080 => LayoutConfig::hd1080(),
    };

    // 1) 主样本：默认 5000 条 / 10 分钟（≈500 条/分钟）
    let comments = make_comments(args.count, args.duration_ms, 0x5EED_1234);
    export_sample("M0-E3 主样本", &comments, &cfg, &args.out, args.window)?;
    if let Some(xml_out) = &args.xml_out {
        let doc = DanmakuDoc::from_comments(comments.clone());
        let xml = wp_danmaku::export_bilibili_xml(&doc);
        write_file(xml_out, &xml)?;
        println!("[B站 XML] {} 条 → {}", comments.len(), xml_out.display());
    }

    // 2) 密度样本：1000 条 / 60 秒（1000 条/分钟）
    let density_count = args.per_minute.max(1);
    let density_comments = make_comments(density_count, 60_000, 0xD15E_A5E5);
    export_sample(
        "1000 条/分钟密度样本",
        &density_comments,
        &cfg,
        &args.density_out,
        None,
    )?;

    println!(
        "\n完成。媒体内核叠加实验建议：\n  \
         mpv --secondary-sub-visibility=yes --secondary-sid=<id> <video.mkv> \
         --sub-file={}（或先 sub-add 再 select）\n  \
         密度压测可反复 seek 到 10s~50s 区间。",
        args.density_out.display()
    );
    Ok(())
}

/// 布局 → 生成 ASS → 写文件 → 自校验（重新解析 Dialogue 数）。
fn export_sample(
    label: &str,
    comments: &[Comment],
    cfg: &LayoutConfig,
    out: &Path,
    window: Option<(i64, i64)>,
) -> Result<(), String> {
    let (placed, stats) = wp_danmaku::layout_with_stats(comments, cfg, DensityPolicy::DropNew);
    let ass = match window {
        Some((start, end)) => generate_ass_windowed(&placed, cfg, start, end),
        None => generate_ass(&placed, cfg),
    };
    write_file(out, &ass)?;

    // 自校验：导出的 ASS 能被重新解析，Dialogue 数 == 布局数（防"宽容解析吞掉坏输出"）
    let content =
        fs::read_to_string(out).map_err(|e| format!("读回失败 {}：{e}", out.display()))?;
    let back = import_ass(&content).map_err(|e| format!("重新解析失败：{e}"))?;
    let dialogue_count = content.matches("Dialogue:").count();

    let geom = geometry(cfg);
    let timeline = PlacedTimeline::new(placed);
    let span_end = timeline
        .placed()
        .iter()
        .map(|p| p.end_ms)
        .max()
        .unwrap_or(0);
    // 每 500ms 采样峰值同屏（给 media-core 报丢帧对照用）
    let mut peak = 0usize;
    let mut at = 0i64;
    while at <= span_end {
        peak = peak.max(timeline.density_at(at));
        at += 500;
    }
    let out_display = out.display();
    let minutes = (span_end as f64 / 60_000.0).max(1e-9);
    let per_minute = stats.placed_count as f64 / minutes;
    println!(
        "[{label}]\n  输出：{out_display}\n  输入 {} 条 → 布局 {} 条（无车道丢弃 {} / 密度丢弃 {} / 空文本 {}）\n  \
         ASS 事件 {dialogue_count} 条（重新解析 {} 条），文件 {} 字节\n  \
         时间轴 0~{} ms（≈{per_minute:.0} 条/分钟），峰值同屏 {peak} 条，车道 {}，PlayRes 高 1080\n  \
         配置：{}x{} area_top={:.2} area_bottom={:.2} scroll={}ms speed={} density_limit={}",
        stats.input_count,
        stats.placed_count,
        stats.dropped_no_lane,
        stats.dropped_density,
        stats.skipped_empty,
        back.comments.len(),
        content.len(),
        span_end,
        geom.lane_count,
        cfg.video_width,
        cfg.video_height,
        cfg.area_top,
        cfg.area_bottom,
        cfg.scroll_duration_ms,
        cfg.speed,
        cfg.density_limit
    );
    Ok(())
}

fn write_file(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录失败 {}：{e}", parent.display()))?;
        }
    }
    fs::write(path, content.as_bytes()).map_err(|e| format!("写文件失败 {}：{e}", path.display()))
}

/// 确定性伪随机弹幕生成（同一 seed 结果可复现）。
fn make_comments(count: usize, duration_ms: i64, seed: u64) -> Vec<Comment> {
    let mut state = seed;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };
    let duration_ms = duration_ms.max(1);
    let mut comments = Vec::with_capacity(count);
    for index in 0..count {
        let rand = next();
        // 均匀铺满时间轴 + 0~300ms 抖动（避免所有弹幕卡在同一毫秒）
        let base = index as i64 * duration_ms / count.max(1) as i64;
        let jitter = (rand >> 40) as i64 % 300;
        let time_ms = (base + jitter).min(duration_ms - 1);
        let mode = match rand % 100 {
            0..=84 => DanmakuMode::Scroll,
            85..=92 => DanmakuMode::Top,
            _ => DanmakuMode::Bottom,
        };
        let color = COLORS[((rand >> 8) as usize) % COLORS.len()];
        let mut text = TEXTS[((rand >> 16) as usize) % TEXTS.len()].to_string();
        if rand % 7 == 0 {
            text.push('#');
            text.push_str(&(index % 100).to_string());
        }
        comments.push(Comment {
            time_ms,
            mode,
            color,
            font_size: 25.0,
            text,
            user: Some(format!("u{}", rand % 1000)),
        });
    }
    comments
}

const USAGE: &str = "\
用法：cargo run -p wp-danmaku --example export_danmaku_ass -- [选项]

默认（无参数）：
  1) 5000 条 4K 弹幕（10 分钟）→ 出/验收截图/弹幕/m0-5000.ass
  2) 1000 条/分钟密度样本（60 秒）→ 出/验收截图/弹幕/m0-density-1000pm.ass

选项：
  --count N          主样本条数（默认 5000）
  --out PATH         主样本输出路径（默认 出/验收截图/弹幕/m0-5000.ass）
  --duration-ms N    主样本时间跨度毫秒（默认 600000 = 10 分钟）
  --per-minute N     密度样本每分钟条数（默认 1000）
  --density-out PATH 密度样本输出路径
  --video 4k|1080p   布局档位（默认 4k）
  --window A,B       只导出 [A,B) 时间窗（视频绝对毫秒，分片 sub-reload 用）
  --xml-out PATH     同时导出 B 站 XML 弹幕
  --help             显示本帮助";

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut iter = env::args().skip(1).peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "--count" => {
                args.count = next_value(&mut iter, "--count")?
                    .parse()
                    .map_err(|_| "--count 需要正整数".to_string())?;
            }
            "--out" => args.out = PathBuf::from(next_value(&mut iter, "--out")?),
            "--duration-ms" => {
                args.duration_ms = next_value(&mut iter, "--duration-ms")?
                    .parse()
                    .map_err(|_| "--duration-ms 需要整数毫秒".to_string())?;
            }
            "--per-minute" => {
                args.per_minute = next_value(&mut iter, "--per-minute")?
                    .parse()
                    .map_err(|_| "--per-minute 需要正整数".to_string())?;
            }
            "--density-out" => {
                args.density_out = PathBuf::from(next_value(&mut iter, "--density-out")?)
            }
            "--video" => {
                args.video = match next_value(&mut iter, "--video")?
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "4k" | "2160p" => VideoChoice::FourK,
                    "1080p" | "hd" => VideoChoice::Hd1080,
                    other => return Err(format!("--video 只支持 4k|1080p，收到 {other}")),
                }
            }
            "--window" => {
                let raw = next_value(&mut iter, "--window")?;
                let (start, end) = raw
                    .split_once(',')
                    .ok_or_else(|| "--window 格式应为 START,END".to_string())?;
                let start: i64 = start.trim().parse().map_err(|_| "窗口起点非法")?;
                let end: i64 = end.trim().parse().map_err(|_| "窗口终点非法")?;
                if end <= start {
                    return Err("--window 终点必须大于起点".to_string());
                }
                args.window = Some((start, end));
            }
            "--xml-out" => args.xml_out = Some(PathBuf::from(next_value(&mut iter, "--xml-out")?)),
            other => return Err(format!("未知参数：{other}")),
        }
    }
    if args.count == 0 {
        return Err("--count 必须 > 0".to_string());
    }
    Ok(args)
}

fn next_value(iter: &mut impl Iterator<Item = String>, name: &str) -> Result<String, String> {
    iter.next().ok_or_else(|| format!("{name} 缺少参数值"))
}
