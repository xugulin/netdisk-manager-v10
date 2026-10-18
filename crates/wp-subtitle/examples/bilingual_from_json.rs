//! 双语字幕注入链路示例：原文 SRT（或 VTT/ASS）+ 译文 JSON/逐行文本 → 双语 ASS。
//!
//! 模拟 wp-ai 翻译完成后的注入流程（整片翻译 → 双语 ASS → `sub-add`/`sub-reload`）：
//! ```text
//! 原文 SRT ──parse──► Cue[] ──翻译(外部/本示例用 JSON 数组)──► 译文 Cue[]
//!        └──────────── build_bilingual_ass_from_doc ───────────► 双语 ASS
//! ```
//!
//! 运行（无参数用内置示例，直接打印 ASS；给 --out 则写文件）：
//! ```text
//! cargo run -p wp-subtitle --example bilingual_from_json
//! cargo run -p wp-subtitle --example bilingual_from_json -- \
//!     --srt 视频.srt --translations 译文.json --out /tmp/双语.ass
//! cargo run -p wp-subtitle --example bilingual_from_json -- --layout above --margin 60
//! ```
//!
//! 译文 JSON 支持：
//! - `["译文1", "译文2", ...]`
//! - `[{"text": "译文1"}, {"translation": "译文2"}, ...]`
//! - `{"translations": ["译文1", ...]}`
//! - 非 JSON（.txt 等）：按行读取，一行一条译文（空行忽略）

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use wp_subtitle::{build_bilingual_ass_from_doc, parse_auto, AssOptions, BilingualLayout, Cue};

const SAMPLE_SRT: &str = "1\n\
00:00:01,000 --> 00:00:03,500\n\
你好，世界\n\
\n\
2\n\
00:00:04,000 --> 00:00:06,000\n\
这是第二句字幕\n\
\n\
3\n\
00:00:07,000 --> 00:00:09,000\n\
第三句：支持多行\n\
第二行内容\n";

const SAMPLE_JSON: &str = r#"["Hello, world", "This is the second subtitle line", "Third line: multiline is supported\nsecond line content"]"#;

const USAGE: &str = "\
用法：cargo run -p wp-subtitle --example bilingual_from_json -- [选项]

无参数时使用内置示例（打印双语 ASS 到 stdout）。

选项：
  --srt PATH            原文字幕路径（.srt/.vtt/.ass/.ssa，默认内置示例 SRT）
  --translations PATH   译文 JSON 数组或逐行文本（默认内置示例）
  --out PATH            输出双语 ASS 路径（省略则打印到 stdout）
  --layout below|above|only
                        双语布局：原文上译文下（默认）/ 译文上原文下 / 只显示译文
  --margin N            底部边距 MarginV（默认 40）
  --font-size N         ASS 字号（默认 48）
  --help                显示本帮助";

struct Args {
    srt: Option<PathBuf>,
    translations: Option<PathBuf>,
    out: Option<PathBuf>,
    layout: BilingualLayout,
    margin_v: i32,
    font_size: u32,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            srt: None,
            translations: None,
            out: None,
            layout: BilingualLayout::TranslatedBelow,
            margin_v: 40,
            font_size: 48,
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
    // 1) 读原文
    let (source_text, source_name) = match &args.srt {
        Some(path) => (
            fs::read_to_string(path).map_err(|e| format!("读原文失败 {}：{e}", path.display()))?,
            path.display().to_string(),
        ),
        None => (SAMPLE_SRT.to_string(), "内置示例.srt".to_string()),
    };
    let doc =
        parse_auto(Some(&source_name), &source_text).map_err(|e| format!("解析原文失败：{e}"))?;
    if doc.cues.is_empty() {
        return Err("原文解析结果没有任何字幕 cue".to_string());
    }

    // 2) 读译文（真实链路里来自 wp-ai 的翻译结果）
    let (trans_text, trans_name) = match &args.translations {
        Some(path) => (
            fs::read_to_string(path).map_err(|e| format!("读译文失败 {}：{e}", path.display()))?,
            path.display().to_string(),
        ),
        None => (SAMPLE_JSON.to_string(), "内置示例.json".to_string()),
    };
    let translations =
        load_translations(&trans_text).map_err(|e| format!("解析译文（{trans_name}）失败：{e}"))?;

    // 3) 译文 → Cue（时间轴沿用原文，条数不齐时截断，缺失条目不输出译文）
    let translated: Vec<Cue> = doc
        .cues
        .iter()
        .zip(translations.iter())
        .map(|(cue, text)| Cue {
            start_ms: cue.start_ms,
            end_ms: cue.end_ms,
            text: text.clone(),
            raw_style: None,
        })
        .collect();

    // 4) 生成双语 ASS（保留原 ASS 头/样式；SRT 输入则用默认样式表）
    let opts = AssOptions {
        layout: args.layout,
        margin_v: args.margin_v,
        font_size: args.font_size,
        ..AssOptions::default()
    };
    let report = build_bilingual_ass_from_doc(&doc, &translated, &opts);
    let summary = format!(
        "原文 {} 条 / 译文 {} 条 → 配对 {} 条，漏译 {} 条（布局：{}）",
        doc.cues.len(),
        translations.len(),
        report.matched,
        report.missing,
        layout_label(args.layout)
    );
    match &args.out {
        Some(path) => {
            write_file(path, &report.content)?;
            println!("{summary}\n已写双语 ASS：{}", path.display());
        }
        None => {
            eprintln!("{summary}");
            print!("{}", report.content);
        }
    }
    Ok(())
}

fn layout_label(layout: BilingualLayout) -> &'static str {
    match layout {
        BilingualLayout::TranslatedBelow => "原文上/译文下",
        BilingualLayout::TranslatedAbove => "译文上/原文下",
        BilingualLayout::OnlyTranslated => "只显示译文",
    }
}

/// 解析译文：JSON 数组（字符串或对象）优先，否则按行读取。
fn load_translations(text: &str) -> Result<Vec<String>, String> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        let items = match value {
            serde_json::Value::Array(items) => items,
            serde_json::Value::Object(map) => map
                .get("translations")
                .or_else(|| map.get("texts"))
                .or_else(|| map.get("translated"))
                .and_then(|v| v.as_array())
                .cloned()
                .ok_or_else(|| "JSON 对象需包含 translations/texts 数组".to_string())?,
            _ => return Err("译文 JSON 顶层必须是数组或对象".to_string()),
        };
        let lines: Vec<String> = items
            .iter()
            .filter_map(|item| match item {
                serde_json::Value::String(text) => Some(text.clone()),
                serde_json::Value::Object(obj) => obj
                    .get("text")
                    .or_else(|| obj.get("translation"))
                    .or_else(|| obj.get("translated"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                _ => None,
            })
            .collect();
        if lines.is_empty() {
            return Err("译文 JSON 数组为空或没有可识别的文本字段".to_string());
        }
        return Ok(lines);
    }
    // 逐行文本（一行一条）
    let lines: Vec<String> = text
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        Err("译文为空".to_string())
    } else {
        Ok(lines)
    }
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

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut iter = env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "--srt" => args.srt = Some(PathBuf::from(next_value(&mut iter, "--srt")?)),
            "--translations" | "--json" | "--translation" => {
                args.translations = Some(PathBuf::from(next_value(&mut iter, "--translations")?))
            }
            "--out" => args.out = Some(PathBuf::from(next_value(&mut iter, "--out")?)),
            "--layout" => {
                args.layout = match next_value(&mut iter, "--layout")?
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "below" | "translated-below" | "原文上" => BilingualLayout::TranslatedBelow,
                    "above" | "translated-above" | "译文上" => BilingualLayout::TranslatedAbove,
                    "only" | "only-translated" | "只译文" => BilingualLayout::OnlyTranslated,
                    other => return Err(format!("--layout 只支持 below|above|only，收到 {other}")),
                }
            }
            "--margin" => {
                args.margin_v = next_value(&mut iter, "--margin")?
                    .parse()
                    .map_err(|_| "--margin 需要整数".to_string())?;
            }
            "--font-size" => {
                args.font_size = next_value(&mut iter, "--font-size")?
                    .parse()
                    .map_err(|_| "--font-size 需要正整数".to_string())?;
            }
            other => return Err(format!("未知参数：{other}")),
        }
    }
    Ok(args)
}

fn next_value(iter: &mut impl Iterator<Item = String>, name: &str) -> Result<String, String> {
    iter.next().ok_or_else(|| format!("{name} 缺少参数值"))
}
