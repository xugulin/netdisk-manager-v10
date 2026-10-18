//! 弹幕文本工具：XML 转义/反转义、ASS 标签剥离、内嵌换行处理。

/// XML 文本反转义（`&amp;` `&lt;` `&gt;` `&quot;` `&apos;` 与数字实体）。
pub fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(idx) = rest.find('&') {
        out.push_str(&rest[..idx]);
        let tail = &rest[idx..];
        // 只取前 12 字节做实体窗口；必须落在 char 边界上（CJK/emoji 会被切坏）
        let cut = tail
            .char_indices()
            .find(|(index, _)| *index >= 12)
            .map(|(index, _)| index)
            .unwrap_or(tail.len());
        let window = &tail[..cut];
        if let Some(semi) = window.find(';') {
            let entity = &tail[1..semi];
            let decoded = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
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

/// XML 文本转义。
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// 去掉 ASS override 块（`{\move(...)}`、`{\c&H...&}` 等）。
pub fn strip_ass_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(idx) = rest.find('{') {
        out.push_str(&rest[..idx]);
        let tail = &rest[idx..];
        match tail.find('}') {
            Some(end) => rest = &tail[end + 1..],
            None => {
                out.push('{');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// ASS 文本转义序列还原为单行文本：`\N`/`\n` → 空格，`\h` → 空格。
pub fn unescape_ass_text_single_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            Some('N') | Some('n') | Some('h') => {
                chars.next();
                out.push(' ');
            }
            _ => out.push('\\'),
        }
    }
    out
}

/// 清洗弹幕文本为可渲染单行：去 ASS 标签、还原转义、去首尾空白。
pub fn clean_danmaku_text(s: &str) -> String {
    let stripped = strip_ass_tags(s);
    let unescaped = unescape_ass_text_single_line(&stripped);
    unescaped.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_entity_followed_by_cjk_no_panic() {
        // `&amp; 转义` 的实体窗口 12 字节会切到 CJK 中间（历史 panic 回归）
        let decoded = super::xml_unescape("A &amp; 转义 与 &amp; 中文 🎉");
        assert_eq!(decoded, "A & 转义 与 & 中文 🎉");
    }

    #[test]
    fn xml_roundtrip() {
        let raw = "a < b & c > \"d\" 'e'";
        let escaped = xml_escape(raw);
        assert_eq!(escaped, "a &lt; b &amp; c &gt; &quot;d&quot; &apos;e&apos;");
        assert_eq!(xml_unescape(&escaped), raw);
        assert_eq!(xml_unescape("&#x4e2d;&#25991;"), "中文");
        assert_eq!(xml_unescape("1 & 2"), "1 & 2");
        assert_eq!(xml_unescape("&unknown;"), "&unknown;");
    }

    #[test]
    fn ass_tags_stripped() {
        assert_eq!(strip_ass_tags("{\\move(1,2,3,4)}文本"), "文本");
        assert_eq!(strip_ass_tags("a{\\c&HFFFFFF&}b"), "ab");
        assert_eq!(strip_ass_tags("未闭合{标签"), "未闭合{标签");
    }

    #[test]
    fn clean_single_line() {
        assert_eq!(
            clean_danmaku_text("{\\an8}第一行\\N第二行"),
            "第一行 第二行"
        );
        assert_eq!(clean_danmaku_text("  hi  "), "hi");
        assert_eq!(clean_danmaku_text(""), "");
    }
}
