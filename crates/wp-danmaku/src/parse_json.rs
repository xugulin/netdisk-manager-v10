//! JSON 弹幕解析/导出。
//!
//! 兼容三类常见输入（宽松、字段名多别名）：
//! 1. 本 crate / 弹幕库导出：`{"source": "BilibiliXml", "comments": [{...}]}`（[`DanmakuDoc`] 序列化）
//! 2. B 站 API 风格：`{"data": {"elems": [{"progress": 1500, "mode": 1, "fontsize": 25, "color": 16777215, "content": "..."}]}}`
//! 3. 普通数组：`[{"time_ms": 1500, "text": "..."}]` 或 `[{"time": 1.5, "text": "..."}]`
//!
//! 字段别名：时间 `time_ms`/`start_ms`/`progress` 为毫秒，`time`/`t` 为**秒**（浮点）；
//! 模式 `mode` 接受 B 站整数或 `"Scroll"/"Top"/"Bottom"/"滚动"/"顶部"/"底部"`；
//! 颜色接受十进制整数或 `#RRGGBB`/`RRGGBB` 字符串；文本 `text`/`content`/`m`/`body`；
//! 用户 `user`/`midHash`/`uid`/`sender`。

use crate::model::{Comment, DanmakuMode, DanmakuSource};
use crate::text::xml_unescape;
use crate::time::seconds_to_ms;
use serde_json::Value;
use wp_core::{Error, Result};

/// 解析 JSON 弹幕（语法错误返回 Err，单条坏数据跳过）。
pub fn parse_json(json: &str) -> Result<crate::model::DanmakuDoc> {
    let value: Value =
        serde_json::from_str(json).map_err(|e| Error::other(format!("弹幕 JSON 解析失败：{e}")))?;
    let (source, comment_values) = match &value {
        Value::Array(items) => (DanmakuSource::LocalFile, items.clone()),
        Value::Object(map) => {
            if let Some(items) = map.get("comments").and_then(Value::as_array) {
                let source = map
                    .get("source")
                    .and_then(|v| serde_json::from_value::<DanmakuSource>(v.clone()).ok())
                    .unwrap_or(DanmakuSource::LocalFile);
                (source, items.clone())
            } else if let Some(data) = map.get("data").and_then(Value::as_object) {
                if let Some(items) = data.get("elems").and_then(Value::as_array) {
                    (DanmakuSource::BilibiliXml, items.clone())
                } else if let Some(items) = data.get("comments").and_then(Value::as_array) {
                    (DanmakuSource::LocalFile, items.clone())
                } else {
                    return Err(Error::other("弹幕 JSON 缺少 comments/elems 数组"));
                }
            } else if let Some(items) = map.get("elems").and_then(Value::as_array) {
                (DanmakuSource::BilibiliXml, items.clone())
            } else if let Some(items) = map
                .get("danmaku")
                .or_else(|| map.get("items"))
                .and_then(Value::as_array)
            {
                (DanmakuSource::LocalFile, items.clone())
            } else {
                return Err(Error::other("弹幕 JSON 缺少 comments/elems 数组"));
            }
        }
        _ => return Err(Error::other("弹幕 JSON 顶层必须是数组或对象")),
    };
    let mut comments: Vec<Comment> = comment_values
        .iter()
        .filter_map(parse_comment_value)
        .collect();
    comments.sort_by_key(|c| c.time_ms);
    Ok(crate::model::DanmakuDoc::new(source, comments))
}

/// 解析单条 JSON 弹幕对象（字段名宽松）。
pub fn parse_comment_value(value: &Value) -> Option<Comment> {
    let obj = value.as_object()?;
    // 文本
    let text = ["text", "content", "m", "body", "message"]
        .iter()
        .find_map(|k| obj.get(*k).and_then(Value::as_str))
        .map(xml_unescape)?
        .trim()
        .to_string();
    if text.is_empty() {
        return None;
    }
    // 时间：毫秒字段优先；`time`/`t` 视为秒
    let time_ms = if let Some(ms) = obj
        .get("time_ms")
        .or_else(|| obj.get("start_ms"))
        .or_else(|| obj.get("progress"))
        .and_then(Value::as_i64)
    {
        ms.max(0)
    } else {
        let secs = obj
            .get("time")
            .or_else(|| obj.get("t"))
            .and_then(Value::as_f64)?;
        seconds_to_ms(secs)
    };
    // 模式
    let mode = obj
        .get("mode")
        .and_then(parse_mode_value)
        .unwrap_or(DanmakuMode::Scroll);
    // 颜色
    let color = obj
        .get("color")
        .and_then(parse_color_value)
        .unwrap_or(0xFFFFFF);
    // 字号
    let font_size = ["font_size", "fontsize", "size"]
        .iter()
        .find_map(|k| obj.get(*k).and_then(Value::as_f64))
        .filter(|v| *v > 0.0)
        .map(|v| v as f32)
        .unwrap_or(25.0);
    // 用户
    let user = ["user", "midHash", "uid", "sender"]
        .iter()
        .find_map(|k| obj.get(*k))
        .and_then(value_to_string)
        .filter(|s| !s.is_empty() && s != "0");
    Some(Comment {
        time_ms,
        mode,
        color,
        font_size,
        text,
        user,
    })
}

fn parse_mode_value(value: &Value) -> Option<DanmakuMode> {
    if let Some(code) = value.as_i64() {
        return DanmakuMode::from_bilibili_code(code);
    }
    let s = value.as_str()?.trim().to_ascii_lowercase();
    match s.as_str() {
        "scroll" | "1" | "滚动" => Some(DanmakuMode::Scroll),
        "top" | "5" | "顶部" => Some(DanmakuMode::Top),
        "bottom" | "4" | "底部" => Some(DanmakuMode::Bottom),
        _ => None,
    }
}

fn parse_color_value(value: &Value) -> Option<u32> {
    if let Some(n) = value.as_u64() {
        return Some((n as u32) & 0xFFFFFF);
    }
    let s = value.as_str()?.trim();
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return u32::from_str_radix(hex, 16).ok();
    }
    s.parse::<u32>().ok().map(|n| n & 0xFFFFFF)
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// 导出 JSON（本 crate 格式；`parse_json` 可往返）。
pub fn export_json(doc: &crate::model::DanmakuDoc) -> Result<String> {
    serde_json::to_string_pretty(doc)
        .map_err(|e| Error::other(format!("弹幕 JSON 序列化失败：{e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DanmakuDoc;

    #[test]
    fn parse_bilibili_api_style() {
        let json = r#"{
  "code": 0,
  "data": {
    "elems": [
      {"progress": 1500, "mode": 1, "fontsize": 25, "color": 16777215, "midHash": "abc", "content": "滚动 &amp; 文本"},
      {"progress": 2500, "mode": 5, "fontsize": 30, "color": 16711680, "midHash": "def", "content": "顶部"},
      {"progress": 3500, "mode": 4, "fontsize": 18, "color": 255, "midHash": "0", "content": "底部"}
    ]
  }
}"#;
        let doc = parse_json(json).unwrap();
        assert_eq!(doc.source, DanmakuSource::BilibiliXml);
        assert_eq!(doc.comments.len(), 3);
        assert_eq!(doc.comments[0].time_ms, 1500);
        assert_eq!(doc.comments[0].text, "滚动 & 文本");
        assert_eq!(doc.comments[0].user.as_deref(), Some("abc"));
        assert_eq!(doc.comments[1].mode, DanmakuMode::Top);
        assert_eq!(doc.comments[1].color, 0xFF0000);
        assert_eq!(doc.comments[2].mode, DanmakuMode::Bottom);
    }

    #[test]
    fn parse_plain_array_with_seconds() {
        let json = r##"[
            {"time": 1.5, "text": "a", "color": "#FF0000", "mode": "scroll"},
            {"time_ms": 2500, "text": "b", "mode": 5, "size": 30},
            {"t": 3, "m": "c", "mode": "底部"}
        ]"##;
        let doc = parse_json(json).unwrap();
        assert_eq!(doc.source, DanmakuSource::LocalFile);
        assert_eq!(doc.comments.len(), 3);
        assert_eq!(doc.comments[0].time_ms, 1500);
        assert_eq!(doc.comments[0].color, 0xFF0000);
        assert_eq!(doc.comments[1].mode, DanmakuMode::Top);
        assert_eq!(doc.comments[1].font_size, 30.0);
        assert_eq!(doc.comments[2].mode, DanmakuMode::Bottom);
        assert_eq!(doc.comments[2].time_ms, 3000);
    }

    #[test]
    fn parse_bad_json_and_bad_items() {
        assert!(parse_json("not json").is_err());
        assert!(parse_json("42").is_err());
        assert!(parse_json(r#"{"foo": 1}"#).is_err());
        let json = r#"[{"no_time": 1, "text": "x"}, {"time": 1.0, "text": ""}, {"time": 2.0, "content": "ok"}]"#;
        let doc = parse_json(json).unwrap();
        assert_eq!(doc.comments.len(), 1);
        assert_eq!(doc.comments[0].text, "ok");
    }

    #[test]
    fn export_roundtrip() {
        let doc = DanmakuDoc::new(
            DanmakuSource::Generated,
            vec![
                Comment::new(1500, DanmakuMode::Scroll, "第一条").with_color(0xFF0000),
                Comment::new(2500, DanmakuMode::Top, "顶部"),
            ],
        );
        let json = export_json(&doc).unwrap();
        let back = parse_json(&json).unwrap();
        assert_eq!(back.source, DanmakuSource::Generated);
        assert_eq!(back.comments.len(), 2);
        for (a, b) in doc.comments.iter().zip(back.comments.iter()) {
            assert_eq!(a.time_ms, b.time_ms);
            assert_eq!(a.mode, b.mode);
            assert_eq!(a.color, b.color);
            assert_eq!(a.text, b.text);
        }
    }
}
