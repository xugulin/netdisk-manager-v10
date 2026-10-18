//! 弹幕子系统（纯逻辑）：模型/解析/车道布局/ASS 生成/屏蔽过滤。
//!
//! 渲染在 GUI 侧：Mode A 用 egui 覆盖层（`wp-gui`），Mode B 用本 crate 生成的 ASS
//! 作为副字幕轨交给 mpv 渲染。任务 T22–T27。
//!
//! # 能力
//! - **解析/导出**：B 站 XML（往返）、JSON（B 站 API / 弹幕库 / 普通数组）、ASS 导入
//! - **布局引擎**：[`layout`] 滚动车道防追尾（[`scroll_overlap`] 几何判定）、顶部/底部
//!   固定车道、密度上限策略（[`DensityPolicy`]）、1080p/4K 相对坐标一致
//! - **ASS 生成**：[`generate_ass`] 滚动 `\move`、固定 `\pos`、颜色 `&HBBGGRR&`、
//!   字号按高度换算、透明度；[`generate_ass_windowed`] 时间窗裁剪（分片 `sub-reload`）
//! - **过滤**：[`DanmakuFilter`] 关键词/通配符/用户/类型屏蔽、重复折叠、密度曲线
//!
//! # 快速上手
//! ```
//! use wp_danmaku::{parse_bilibili_xml, export_bilibili_xml, layout, generate_ass, LayoutConfig};
//!
//! let xml = r#"<i><d p="1.5,1,25,16777215,0,0,user,1">你好</d></i>"#;
//! let doc = parse_bilibili_xml(xml).unwrap();
//! assert_eq!(doc.comments.len(), 1);
//! let xml2 = export_bilibili_xml(&doc);
//! assert!(xml2.contains("1.500,1,25,16777215"));
//!
//! let placed = layout(&doc.comments, &LayoutConfig::hd1080());
//! let ass = generate_ass(&placed, &LayoutConfig::hd1080());
//! assert!(ass.contains("\\move("));
//! ```

#![forbid(unsafe_code)]
#![deny(missing_debug_implementations)]

pub mod filter;
pub mod layout;
pub mod model;
pub mod overlay;
pub mod parse_ass_import;
pub mod parse_bilibili;
pub mod parse_json;
pub mod render_ass;
pub mod text;
pub mod time;

pub use filter::{dedupe_comments, density_curve, glob_match, DanmakuFilter, FilterReason};
pub use layout::{geometry, layout_with_policy, scroll_overlap, text_width_ratio, PlacedTimeline};
pub use model::{
    comment_weight, AssRenderOptions, Comment, DanmakuDoc, DanmakuMode, DanmakuSource,
    DensityPolicy, LayoutConfig, LayoutGeometry, LayoutStats, PlacedComment,
};
pub use overlay::{DanmakuGlyph, EguiGlyph, OverlayStyle, VideoRect};
pub use parse_ass_import::import_ass;
pub use parse_bilibili::{export_bilibili_xml, parse_bilibili_xml, parse_p_attr};
pub use parse_json::{export_json, parse_json};
pub use render_ass::{
    ass_color, escape_danmaku_text, export_ass, generate_ass_windowed, generate_ass_with_options,
    play_res,
};
pub use text::{
    clean_danmaku_text, strip_ass_tags, unescape_ass_text_single_line, xml_escape, xml_unescape,
};
pub use time::{format_ass_timestamp, ms_to_seconds_string, parse_ass_timestamp, seconds_to_ms};

use wp_core::{Error, Result};

/// 车道布局（T23）：默认密度策略（超限丢弃新弹幕），返回渲染用的相对坐标。
///
/// 见 [`PlacedComment`]：`y_ratio`/`width_ratio` 为相对视频宽高 0~1，
/// 1080p 与 4K 结果一致。
pub fn layout(comments: &[Comment], cfg: &LayoutConfig) -> Vec<PlacedComment> {
    layout::layout(comments, cfg)
}

/// 布局 + 统计（面板诊断用）。
pub fn layout_with_stats(
    comments: &[Comment],
    cfg: &LayoutConfig,
    policy: DensityPolicy,
) -> (Vec<PlacedComment>, LayoutStats) {
    layout::layout_with_policy(comments, cfg, policy)
}

/// 生成 ASS 弹幕轨（T25）：滚动 `\move`、固定 `\pos`、颜色/字号/透明度。
pub fn generate_ass(placed: &[PlacedComment], cfg: &LayoutConfig) -> String {
    render_ass::generate_ass(placed, cfg)
}

/// 猜测弹幕文件格式（本地同名 XML/JSON/ASS 自动加载用）。
pub fn guess_format(path: &str, content: &str) -> Option<DanmakuSource> {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "xml" => return Some(DanmakuSource::BilibiliXml),
        "json" | "jsonl" => return Some(DanmakuSource::LocalFile),
        "ass" | "ssa" => return Some(DanmakuSource::LocalFile),
        _ => {}
    }
    let head = content.trim_start();
    if head.starts_with("<?xml") || head.starts_with("<i") {
        Some(DanmakuSource::BilibiliXml)
    } else if head.starts_with('{')
        || head.starts_with('[')
        || (content.contains("[Events]") && content.contains("Dialogue:"))
    {
        Some(DanmakuSource::LocalFile)
    } else {
        None
    }
}

/// 按扩展名/内容自动解析弹幕文件（XML/JSON/ASS）。
pub fn parse_auto(path: Option<&str>, content: &str) -> Result<DanmakuDoc> {
    let path = path.unwrap_or("");
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "xml" => return parse_bilibili_xml(content),
        "json" | "jsonl" => return parse_json(content),
        "ass" | "ssa" => return import_ass(content),
        _ => {}
    }
    let head = content.trim_start();
    if head.starts_with("<?xml") || head.starts_with("<i") {
        parse_bilibili_xml(content)
    } else if head.starts_with('{') || head.starts_with('[') {
        parse_json(content)
    } else if content.contains("[Events]") && content.contains("Dialogue:") {
        import_ass(content)
    } else {
        Err(Error::unsupported("无法识别的弹幕文件格式"))
    }
}

/// 合并多份弹幕文档：按时间排序 + 可选时间窗去重。
pub fn merge_docs(docs: &[DanmakuDoc], dedupe_window_ms: Option<i64>) -> DanmakuDoc {
    let mut comments: Vec<Comment> = docs
        .iter()
        .flat_map(|doc| doc.comments.iter().cloned())
        .collect();
    comments.sort_by_key(|c| c.time_ms);
    if let Some(window) = dedupe_window_ms {
        comments = filter::dedupe_comments(&comments, window);
    }
    DanmakuDoc::from_comments(comments)
}

/// 规范化文档：负时间裁剪、字号/颜色合法化、按时间排序。
pub fn normalize_doc(doc: &mut DanmakuDoc) {
    doc.normalize();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_pipeline_xml_to_ass() {
        let xml = r#"<?xml version="1.0"?><i>
<d p="1.0,1,25,16777215,0,0,u1,1">滚动</d>
<d p="2.0,5,25,16711680,0,0,u2,2">顶部红</d>
<d p="3.0,4,25,255,0,0,0,3">底部</d>
</i>"#;
        let doc = parse_bilibili_xml(xml).unwrap();
        assert_eq!(doc.comments.len(), 3);
        let cfg = LayoutConfig::hd1080();
        let placed = layout(&doc.comments, &cfg);
        assert_eq!(placed.len(), 3);
        let ass = generate_ass(&placed, &cfg);
        assert!(ass.contains("\\move("));
        assert!(ass.contains("\\pos("));
        assert!(ass.contains("\\c&H0000FF&")); // 红
                                               // 导出 XML 再解析
        let xml2 = export_bilibili_xml(&doc);
        let doc2 = parse_bilibili_xml(&xml2).unwrap();
        assert_eq!(doc2.comments.len(), doc.comments.len());
    }

    #[test]
    fn auto_parse_by_extension_and_content() {
        let xml = r#"<i><d p="1.0,1,25,16777215,0,0,0,1">x</d></i>"#;
        assert_eq!(parse_auto(Some("a.xml"), xml).unwrap().comments.len(), 1);
        assert!(parse_auto(Some("a.bin"), xml).is_ok());
        let json = r#"[{"time_ms": 1000, "text": "j"}]"#;
        assert_eq!(parse_auto(Some("a.json"), json).unwrap().comments.len(), 1);
        let ass = "[Events]\nDialogue: 0,0:00:01.00,0:00:02.00,D,,0,0,0,,{\\move(1,1,2,2)}x\n";
        assert_eq!(parse_auto(Some("a.ass"), ass).unwrap().comments.len(), 1);
        assert!(parse_auto(Some("a.txt"), "garbage").is_err());
        assert_eq!(guess_format("a.xml", ""), Some(DanmakuSource::BilibiliXml));
        assert_eq!(guess_format("a.json", ""), Some(DanmakuSource::LocalFile));
        assert_eq!(guess_format("a.txt", "garbage"), None);
    }

    #[test]
    fn merge_and_dedupe_docs() {
        let a = DanmakuDoc::from_comments(vec![
            Comment::new(2000, DanmakuMode::Scroll, "重复"),
            Comment::new(1000, DanmakuMode::Scroll, "a"),
        ]);
        let b = DanmakuDoc::from_comments(vec![
            Comment::new(2500, DanmakuMode::Scroll, "重复"),
            Comment::new(3000, DanmakuMode::Scroll, "b"),
        ]);
        let merged = merge_docs(&[a.clone(), b.clone()], None);
        assert_eq!(merged.comments.len(), 4);
        // 按时间排序
        assert_eq!(merged.comments[0].time_ms, 1000);
        // 5s 窗口内“重复”折叠
        let merged = merge_docs(&[a, b], Some(5000));
        assert_eq!(merged.comments.len(), 3);
        assert!(merged
            .comments
            .iter()
            .all(|c| c.text != "重复" || c.time_ms == 2000));
    }

    #[test]
    fn filter_then_layout() {
        let comments = vec![
            Comment::new(0, DanmakuMode::Scroll, "正常"),
            Comment::new(0, DanmakuMode::Scroll, "广告内容"),
        ];
        let filter = DanmakuFilter {
            keywords: vec!["广告".into()],
            ..DanmakuFilter::new()
        };
        let kept = filter.apply(&comments);
        assert_eq!(kept.len(), 1);
        let placed = layout(&kept, &LayoutConfig::hd1080());
        assert_eq!(placed[0].comment.text, "正常");
    }

    #[test]
    fn normalize_doc_wrapper() {
        let mut doc = DanmakuDoc::from_comments(vec![
            Comment::new(2000, DanmakuMode::Scroll, "b"),
            Comment::new(1000, DanmakuMode::Scroll, "a"),
        ]);
        normalize_doc(&mut doc);
        assert_eq!(doc.comments[0].text, "a");
    }
}
