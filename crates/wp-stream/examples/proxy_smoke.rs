//! 真机代理冒烟工具（M0-E1 辅助验证，不属于生产二进制）。
//!
//! 输入是 `wp-cli link <远端路径>` 输出的 `DirectLink` JSON（含 url/headers/size），
//! 本工具用固定 `LinkProvider` 让 `StreamProxy` 走一遍真实链路：
//! 起本地代理 → GET 首段 Range → GET 中段 Range → 再 GET 首段（验证缓存命中）→ 打印统计。
//!
//! 用法：
//! ```text
//! wp-cli --drive baidu link /目录/视频.mp4 > /tmp/link.json
//! cargo run -p wp-stream --example proxy_smoke -- /tmp/link.json
//! ```
//!
//! 说明：只读探测（默认每段 1KB）；缓存目录用系统临时目录，退出时删除。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use wp_core::model::DirectLink;
use wp_stream::{LinkProvider, StreamConfig, StreamProxy};

/// 固定返回同一张直链的 provider（真机冒烟用）。
struct FixedProvider {
    link: DirectLink,
    cache_key: String,
}

#[async_trait]
impl LinkProvider for FixedProvider {
    fn cache_key(&self) -> String {
        self.cache_key.clone()
    }
    async fn direct_link(&self, _remote_path: &str) -> wp_core::Result<DirectLink> {
        Ok(self.link.clone())
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("ERROR: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(link_file) = args.first() else {
        return Err("用法：proxy_smoke <direct-link.json>".into());
    };
    let text =
        std::fs::read_to_string(link_file).map_err(|e| format!("读 {link_file} 失败：{e}"))?;
    let link: DirectLink =
        serde_json::from_str(&text).map_err(|e| format!("解析直链 JSON 失败：{e}"))?;
    if link.url.is_empty() {
        return Err("直链为空".into());
    }
    if link.is_hls {
        println!(
            "{}",
            serde_json::json!({"is_hls": true, "note": "HLS 直链不走 Range 代理，跳过"})
        );
        return Ok(());
    }
    let total = if link.size > 0 {
        link.size
    } else {
        return Err("直链 JSON 里没有 size，冒烟需要 size>0".into());
    };

    let cache_dir = std::env::temp_dir().join(format!("wp-stream-smoke-{}", std::process::id()));
    let cfg = StreamConfig {
        cache_dir: cache_dir.clone(),
        // 冒烟用小分块 + 关闭预读，避免下载大文件
        chunk_bytes: 256 * 1024,
        readahead_bytes: 0,
        max_cache_bytes: 256 * 1024 * 1024,
        parallel_fetches: 1,
        max_link_restarts: 3,
        request_timeout_secs: 30,
        connect_timeout_secs: 10,
    };
    let link_for_check = link.clone();
    let proxy = StreamProxy::new(cfg);
    let provider = Arc::new(FixedProvider {
        link,
        cache_key: format!("smoke:{}", PathBuf::from(link_file).display()),
    });
    let handle = proxy
        .open_with_provider(provider, "/smoke.mp4")
        .await
        .map_err(|e| format!("打开流失败：{e}"))?;
    if handle.direct {
        println!("{}", serde_json::json!({"direct": true, "url": handle.url}));
        return Ok(());
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| format!("构建 HTTP 客户端失败：{e}"))?;

    // 1) 首段
    let (first, _first_body) = fetch_range(&client, &handle.url, 0, 1023).await?;
    // 2) 中段（触发新块下载 + 定位读）；与直连同一段做字节级对照
    let mid_start = total / 2;
    let mid_end = (mid_start + 1023).min(total - 1);
    let (mid, mid_body) = fetch_range(&client, &handle.url, mid_start, mid_end).await?;
    let mid_matches_upstream =
        match fetch_direct(&client, &link_for_check, mid_start, mid_end).await {
            Ok(direct_body) => direct_body == mid_body,
            Err(_) => false,
        };
    // 3) 再取首段：应命中磁盘缓存（不再请求上游）
    let stats_before = proxy.stats(&handle.token);
    let (first_again, _) = fetch_range(&client, &handle.url, 0, 1023).await?;
    let stats_after = proxy.stats(&handle.token);

    let cache_hit = stats_before
        .as_ref()
        .zip(stats_after.as_ref())
        .map(|(a, b)| a.downloaded_bytes == b.downloaded_bytes)
        .unwrap_or(false);
    let stats = stats_after.unwrap_or_default();

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "url": handle.url,
            "direct": handle.direct,
            "display": handle.display,
            "total_bytes": handle.total_bytes,
            "first": first,
            "mid": mid,
            "first_again": first_again,
            "mid_body_matches_upstream": mid_matches_upstream,
            "cache_hit_on_repeat": cache_hit,
            "stats": {
                "buffered_bytes": stats.buffered_bytes,
                "downloaded_bytes": stats.downloaded_bytes,
                "bitrate_bps": stats.bitrate_bps,
                "upstream_restarts": stats.upstream_restarts,
                "errors": stats.errors,
                "complete": stats.complete,
            }
        }))
        .unwrap_or_default()
    );

    proxy.close(&handle.token);
    proxy.shutdown().await;
    let _ = std::fs::remove_dir_all(&cache_dir);
    Ok(())
}

async fn fetch_range(
    client: &reqwest::Client,
    url: &str,
    start: u64,
    end: u64,
) -> Result<(serde_json::Value, Vec<u8>), String> {
    let resp = client
        .get(url)
        .header("Range", format!("bytes={start}-{end}"))
        .send()
        .await
        .map_err(|e| format!("请求代理失败：{e}"))?;
    let status = resp.status().as_u16();
    let content_range = resp
        .headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("读响应体失败：{e}"))?;
    let meta = serde_json::json!({
        "status": status,
        "content_range": content_range,
        "len": body.len(),
    });
    Ok((meta, body.to_vec()))
}

/// 绕过代理、直接向直链请求同一段（用于对照代理返回的字节是否正确）。
async fn fetch_direct(
    client: &reqwest::Client,
    link: &DirectLink,
    start: u64,
    end: u64,
) -> Result<Vec<u8>, String> {
    let mut req = client
        .get(&link.url)
        .header("Range", format!("bytes={start}-{end}"));
    for (k, v) in &link.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.map_err(|e| format!("直连请求失败：{e}"))?;
    if resp.status().as_u16() != 206 {
        return Err(format!("直连未返回 206：{}", resp.status()));
    }
    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("直连读体失败：{e}"))?;
    Ok(body.to_vec())
}
