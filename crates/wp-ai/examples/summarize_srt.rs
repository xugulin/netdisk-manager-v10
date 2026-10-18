//! 示例：SRT 字幕 + MockProvider → 章节 JSON + 可读 Markdown（**不需要 API key**）。
//!
//! 运行：
//! ```text
//! cargo run -p wp-ai --example summarize_srt
//! ```
//!
//! 真实使用时把 `MockProvider` 换成 `build_provider_registry(&ai_config)`（远端 DeepSeek/OpenAI
//! 或本地 llama-server），其余调用完全相同。详见 `docs/PLAYER_AI.md` §P3。

use std::sync::Arc;

use wp_ai::cache::AiCache;
use wp_ai::mock::MockProvider;
use wp_ai::provider::ProviderRegistry;
use wp_ai::summarize::{summarize_subtitles, summary_to_markdown, SummarizeOptions};
use wp_ai::{AiConfig, ProviderConfig, ProviderKind};

/// 示例 SRT（约 12 分钟跨度，会切成 2 个 5 分钟窗口）。
const SAMPLE_SRT: &str = "\
1
00:00:01,000 --> 00:00:04,000
欢迎来到本片，我们从一个雨夜开始。

2
00:03:00,000 --> 00:03:05,000
主角在车站等一班永远不会来的列车。

3
00:04:59,000 --> 00:05:03,000
他决定沿着铁轨走下去。

4
00:08:00,000 --> 00:08:05,000
天亮时，他遇到了一位修车匠。

5
00:11:55,000 --> 00:12:00,000
修车匠告诉他：列车早已停运，但路还在。
";

#[tokio::main]
async fn main() -> wp_core::Result<()> {
    // 1) 解析 SRT（真实项目里来自外挂字幕/内嵌轨导出）
    let doc = wp_subtitle::parse(wp_subtitle::SubtitleFormat::Srt, SAMPLE_SRT)?;
    println!("解析到 {} 条字幕", doc.cues.len());

    // 2) Mock Provider：为 2 个窗口各注入一份 JSON 回复（真实使用换真实 Provider）
    let mock = Arc::new(MockProvider::new("mock-summarize").with_completions(vec![
        serde_json::json!({
            "chapters": [
                {"start_ms": 0, "end_ms": 300000, "title": "雨夜与车站", "summary": "主角在雨夜车站等待列车"}
            ],
            "overview": "一个雨夜，主角在车站等待一班不会来的列车。",
            "keywords": ["雨夜", "车站", "等待"]
        })
        .to_string(),
        serde_json::json!({
            "chapters": [
                {"start_ms": 300000, "end_ms": 480000, "title": "沿铁轨前行", "summary": "主角决定沿铁轨走向天亮"},
                {"start_ms": 480000, "end_ms": 720000, "title": "修车匠", "summary": "修车匠点明列车停运但路还在"}
            ],
            "overview": "主角沿铁轨前行，遇到修车匠后得到新的方向。",
            "keywords": ["铁轨", "修车匠", "方向"]
        })
        .to_string(),
    ]));
    let ai_config = AiConfig {
        providers: vec![ProviderConfig {
            id: "mock-summarize".into(),
            kind: ProviderKind::LocalLlm,
            enabled: true,
            priority: 10,
            ..ProviderConfig::default()
        }],
        offline_only: true,
        allow_during_playback: false,
    };
    let mut registry = ProviderRegistry::from_config(&ai_config);
    registry.register(mock);

    // 3) 缓存目录（真实项目用 数据/缓存/ai 以跨次复用；示例每次清空保证输出稳定可复制）
    let cache_dir = std::env::temp_dir().join("wp-ai-summarize-example");
    let _ = std::fs::remove_dir_all(&cache_dir);
    let cache = AiCache::new(cache_dir);

    // 4) 生成章节/摘要（Mock 下无需网络/密钥）
    let opts = SummarizeOptions::default(); // 5 分钟窗口、中文输出
    let outcome = summarize_subtitles(&registry, &cache, "example-fingerprint", &doc, &opts).await;

    println!("\n== 章节 JSON ==");
    println!("{}", outcome.summary.to_json_pretty()?);
    println!("== Markdown ==");
    println!("{}", summary_to_markdown(&outcome.summary));
    println!(
        "== 统计 == 窗口={} 章节={} Provider={:?} 缓存={} 降级={}",
        outcome.windows,
        outcome.chapter_count(),
        outcome.provider_id,
        outcome.cached,
        outcome.degraded
    );
    for warning in &outcome.warnings {
        println!("warning: {warning}");
    }
    for error in &outcome.errors {
        println!("error: {error}");
    }
    Ok(())
}
