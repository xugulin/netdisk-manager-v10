//! 分类浏览：`GET /api/categorylist`。
//!
//! 协议依据（`核心/接口/文件接口.py` `取分类列表` + 文档 §4.1）：
//! * query：`category=<1..6>&order=time&desc=1&num=100&page=1`，`带渠道=False`；
//! * 响应键是 **`info`**（不是 `/api/list` 的 `list`）；
//! * category：1 视频 / 2 音频 / 3 图片 / 4 文档 / 5 应用 / 6 其它；
//!   目录的 category 恒为 6，因此「其它」里会包含目录，属正常现象。
//!
//! `all` 百度没有对应枚举值，本模块用 1..=6 逐个拉取后拼接（不猜测
//! “category=0/全部”这种未见于 Python 源码与 HAR 的参数）。

use crate::files::entry_from_value;
use crate::http::ReqOpts;
use crate::BaiduDrive;
use serde_json::Value;
use wp_core::model::RemoteEntry;
use wp_core::{Error, Result};

/// 单页条数（Python 默认 100；服务端也接受 1000，这里取保守值）。
const PAGE_SIZE: usize = 100;
/// 单个分类最多翻 50 页 / 5000 条。
///
/// 实测（2026-10-18）：百度图片分类有 1 万+ 条，翻到第 103 页时服务端返回
/// `errno:31034`（疑似深度翻页/频控）。分类浏览是"浏览"而不是"全量导出"，
/// 因此这里主动设上限，且对 31034 走**优雅降级**（返回已拿到的页，不报错）。
const MAX_PAGES: u32 = 50;
const MAX_ENTRIES: usize = 5_000;
/// 页间隔：降低被服务端频控（31034）的概率。
const PAGE_DELAY_MS: u64 = 120;

/// 分类名 → 百度 category 数字；`all` 返回 `None`。
///
/// 除接口约定的英文名外，顺手接受 Python 源码里的中文名，方便 CLI 用户。
pub(crate) fn category_code(input: &str) -> Result<Option<u32>> {
    let key = input.trim().to_ascii_lowercase();
    let code = match key.as_str() {
        "all" | "全部" | "所有" => return Ok(None),
        "video" | "视频" => 1,
        "audio" | "音频" => 2,
        "image" | "picture" | "图片" => 3,
        "doc" | "document" | "文档" => 4,
        "app" | "application" | "应用" => 5,
        "other" | "其它" | "其他" => 6,
        "" => {
            return Err(Error::other(
                "分类不能为空：支持 video/audio/image/doc/app/other/all",
            ))
        }
        other => {
            return Err(Error::other(format!(
                "未知分类：{other:?}（支持 video/audio/image/doc/app/other/all）"
            )))
        }
    };
    Ok(Some(code))
}

/// 解析 `categorylist` 响应里的 `info` 数组。
pub(crate) fn entries_from_info(v: &Value) -> Vec<RemoteEntry> {
    v.get("info")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| entry_from_value(item, "/"))
                .collect()
        })
        .unwrap_or_default()
}

/// 拉取单个 category 的全部页。
async fn fetch_category(d: &BaiduDrive, code: u32) -> Result<Vec<RemoteEntry>> {
    let mut out = Vec::new();
    for page in 1..=MAX_PAGES {
        if out.len() >= MAX_ENTRIES {
            wp_core::log_warn!(
                "百度",
                "分类 {code} 达到 {MAX_ENTRIES} 条上限，结果可能不完整"
            );
            break;
        }
        if page > 1 {
            tokio::time::sleep(std::time::Duration::from_millis(PAGE_DELAY_MS)).await;
        }
        // 与 HAR 实测参数对齐：带 channel=chunlei + version=<毫秒>
        let params = vec![
            ("version".to_string(), crate::now_ms().to_string()),
            ("category".to_string(), code.to_string()),
            ("order".to_string(), "time".to_string()),
            ("desc".to_string(), "1".to_string()),
            ("num".to_string(), PAGE_SIZE.to_string()),
            ("page".to_string(), page.to_string()),
        ];
        let v = match d
            .api_get("/api/categorylist", &params, ReqOpts::authed())
            .await
        {
            Ok(v) => v,
            // 31034：深度翻页/频控。分类浏览降级为"返回已拿到的部分"。
            Err(Error::Server { code: 31034, .. }) => {
                wp_core::log_warn!(
                    "百度",
                    "分类 {code} 第 {page} 页被服务端限制（errno:31034），返回已取到的 {} 条",
                    out.len()
                );
                break;
            }
            Err(e) => return Err(e),
        };
        let Some(items) = v.get("info").and_then(Value::as_array) else {
            // Python 直接返回原始响应；这里把“没有 info”当作空结果，避免
            // 因服务端偶发缺字段就丢掉前面已拿到的页。
            break;
        };
        let count = items.len();
        out.extend(entries_from_info(&v));
        if count < PAGE_SIZE {
            break;
        }
    }
    Ok(out)
}

/// 按分类列全盘文件；`all` = 六个分类顺序拼接。
pub(crate) async fn list(d: &BaiduDrive, category: &str) -> Result<Vec<RemoteEntry>> {
    match category_code(category)? {
        Some(code) => {
            let out = fetch_category(d, code).await?;
            wp_core::log_debug!("百度", "分类 {category} → {} 项", out.len());
            Ok(out)
        }
        None => {
            let mut out = Vec::new();
            for code in 1..=6u32 {
                out.extend(fetch_category(d, code).await?);
            }
            wp_core::log_debug!("百度", "分类 all → {} 项", out.len());
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn category_names_map_to_baidu_codes() {
        assert_eq!(category_code("video").unwrap(), Some(1));
        assert_eq!(category_code(" AUDIO ").unwrap(), Some(2));
        assert_eq!(category_code("image").unwrap(), Some(3));
        assert_eq!(category_code("doc").unwrap(), Some(4));
        assert_eq!(category_code("app").unwrap(), Some(5));
        assert_eq!(category_code("other").unwrap(), Some(6));
        assert_eq!(category_code("全部").unwrap(), None);
        assert_eq!(category_code("all").unwrap(), None);
        assert!(category_code("music").is_err());
        assert!(category_code("").is_err());
    }

    #[test]
    fn category_info_mapping_uses_info_key_and_isdir() {
        let v = json!({
            "errno": 0,
            "info": [
                {
                    "fs_id": 411921386376817i64,
                    "server_filename": "看海.jpg",
                    "path": "/图片/看海.jpg",
                    "isdir": 0,
                    "size": 1234,
                    "server_mtime": 1789325603
                },
                {
                    "fs_id": 828379621778212i64,
                    "server_filename": "测试",
                    "path": "/测试",
                    "isdir": 1,
                    "size": 0,
                    "category": 6
                }
            ]
        });
        let entries = entries_from_info(&v);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "411921386376817");
        assert_eq!(entries[0].name, "看海.jpg");
        assert!(!entries[0].is_dir);
        assert_eq!(entries[0].size, 1234);
        assert!(entries[1].is_dir);
        // 目录 category=6 不能拿来判目录，这里确认仍按 isdir 判。
        assert_eq!(entries[1].name, "测试");

        // 没有 info 时返回空，而不是报错。
        assert!(entries_from_info(&json!({"errno": 0})).is_empty());
    }
}
