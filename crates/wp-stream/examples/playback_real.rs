//! 真机播放源决策冒烟（M0-E1 / 播放集成验收辅助，非生产二进制）。
//!
//! 输入 `wp-cli link <远端路径>` 输出的 `DirectLink` JSON，走与 GUI 相同的
//! [`wp_stream::resolve_playback_with_provider`] 决策链路，输出：
//! * HLS：`{"kind":"hls","url":<m3u8>,"headers":[...]}`（不启动代理会话）；
//! * 普通文件：`{"kind":"proxied","url":"http://127.0.0.1:.../s/<token>",...}`。
//!
//! 输出给脚本/人工做后续 mpv 直连冒烟用（headers 含 Cookie，注意不要提交到仓库）。
//!
//! 用法：`cargo run -p wp-stream --example playback_real -- /tmp/link.json [/远端/路径]`

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use wp_core::model::DirectLink;
use wp_stream::{
    resolve_playback_with_provider, LinkProvider, PlaybackSource, StreamConfig, StreamProxy,
};

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
        return Err("用法：playback_real <direct-link.json> [remote_path]".into());
    };
    let remote_path = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "/remote/video.mp4".to_string());
    let text =
        std::fs::read_to_string(link_file).map_err(|e| format!("读 {link_file} 失败：{e}"))?;
    let link: DirectLink =
        serde_json::from_str(&text).map_err(|e| format!("解析直链 JSON 失败：{e}"))?;
    if link.url.trim().is_empty() {
        return Err("直链为空".into());
    }
    let is_hls_hint = link.is_hls;

    let cache_dir =
        std::env::temp_dir().join(format!("wp-stream-play-real-{}", std::process::id()));
    let proxy = StreamProxy::new(StreamConfig {
        cache_dir: cache_dir.clone(),
        readahead_bytes: 0,
        chunk_bytes: 256 * 1024,
        max_cache_bytes: 64 * 1024 * 1024,
        ..StreamConfig::default()
    });
    let provider = Arc::new(FixedProvider {
        link,
        cache_key: format!("playback-real:{}", Path::new(link_file).display()),
    });
    let source = resolve_playback_with_provider(&proxy, provider, &remote_path)
        .await
        .map_err(|e| format!("resolve_playback 失败：{e}"))?;

    let out = match &source {
        PlaybackSource::DirectHls {
            url,
            headers,
            display,
            size,
        } => serde_json::json!({
            "kind": "hls",
            "is_hls": true,
            "is_hls_hint_from_direct_link": is_hls_hint,
            "url": url,
            "display": display,
            "size": size,
            "headers": headers.iter().map(|(k, v)| serde_json::json!({"name": k, "value": v})).collect::<Vec<_>>(),
            "header_names": headers.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
            "proxy_sessions": proxy.session_count(),
        }),
        PlaybackSource::Proxied { handle } => {
            // 普通文件：再通过本地代理读 1KB，确认真的可播
            let mut probe = serde_json::json!({"status": null, "len": 0});
            match reqwest::Client::new()
                .get(&handle.url)
                .header("Range", "bytes=0-1023")
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let len = resp.bytes().await.map(|b| b.len()).unwrap_or(0);
                    probe = serde_json::json!({"status": status, "len": len});
                }
                Err(e) => {
                    probe = serde_json::json!({"error": e.to_string()});
                }
            }
            serde_json::json!({
                "kind": "proxied",
                "is_hls": false,
                "url": handle.url,
                "token": handle.token,
                "display": handle.display,
                "size": handle.total_bytes,
                "headers": [],
                "header_names": [],
                "probe": probe,
                "proxy_sessions": proxy.session_count(),
            })
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
    );

    if let Some(token) = source.token() {
        proxy.close(token);
    }
    proxy.shutdown().await;
    let _ = std::fs::remove_dir_all(&cache_dir);
    Ok(())
}
