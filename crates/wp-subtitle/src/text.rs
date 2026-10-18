//! 文本清洗与转义：HTML/VTT 标签、ASS override 块、ASS 转义换行、XML 实体。
//!
//! 设计原则：**解析时「去特效保留含义」**（换行保留为 `\n`，普通空格保留），
//! **生成时「重新转义」**（`\n` → `\N`，`{`/`}` → 全角，避免被 libass 当成标签）。

/// 统一换行（去掉 UTF-8 BOM，`\r\n`/`\r` → `\n`）。
pub fn normalize_newlines(text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// 解码 XML/HTML 命名实体与数字实体（`&amp;` `&#39;` `&#x4e2d;` 等）。
pub fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(idx) = rest.find('&') {
        out.push_str(&rest[..idx]);
        let tail = &rest[idx..];
        // 实体最多 ~12 字符，避免把正文里孤立的 & 当实体
        let window = &tail[..tail.len().min(12)];
        if let Some(semi) = window.find(';') {
            let entity = &tail[1..semi];
            let decoded = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                    u32::from_str_radix(&entity[2..], 16)
                        .ok()
                        .and_then(char::from_u32)
                }
                _ if entity.starts_with('#') => {
                    entity[1..].parse::<u32>().ok().and_then(char::from_u32)
                }
                _ => None,
            };
            match decoded {
                Some(ch) => {
                    out.push(ch);
                    rest = &tail[semi + 1..];
                }
                None => {
                    // 未知实体原样保留
                    out.push('&');
                    out.push_str(&tail[1..semi + 1]);
                    rest = &tail[semi + 1..];
                }
            }
        } else {
            out.push('&');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

/// 去掉 HTML/VTT 标签（`<i>`、`<font color=...>`、`<c.yellow>`、`<v Speaker>` 等）。
///
/// `<br>` / `<br/>` / `</br>` 转为换行；未闭合的 `<` 原样保留（不吞正文）。
pub fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(idx) = rest.find('<') {
        out.push_str(&rest[..idx]);
        let tail = &rest[idx..];
        match tail.find('>') {
            Some(end) => {
                let tag = &tail[1..end];
                let tag_name = tag
                    .trim()
                    .trim_matches('/')
                    .split(['.', ' ', '\t'])
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if tag_name == "br" {
                    out.push('\n');
                }
                rest = &tail[end + 1..];
            }
            None => {
                // 没有闭合的 '>'：保留 '<' 及后面的文本
                out.push('<');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 去掉 ASS override 块（`{\pos(...)}`、`{\an8}` 等，含不闭合时吞到行尾）。
pub fn strip_override_blocks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(idx) = rest.find('{') {
        out.push_str(&rest[..idx]);
        let tail = &rest[idx..];
        match tail.find('}') {
            Some(end) => rest = &tail[end + 1..],
            None => {
                // 不闭合的 { 视为普通字符
                out.push('{');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// ASS 转义序列还原：`\N`/`\n` → 换行，`\h` → 空格，其它 `\x` 原样保留。
pub fn unescape_ass_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            Some('N') | Some('n') => {
                chars.next();
                out.push('\n');
            }
            Some('h') => {
                chars.next();
                out.push(' ');
            }
            _ => out.push('\\'),
        }
    }
    out
}

/// 生成 ASS 文本转义：换行 → `\N`；`{`/`}` → 全角（libass 会吞掉 override 块内的字符）。
pub fn escape_ass_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '\n' => out.push_str("\\N"),
            '\r' => {}
            '{' => out.push('｛'),
            '}' => out.push('｝'),
            _ => out.push(ch),
        }
    }
    out
}

/// 完整清洗（解析入口用）：HTML 标签 + ASS override + ASS 转义 + 实体。
///
/// 顺序：先 HTML（`<font>` 里可能含 `{}` 吗？极少），再 override，最后反转义与实体，
/// 这样 `&amp;` 不会被二次处理。
pub fn strip_effects(s: &str) -> String {
    let s = strip_override_blocks(s);
    let s = unescape_ass_text(&s);
    let s = strip_html_tags(&s);
    decode_entities(&s)
}

/// 拆出文本开头的连续 override 块（ASS 原文样式前缀）与剩余文本。
///
/// 例：`"{\an8}{\pos(10,20)}你好"` → `(Some("{\an8}{\pos(10,20)}"), "你好")`
pub fn split_leading_override(s: &str) -> (Option<String>, String) {
    let mut rest = s;
    let mut tags = String::new();
    loop {
        let trimmed = rest.trim_start();
        if !trimmed.starts_with('{') {
            rest = trimmed;
            break;
        }
        match trimmed.find('}') {
            Some(end) => {
                tags.push_str(&trimmed[..=end]);
                rest = &trimmed[end + 1..];
            }
            None => {
                rest = trimmed;
                break;
            }
        }
    }
    let raw = if tags.is_empty() { None } else { Some(tags) };
    (raw, rest.trim_start().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newlines_and_bom() {
        assert_eq!(normalize_newlines("\u{feff}a\r\nb\rc"), "a\nb\nc");
    }

    #[test]
    fn entities_decoded() {
        assert_eq!(decode_entities("a&amp;b"), "a&b");
        assert_eq!(decode_entities("&lt;tag&gt;"), "<tag>");
        assert_eq!(decode_entities("&#39;q&#39;"), "'q'");
        assert_eq!(decode_entities("&#x4e2d;"), "中");
        assert_eq!(decode_entities("1 & 2"), "1 & 2");
        assert_eq!(decode_entities("&unknown;"), "&unknown;");
    }

    #[test]
    fn html_stripped_br_to_newline() {
        assert_eq!(strip_html_tags("<i>斜体</i>正常"), "斜体正常");
        assert_eq!(strip_html_tags("<font color=\"#fff\">彩色</font>"), "彩色");
        assert_eq!(strip_html_tags("第一行<br/>第二行"), "第一行\n第二行");
        assert_eq!(strip_html_tags("<v Speaker>hello"), "hello");
        assert_eq!(strip_html_tags("a < b"), "a < b");
    }

    #[test]
    fn override_blocks_stripped() {
        assert_eq!(strip_override_blocks("{\\an8}顶部"), "顶部");
        assert_eq!(strip_override_blocks("{\\pos(1,2)}x{\\b1}y"), "xy");
        assert_eq!(strip_override_blocks("正常{未闭合"), "正常{未闭合");
    }

    #[test]
    fn ass_escape_roundtrip() {
        let plain = "第一行\n第二行";
        let escaped = escape_ass_text(plain);
        assert_eq!(escaped, "第一行\\N第二行");
        assert_eq!(unescape_ass_text(&escaped), plain);
        assert_eq!(unescape_ass_text("a\\hb\\Nc"), "a b\nc");
        assert_eq!(escape_ass_text("{tag}"), "｛tag｝");
    }

    #[test]
    fn strip_effects_mixed() {
        let input = "{\\an8}<i>Hello</i> &amp; 你好\\Nsecond &lt;3";
        assert_eq!(strip_effects(input), "Hello & 你好\nsecond <3");
    }

    #[test]
    fn leading_override_split() {
        let (raw, rest) = split_leading_override("{\\an8}{\\pos(1,2)}你好\n世界");
        assert_eq!(raw.as_deref(), Some("{\\an8}{\\pos(1,2)}"));
        assert_eq!(rest, "你好\n世界");
        let (raw, rest) = split_leading_override("没有样式");
        assert_eq!(raw, None);
        assert_eq!(rest, "没有样式");
    }
}
