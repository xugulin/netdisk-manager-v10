//! 双语 ASS 注入辅助：生成内容 → 写临时文件 → 交给 mpv `sub-add` / `sub-reload`。
//!
//! 用 `std::env::temp_dir()` + 进程号 + 原子计数保证文件名唯一，避免多窗口/多次注入
//! 互相覆盖；不引入额外依赖（不强制 `tempfile` crate）。

use crate::ass::build_bilingual_ass_report_with_header;
use crate::model::{AssOptions, BilingualAss, BilingualLayout, Cue, SubtitleDoc};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use wp_core::{Error, Result};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 生成一个临时 `.ass` 路径（不创建文件）。`prefix` 用于排错识别。
pub fn temp_ass_path(prefix: &str) -> PathBuf {
    let safe: String = prefix
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(32)
        .collect();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let count = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "wp-subtitle-{}-{}-{}-{}.ass",
        std::process::id(),
        if safe.is_empty() { "bilingual" } else { &safe },
        nanos,
        count
    ))
}

/// 把 ASS 内容写到临时文件（UTF-8，无 BOM），返回路径。
pub fn write_temp_ass(content: &str, prefix: &str) -> Result<PathBuf> {
    let path = temp_ass_path(prefix);
    std::fs::write(&path, content.as_bytes())
        .map_err(|e| Error::other(format!("写临时字幕文件失败（{}）：{e}", path.display())))?;
    Ok(path)
}

/// 删掉临时字幕文件（不存在视为成功）。
pub fn cleanup_temp_ass(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::other(format!(
            "删除临时字幕文件失败（{}）：{e}",
            path.display()
        ))),
    }
}

/// 生成双语 ASS 并写入临时文件（AI 翻译注入主流程）。
pub fn write_bilingual_ass(
    original: &[Cue],
    translated: &[Cue],
    opts: &AssOptions,
    prefix: &str,
) -> Result<(PathBuf, BilingualAss)> {
    let report = build_bilingual_ass_report_with_header(original, translated, opts, None);
    let path = write_temp_ass(&report.content, prefix)?;
    Ok((path, report))
}

/// 从 `SubtitleDoc` 生成双语 ASS 并写入临时文件（保留原 ASS 头/样式）。
pub fn write_bilingual_ass_from_doc(
    doc: &SubtitleDoc,
    translated: &[Cue],
    opts: &AssOptions,
    prefix: &str,
) -> Result<(PathBuf, BilingualAss)> {
    let report = build_bilingual_ass_report_with_header(
        &doc.cues,
        translated,
        opts,
        doc.ass_header.as_deref(),
    );
    let path = write_temp_ass(&report.content, prefix)?;
    Ok((path, report))
}

/// 双轨布局的显示顺序说明（字幕面板/设置页可直接展示）。
pub fn layout_label(layout: BilingualLayout) -> &'static str {
    match layout {
        BilingualLayout::TranslatedBelow => "原文在上 / 译文在下",
        BilingualLayout::TranslatedAbove => "译文在上 / 原文在下",
        BilingualLayout::OnlyTranslated => "只显示译文",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(start: i64, end: i64, text: &str) -> Cue {
        Cue {
            start_ms: start,
            end_ms: end,
            text: text.into(),
            raw_style: None,
        }
    }

    #[test]
    fn temp_path_unique_and_safe() {
        let a = temp_ass_path("a/b c");
        let b = temp_ass_path("a/b c");
        assert_ne!(a, b);
        let name = a.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("wp-subtitle-"));
        assert!(name.ends_with(".ass"));
        assert!(name.chars().all(|c| c != '/'));
    }

    #[test]
    fn write_read_cleanup() {
        let path = write_temp_ass("[Script Info]\nScriptType: v4.00+\n", "unit-test").unwrap();
        assert!(path.exists());
        let read = std::fs::read_to_string(&path).unwrap();
        assert!(read.contains("ScriptType: v4.00+"));
        cleanup_temp_ass(&path).unwrap();
        assert!(!path.exists());
        // 重复删除不报错
        cleanup_temp_ass(&path).unwrap();
    }

    #[test]
    fn write_bilingual_ass_flow() {
        let originals = vec![cue(1000, 3000, "原文")];
        let translated = vec![cue(1000, 3000, "译文")];
        let (path, report) =
            write_bilingual_ass(&originals, &translated, &AssOptions::default(), "flow").unwrap();
        assert_eq!(report.matched, 1);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("Dialogue: 0,0:00:01.00,0:00:03.00,Default"));
        assert!(content.contains("Dialogue:"));
        cleanup_temp_ass(&path).unwrap();
    }

    #[test]
    fn layout_labels() {
        for layout in [
            BilingualLayout::TranslatedBelow,
            BilingualLayout::TranslatedAbove,
            BilingualLayout::OnlyTranslated,
        ] {
            assert!(!layout_label(layout).is_empty());
        }
    }
}
