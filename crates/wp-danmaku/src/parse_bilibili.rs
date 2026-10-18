//! B 站弹幕 XML 解析/导出。
//!
//! 格式：`<i><d p="time,mode,size,color,ts,pool,user,hash">text</d>...</i>`
//! - `time` 秒（浮点）→ 内部毫秒
//! - `mode`：1/2/3 滚动、4 底部、5 顶部、6 逆向滚动（降级为滚动）、7/8 高级/脚本弹幕跳过
//! - `size`：字号（以 1080p 为基准，默认 25）
//! - `color`：十进制 `0xRRGGBB`
//! - 文本需 XML 反转义（`&amp;` 等）

use crate::model::{Comment, DanmakuDoc, DanmakuMode, DanmakuSource};
use crate::text::{xml_escape, xml_unescape};
use crate::time::{ms_to_seconds_string, seconds_to_ms};
use wp_core::Result;

/// 解析 B 站 XML 弹幕。
///
/// 宽松解析：坏 `<d>` 行跳过；支持自闭合空弹幕；超出 8 个字段的多余字段忽略。
pub fn parse_bilibili_xml(xml: &str) -> Result<DanmakuDoc> {
    let mut comments: Vec<Comment> = Vec::new();
    let mut rest = xml;
    while let Some(open_rel) = rest.find("<d") {
        let after_open = &rest[open_rel..];
        // `<d` 后面必须是空白或 `>`，避免误匹配 `<data>` 之类
        let next = after_open.as_bytes().get(2).copied();
        let is_d_tag = matches!(next, Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'>'));
        if !is_d_tag {
            rest = &after_open[2..];
            continue;
        }
        let Some(tag_end_rel) = after_open.find('>') else {
            break;
        };
        let tag = &after_open[..tag_end_rel];
        let self_closing = tag.trim_end().ends_with('/');
        let p_attr = extract_attr(tag, "p");
        let content_start = open_rel + tag_end_rel + 1;
        let (text_raw, next_rest) = if self_closing {
            (String::new(), &rest[content_start..])
        } else {
            match rest[content_start..].find("</d>") {
                Some(close_rel) => (
                    rest[content_start..content_start + close_rel].to_string(),
                    &rest[content_start + close_rel + 4..],
                ),
                None => {
                    // 没有闭合标签：取到下一个 `<d` 或结尾
                    let tail = &rest[content_start..];
                    let stop = tail.find("<d").unwrap_or(tail.len());
                    (tail[..stop].to_string(), &rest[rest.len()..])
                }
            }
        };
        rest = next_rest;
        let Some(p) = p_attr else { continue };
        if let Some(comment) = parse_p_attr(&p, &text_raw) {
            comments.push(comment);
        }
    }
    comments.sort_by_key(|c| c.time_ms);
    Ok(DanmakuDoc::new(DanmakuSource::BilibiliXml, comments))
}

/// 解析单个 `p` 属性值 + 文本为弹幕。
pub fn parse_p_attr(p: &str, text_raw: &str) -> Option<Comment> {
    let fields: Vec<&str> = p.split(',').collect();
    if fields.len() < 4 {
        return None;
    }
    let time_sec: f64 = fields[0].trim().parse().ok()?;
    let mode_code: i64 = fields[1].trim().parse().ok()?;
    let mode = DanmakuMode::from_bilibili_code(mode_code)?;
    let size: f32 = fields[2]
        .trim()
        .parse()
        .ok()
        .filter(|v: &f32| v.is_finite() && *v > 0.0)
        .unwrap_or(25.0);
    let color: u32 = fields[3].trim().parse().unwrap_or(0xFFFFFF) & 0xFFFFFF;
    let user = fields
        .get(6)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && *s != "0")
        .map(|s| s.to_string());
    let text = xml_unescape(text_raw.trim());
    if text.is_empty() {
        return None;
    }
    Some(Comment {
        time_ms: seconds_to_ms(time_sec),
        mode,
        color,
        font_size: size,
        text,
        user,
    })
}

/// 导出 B 站 XML 弹幕（`parse_bilibili_xml(export(doc))` 可往返，时间精确到毫秒）。
pub fn export_bilibili_xml(doc: &DanmakuDoc) -> String {
    let mut out = String::with_capacity(doc.comments.len() * 96 + 256);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<i>\n");
    out.push_str("  <chatserver>chat.bilibili.com</chatserver>\n");
    out.push_str("  <chatid>0</chatid>\n");
    out.push_str("  <mission>0</mission>\n");
    out.push_str(&format!(
        "  <maxlimit>{}</maxlimit>\n",
        doc.comments.len().max(1000)
    ));
    out.push_str("  <state>0</state>\n");
    out.push_str("  <real_name>0</real_name>\n");
    out.push_str("  <source>k-v</source>\n");
    let mut sorted: Vec<&Comment> = doc.comments.iter().collect();
    sorted.sort_by_key(|c| c.time_ms);
    for (index, comment) in sorted.iter().enumerate() {
        let user = comment.user.as_deref().unwrap_or("0");
        let size = comment.font_size.round().max(1.0) as u32;
        out.push_str(&format!(
            "  <d p=\"{},{},{},{},0,0,{},{}\">{}</d>\n",
            ms_to_seconds_string(comment.time_ms),
            comment.mode.to_bilibili_code(),
            size,
            comment.color & 0xFFFFFF,
            xml_escape(user),
            index + 1,
            xml_escape(&comment.text),
        ));
    }
    out.push_str("</i>\n");
    out
}

/// 从 XML 标签里提取属性值（支持单双引号）。
fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let key = format!("{name}=");
    let idx = tag.find(&key)?;
    let after = &tag[idx + key.len()..];
    let mut chars = after.chars();
    let quote = chars.next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let value = &after[1..];
    let end = value.find(quote)?;
    Some(value[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<i>
  <chatserver>chat.bilibili.com</chatserver>
  <maxlimit>1000</maxlimit>
  <d p="1.5,1,25,16777215,1700000000,0,abc123,1001">第一条 &amp; 滚动</d>
  <d p="2.25,5,25,16711680,1700000001,0,def456,1002">顶部红色</d>
  <d p="3.0,4,18,255,1700000002,0,0,1003">底部小字</d>
  <d p="4.0,6,25,255,1700000003,0,0,1004">逆向滚动降级</d>
  <d p="5.0,7,25,255,1700000004,0,0,1005">高级弹幕跳过</d>
  <d p="6.0,1,25,16777215,1700000005,0,0,1006"/>
</i>
"#;

    #[test]
    fn parse_bilibili_sample() {
        let doc = parse_bilibili_xml(SAMPLE).unwrap();
        // 高级弹幕 + 空文本自闭合弹幕被跳过
        assert_eq!(doc.comments.len(), 4);
        assert_eq!(doc.source, DanmakuSource::BilibiliXml);
        assert_eq!(doc.comments[0].time_ms, 1500);
        assert_eq!(doc.comments[0].mode, DanmakuMode::Scroll);
        assert_eq!(doc.comments[0].color, 0xFFFFFF);
        assert_eq!(doc.comments[0].text, "第一条 & 滚动");
        assert_eq!(doc.comments[0].user.as_deref(), Some("abc123"));
        assert_eq!(doc.comments[1].mode, DanmakuMode::Top);
        assert_eq!(doc.comments[1].color, 0xFF0000);
        assert_eq!(doc.comments[2].mode, DanmakuMode::Bottom);
        assert_eq!(doc.comments[2].font_size, 18.0);
        // 逆向滚动降级为滚动
        assert_eq!(doc.comments[3].mode, DanmakuMode::Scroll);
        assert_eq!(doc.comments[3].time_ms, 4000);
    }

    #[test]
    fn parse_bilibili_empty_and_bad() {
        assert!(parse_bilibili_xml("").unwrap().is_empty());
        assert!(parse_bilibili_xml("<i></i>").unwrap().is_empty());
        let bad = r#"<i>
<d p="not-a-number,1,25,255,0,0,0,1">坏时间</d>
<d p="1.0">字段不足</d>
<d p="2.0,1,25,16777215,0,0,0,2">正常</d>
</i>"#;
        let doc = parse_bilibili_xml(bad).unwrap();
        assert_eq!(doc.comments.len(), 1);
        assert_eq!(doc.comments[0].text, "正常");
    }

    #[test]
    fn export_roundtrip() {
        let doc = parse_bilibili_xml(SAMPLE).unwrap();
        let xml = export_bilibili_xml(&doc);
        assert!(xml.starts_with("<?xml"));
        assert!(xml.contains("p=\"1.500,1,25,16777215,0,0,abc123,1\""));
        let back = parse_bilibili_xml(&xml).unwrap();
        assert_eq!(back.comments.len(), doc.comments.len());
        for (a, b) in doc.comments.iter().zip(back.comments.iter()) {
            assert_eq!(a.time_ms, b.time_ms);
            assert_eq!(a.mode, b.mode);
            assert_eq!(a.color, b.color);
            assert_eq!(a.font_size, b.font_size);
            assert_eq!(a.text, b.text);
            assert_eq!(a.user, b.user);
        }
    }

    #[test]
    fn export_escapes_special_chars() {
        let doc = DanmakuDoc::from_comments(vec![Comment::new(
            1234,
            DanmakuMode::Scroll,
            "1 < 2 & \"3\" > '4'",
        )]);
        let xml = export_bilibili_xml(&doc);
        let back = parse_bilibili_xml(&xml).unwrap();
        assert_eq!(back.comments[0].text, "1 < 2 & \"3\" > '4'");
        assert_eq!(back.comments[0].time_ms, 1234);
    }

    #[test]
    fn export_empty_doc() {
        let xml = export_bilibili_xml(&DanmakuDoc::default());
        assert!(xml.contains("</i>"));
        assert!(parse_bilibili_xml(&xml).unwrap().is_empty());
    }

    #[test]
    fn negative_time_clamped() {
        let xml = r#"<i><d p="-1.5,1,25,16777215,0,0,0,1">负时间</d></i>"#;
        let doc = parse_bilibili_xml(xml).unwrap();
        assert_eq!(doc.comments[0].time_ms, 0);
    }
}
