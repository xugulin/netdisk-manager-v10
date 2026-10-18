//! 云添加（离线下载）任务：创建（`add_task`）+ 列表（`list_task`）。
//!
//! ## 创建任务（真机反推，2026-10-18，真实 Cookie 会话）
//!
//! 以下请求形态已**通过服务端参数校验**：
//! ```text
//! POST /rest/2.0/services/cloud_dl
//!     ?channel=chunlei&clienttype=0&web=1&app_id=250528&bdstoken=<token>
//!     &method=add_task
//! form: source_url=<urlencoded 直链>&save_path=/目录
//!       [&task_name=<可选>][&od_type=4（仅 magnet/BT）]
//! ```
//! 真机返回（账号超容，卡在空间检查 —— 恰好证明参数形态被接受）：
//! `{"error_code":36009,"error_msg":"storage exceed limit",
//!   "show_msg":"百度网盘空间不足", ...}`
//! 成功时预期 `{"error_code":0,"task_id":<数字>...}`（HAR 的 list_task
//! `task_info` 里可见 `task_id`）。
//!
//! ⚠️ **该接口的错误字段是 `error_code` / `error_msg` / `show_msg`，不是 `errno`！**
//! 所以不能依赖 `api_get` 的通用 errno 检查；本模块自己解析 `error_code` 并映射
//! 中文文案。普通 http 任务 `od_type=0`，可省略；magnet/BT 任务 HAR 里是 `od_type=4`。
//!
//! ## 任务列表
//!
//! 列表请求完全按 HAR 实测参数还原：
//! ```text
//! GET /rest/2.0/services/cloud_dl
//!     ?start=0&limit=100&need_task_info=1&status=255
//!     &method=list_task&t=<毫秒>&bdstoken=...
//! ```
//! 响应 `task_info` 为数组，元素字段：
//! `task_id / task_name / source_url / save_path / status / create_time / od_type`。
//!
//! ⚠️ 百度任务状态语义与 `wp_core::model::cloud_status_text()` 的**通用表相反**：
//! 百度 `0 = 已完成`，而通用表把 `"0"` 当作「等待中」。HAR 的 `query_task`
//! 样本可交叉验证：所有 `status=0` 的任务 `finished_size == file_size` 且
//! `finish_time > 0`；`status=4` 的任务 `finished_size=0`。所以这里用百度自己的
//! 映射，不能直接套通用函数。

use crate::files;
use crate::http::ReqOpts;
use crate::{err_text, now_ms, BaiduDrive};
use serde_json::Value;
use url::Url;
use wp_core::model::CloudTask;
use wp_core::util::{base_name, file_name_from_url, norm_path};
use wp_core::{Error, Result};

/// 离线下载接口路径（创建与列表共用）。
const CLOUD_DL: &str = "/rest/2.0/services/cloud_dl";

/// 单页任务数（HAR 为 100）。
const PAGE_SIZE: usize = 100;
/// 最多翻 10 页（1000 个任务），避免异常响应导致死循环。
const MAX_PAGES: usize = 10;

/// 任务 id / 名称等字段：字符串或数字都接受。
fn field_text(v: &Value, keys: &[&str]) -> String {
    for key in keys {
        let Some(raw) = v.get(*key) else { continue };
        if let Some(s) = raw.as_str() {
            let s = s.trim();
            if !s.is_empty() {
                return s.to_string();
            }
        } else if let Some(n) = raw.as_i64() {
            return n.to_string();
        } else if let Some(n) = raw.as_u64() {
            return n.to_string();
        }
    }
    String::new()
}

/// 宽松数字解析：HAR 里 `file_size` / `finished_size` 是字符串。
fn loose_u64(v: &Value, keys: &[&str]) -> u64 {
    for key in keys {
        let Some(raw) = v.get(*key) else { continue };
        if let Some(n) = raw.as_u64() {
            return n;
        }
        if let Some(n) = raw.as_i64() {
            return n.max(0) as u64;
        }
        if let Some(s) = raw.as_str() {
            if let Ok(n) = s.trim().parse::<u64>() {
                return n;
            }
        }
    }
    0
}

fn loose_i64(v: &Value, keys: &[&str]) -> Option<i64> {
    for key in keys {
        let Some(raw) = v.get(*key) else { continue };
        if let Some(n) = raw.as_i64() {
            return Some(n);
        }
        if let Some(s) = raw.as_str() {
            if let Ok(n) = s.trim().parse::<i64>() {
                return Some(n);
            }
        }
    }
    None
}

fn loose_f64(v: &Value, key: &str) -> Option<f64> {
    let raw = v.get(key)?;
    if let Some(n) = raw.as_f64() {
        return Some(n);
    }
    raw.as_str().and_then(|s| s.trim().parse::<f64>().ok())
}

/// 百度任务状态 → 中文。
///
/// 0..=4 依百度云离线下载的公开状态约定；HAR 样本验证了 0（完成）与
/// 4（未开始下载）。未知值原样带回，方便真机联调时发现新状态。
pub(crate) fn status_text(code: &str) -> String {
    match code.trim() {
        "0" => "已完成".into(),
        "1" => "失败".into(),
        "2" => "已取消".into(),
        "3" => "下载中".into(),
        "4" => "等待中".into(),
        "" => "未知".into(),
        other => format!("状态 {other}"),
    }
}

/// 单个 `task_info` 元素 → 统一模型。
///
/// `fallback_id` 用于 `query_task` 那种「以 task_id 为键的对象」响应。
pub(crate) fn task_from_value(v: &Value, fallback_id: Option<&str>) -> Option<CloudTask> {
    let id = {
        let text = field_text(v, &["task_id", "taskId", "id"]);
        if text.is_empty() {
            fallback_id.unwrap_or("").trim().to_string()
        } else {
            text
        }
    };
    if id.is_empty() {
        return None;
    }
    let status = field_text(v, &["status"]);
    let save_path = field_text(v, &["save_path", "savePath", "path"]);
    let url = field_text(v, &["source_url", "sourceUrl", "url"]);
    let mut name = field_text(v, &["task_name", "taskName", "name", "server_filename"]);
    if name.is_empty() {
        name = base_name(&save_path);
    }
    if name.is_empty() {
        name = format!("任务 {id}");
    }

    let total = loose_u64(v, &["file_size", "fileSize", "total", "size"]);
    let done = loose_u64(v, &["finished_size", "finishedSize", "done", "downloaded"]);
    let progress = match loose_f64(v, "progress") {
        Some(raw) => {
            // query_task 的 progress 是 0..100；本机没有见过 0..1 的写法，
            // 但兼容一下，避免把 50 当成 5000%。
            if raw > 1.0 {
                raw / 100.0
            } else {
                raw
            }
        }
        None if total > 0 => done as f64 / total as f64,
        None => 0.0,
    }
    .clamp(0.0, 1.0) as f32;

    let created = loose_i64(v, &["create_time", "createTime", "ctime"]).filter(|t| *t > 0);
    let error = {
        let text = err_text(v, "");
        // err_text 在没有错误字段时会回退空串；只有真的写了 message 才展示，
        // 避免把正常任务的 show_msg="success" 当错误。
        if text.is_empty() || text.eq_ignore_ascii_case("success") {
            String::new()
        } else {
            text
        }
    };

    Some(CloudTask {
        id,
        name,
        url,
        status: status.clone(),
        status_text: status_text(&status),
        progress,
        total,
        done,
        created,
        save_path,
        error,
    })
}

/// 解析 `task_info`：既支持 list_task 的数组，也支持 query_task 的对象。
pub(crate) fn tasks_from_response(v: &Value) -> Vec<CloudTask> {
    match v.get("task_info") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| task_from_value(item, None))
            .collect(),
        Some(Value::Object(map)) => map
            .iter()
            .filter_map(|(id, item)| task_from_value(item, Some(id)))
            .collect(),
        _ => Vec::new(),
    }
}

/// 离线任务的来源类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceKind {
    /// `magnet:?xt=...`（BT/磁力）。
    Magnet,
    /// http(s)/ftp 直链。
    Direct,
}

/// 校验并判定 `url` 类型（http/https/ftp/magnet）。
pub(crate) fn source_kind(url: &str) -> Result<SourceKind> {
    let raw = url.trim();
    if raw.is_empty() {
        return Err(Error::other("离线下载地址不能为空"));
    }
    let lower = raw.to_ascii_lowercase();
    if lower.starts_with("magnet:") {
        return Ok(SourceKind::Magnet);
    }
    if ["http://", "https://", "ftp://"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return Ok(SourceKind::Direct);
    }
    Err(Error::other(format!(
        "不支持的离线下载地址（仅支持 http/https/ftp/magnet）：{}",
        raw.chars().take(120).collect::<String>()
    )))
}

/// `save_path` 规范化：空串/根目录 → `/`，其余去掉尾部斜杠。
pub(crate) fn normalize_save_path(dest_dir: &str) -> String {
    norm_path(dest_dir)
}

/// 直链的 `task_name`：取 URL 最后一段文件名。
///
/// 以 `/` 结尾（目录型 URL）或纯域名（没有 path）时不硬塞 `task_name`，
/// 交给百度的 `source_url` 自行推断。
fn task_name_from_url(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    if !parsed.path().rsplit('/').any(|seg| !seg.trim().is_empty()) {
        return None;
    }
    let name = file_name_from_url(parsed.path());
    // `sanitize_file_name` 对空名字会回退成“未命名”，这里视为没有文件名。
    (!name.is_empty() && name != "/" && name != "未命名").then_some(name)
}

/// 构造 `add_task` 的表单字段（纯函数，便于单测）。
///
/// * magnet → `od_type=4`（HAR 里 BT 任务），不带 `task_name`（种子名由服务端解析）；
/// * 直链 → 不传 `od_type`（等价于 `od_type=0`），尽量带 `task_name`
///   （取 URL 最后一段文件名）。
fn add_task_form_for(kind: SourceKind, url: &str, save_path: &str) -> Vec<(String, String)> {
    let mut form = vec![
        ("source_url".to_string(), url.trim().to_string()),
        ("save_path".to_string(), normalize_save_path(save_path)),
    ];
    match kind {
        SourceKind::Magnet => form.push(("od_type".to_string(), "4".to_string())),
        SourceKind::Direct => {
            if let Some(name) = task_name_from_url(url) {
                form.push(("task_name".to_string(), name));
            }
        }
    }
    form
}

/// 校验 URL 并构造表单（带校验的入口，便于单测）。
#[cfg(test)]
fn add_task_form(url: &str, save_path: &str) -> Result<Vec<(String, String)>> {
    let kind = source_kind(url)?;
    Ok(add_task_form_for(kind, url, save_path))
}

/// `add_task` 的官方错误码 → 中文文案（从网页 JS 提取）。
pub(crate) fn cloud_add_error_label(code: i64) -> Option<&'static str> {
    Some(match code {
        36005 => "用户未登录",
        36006 => "用户未激活",
        36007 => "用户未授权",
        36009 => "空间不足，无法创建离线任务",
        36010 => "文件不存在",
        36012 => "操作超时",
        36013 => "同时下载的任务过多",
        36014 => "存储路径已使用",
        36016 => "任务已删除",
        36017 => "任务已完成",
        36018 => "解析失败，种子文件损坏",
        36019 => "任务正在处理中",
        36020 => "任务地址不存在",
        36021 => "普通用户同时最多 1 个离线任务（需开通离线下载套餐）",
        36022 => "任务过多",
        36023 => "普通用户每月只能离线下载 5 个任务",
        36024 => "本月下载数已超限制",
        36025 => "分享链接已失效",
        36026 | 36027 => "链接格式有误",
        36028 => "暂时无法找到相关种子信息",
        36031 => "网络繁忙",
        36032 => "违规内容被屏蔽",
        _ => return None,
    })
}

/// 错误码 → 可读中文；未知码保留服务端 `error_msg/show_msg` 原文。
pub(crate) fn cloud_add_error_message(code: i64, v: &Value) -> String {
    let detail = err_text(v, "");
    let code_text = format!("error_code:{code}");
    match cloud_add_error_label(code) {
        Some(label) => {
            if detail.is_empty() || detail == label {
                format!("{label}（{code_text}）")
            } else {
                format!("{label}（百度返回：{detail}；{code_text}）")
            }
        }
        None => {
            if detail.is_empty() {
                format!("云添加失败（{code_text}）")
            } else {
                format!("{detail}（{code_text}）")
            }
        }
    }
}

/// 兼容数字/字符串的 `task_id` 提取；0/空串视为缺失。
pub(crate) fn response_task_id(v: &Value) -> Option<String> {
    let raw = ["task_id", "taskId", "taskid"]
        .iter()
        .find_map(|key| v.get(*key))?;
    let text = if let Some(s) = raw.as_str() {
        s.trim().to_string()
    } else if let Some(n) = raw.as_i64() {
        n.to_string()
    } else if let Some(n) = raw.as_u64() {
        n.to_string()
    } else {
        return None;
    };
    if text.is_empty() || text == "0" {
        None
    } else {
        Some(text)
    }
}

/// 解析 `add_task` 响应。
///
/// ⚠️ 错误字段是 `error_code`（不是 `errno`）；成功时返回 task_id 字符串。
pub(crate) fn parse_add_task_response(v: &Value) -> Result<String> {
    let code = loose_i64(v, &["error_code", "errorCode"]).unwrap_or(0);
    if code == 0 {
        if let Some(id) = response_task_id(v) {
            return Ok(id);
        }
        let body: String = v.to_string();
        return Err(Error::server(
            -1,
            format!(
                "云添加成功但响应缺少 task_id：{}",
                body.chars().take(300).collect::<String>()
            ),
        ));
    }
    Err(Error::server(code, cloud_add_error_message(code, v)))
}

/// 创建离线下载任务，成功返回百度 `task_id`。
pub(crate) async fn add_task(d: &BaiduDrive, url: &str, dest_dir: &str) -> Result<String> {
    // 先校验 URL（不发请求/不建目录），再确保保存目录存在。
    let kind = source_kind(url)?;
    let dir = files::ensure_dir(d, dest_dir).await?;
    let save_path = normalize_save_path(&dir.path);
    let form = add_task_form_for(kind, url, &save_path);
    let params = vec![("method".to_string(), "add_task".to_string())];
    // with_bdstoken 会自动补 channel=chunlei；公共 query（clienttype/app_id/
    // web/dp-logid）由 http.rs 统一附加，与真机请求形态一致。
    let v = d
        .api_post_form(CLOUD_DL, &params, &form, ReqOpts::authed().with_bdstoken())
        .await?;
    let task_id = parse_add_task_response(&v)?;
    wp_core::log_info!("百度", "已创建离线任务 {task_id}：{url} → {save_path}");
    Ok(task_id)
}

/// 云任务列表（只读；`status=255` = 全部状态）。
pub(crate) async fn list_tasks(d: &BaiduDrive) -> Result<Vec<CloudTask>> {
    let mut out: Vec<CloudTask> = Vec::new();
    let mut start: u64 = 0;
    for _ in 0..MAX_PAGES {
        let params = vec![
            ("start".to_string(), start.to_string()),
            ("limit".to_string(), PAGE_SIZE.to_string()),
            ("need_task_info".to_string(), "1".to_string()),
            ("status".to_string(), "255".to_string()),
            ("method".to_string(), "list_task".to_string()),
            ("t".to_string(), now_ms().to_string()),
        ];
        let v = d
            .api_get(
                "/rest/2.0/services/cloud_dl",
                &params,
                ReqOpts::authed().no_channel().with_bdstoken(),
            )
            .await?;
        let items = v.get("task_info").and_then(Value::as_array);
        let count = items.map(|a| a.len()).unwrap_or(0);
        let parsed = tasks_from_response(&v);
        let fetched = parsed.len();
        out.extend(parsed);
        let total = v.get("total").and_then(Value::as_u64).unwrap_or(0);
        if count < PAGE_SIZE {
            break;
        }
        if total > 0 && start + fetched as u64 >= total {
            break;
        }
        start += PAGE_SIZE as u64;
    }
    wp_core::log_debug!("百度", "云任务列表 → {} 项", out.len());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn list_task_entry_mapping_from_har() {
        // 字段形状取自 HAR `cloud_dl?method=list_task` 实测响应。
        let v = json!({
            "task_id": "6273246743",
            "od_type": "0",
            "source_url": "https://example.invalid/mabox.iso",
            "save_path": "/我的资源/mabox.iso",
            "status": "0",
            "create_time": "1767525807",
            "task_name": "mabox.iso",
            "orgin_web_url": ""
        });
        let t = task_from_value(&v, None).unwrap();
        assert_eq!(t.id, "6273246743");
        assert_eq!(t.name, "mabox.iso");
        assert_eq!(t.url, "https://example.invalid/mabox.iso");
        assert_eq!(t.status, "0");
        assert_eq!(t.status_text, "已完成");
        assert_eq!(t.created, Some(1767525807));
        assert_eq!(t.save_path, "/我的资源/mabox.iso");
        assert!(t.error.is_empty());
        assert_eq!(t.progress, 0.0);
    }

    #[test]
    fn query_task_object_form_and_progress() {
        let v = json!({
            "task_info": {
                "1647018570": {
                    "status": "0",
                    "file_size": "4919256312",
                    "finished_size": "4919256312",
                    "create_time": "1478781414",
                    "save_path": "/一般视频/电影/",
                    "source_url": "magnet:?xt=urn:btih:abc",
                    "task_name": "电影"
                },
                "1681710176": {
                    "status": "4",
                    "file_size": "1066791369",
                    "finished_size": "0",
                    "create_time": "1481124027",
                    "save_path": "/图片-文本/",
                    "source_url": "magnet:?xt=urn:btih:def",
                    "task_name": "借贷宝"
                }
            }
        });
        let tasks = tasks_from_response(&v);
        assert_eq!(tasks.len(), 2);
        let done = tasks.iter().find(|t| t.id == "1647018570").unwrap();
        assert_eq!(done.status_text, "已完成");
        assert_eq!(done.total, 4919256312);
        assert_eq!(done.done, done.total);
        assert_eq!(done.progress, 1.0);
        let waiting = tasks.iter().find(|t| t.id == "1681710176").unwrap();
        assert_eq!(waiting.status_text, "等待中");
        assert_eq!(waiting.progress, 0.0);
    }

    #[test]
    fn progress_percent_and_unknown_status_are_safe() {
        let v = json!({
            "task_id": 1,
            "status": "3",
            "progress": "25",
            "file_size": "1000",
            "finished_size": "250"
        });
        let t = task_from_value(&v, None).unwrap();
        assert_eq!(t.status_text, "下载中");
        assert!((t.progress - 0.25).abs() < 1e-6);
        assert_eq!(status_text("9"), "状态 9");
        assert_eq!(status_text(""), "未知");
        // 没有 task_id 的脏条目丢弃。
        assert!(task_from_value(&json!({"status": "0"}), None).is_none());
    }

    fn form_value<'a>(form: &'a [(String, String)], key: &str) -> Option<&'a str> {
        form.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn source_kind_accepts_only_supported_schemes() {
        assert_eq!(
            source_kind("https://example.com/a.iso").unwrap(),
            SourceKind::Direct
        );
        assert_eq!(
            source_kind("HTTP://example.com/a.iso").unwrap(),
            SourceKind::Direct
        );
        assert_eq!(
            source_kind("ftp://example.com/a.iso").unwrap(),
            SourceKind::Direct
        );
        assert_eq!(
            source_kind("  MAGNET:?xt=urn:btih:abc  ").unwrap(),
            SourceKind::Magnet
        );
        assert!(source_kind("").is_err());
        assert!(source_kind("   ").is_err());
        assert!(source_kind("ed2k://|file|a|1|hash|/").is_err());
        assert!(source_kind("magnet").is_err(), "缺冒号不是合法 magnet");
    }

    #[test]
    fn add_task_form_uses_od_type_4_only_for_magnet() {
        // magnet：od_type=4，不猜 task_name。
        let form = add_task_form("magnet:?xt=urn:btih:abc", "/下载/BT").unwrap();
        assert_eq!(
            form_value(&form, "source_url"),
            Some("magnet:?xt=urn:btih:abc")
        );
        assert_eq!(form_value(&form, "save_path"), Some("/下载/BT"));
        assert_eq!(form_value(&form, "od_type"), Some("4"));
        assert_eq!(form_value(&form, "task_name"), None);

        // http 直链：不传 od_type（等价 0），task_name 取 URL 最后一段（去掉 query）。
        let form = add_task_form(
            "https://example.com/download/a%20b.iso?token=1#part",
            "/下载/电影",
        )
        .unwrap();
        assert_eq!(form_value(&form, "od_type"), None);
        assert_eq!(form_value(&form, "task_name"), Some("a b.iso"));

        // 目录型 URL（以 / 结尾）没有文件名，不硬塞“未命名”。
        let form = add_task_form("https://example.com/download/", "/").unwrap();
        assert_eq!(form_value(&form, "task_name"), None);
    }

    #[test]
    fn cloud_add_error_codes_map_to_chinese() {
        let v = json!({
            "error_code": 36009,
            "error_msg": "storage exceed limit",
            "show_msg": "百度网盘空间不足"
        });
        let msg = cloud_add_error_message(36009, &v);
        assert!(msg.contains("空间不足"), "实际：{msg}");
        assert!(msg.contains("百度网盘空间不足"), "实际：{msg}");
        match parse_add_task_response(&v) {
            Err(Error::Server { code, msg }) => {
                assert_eq!(code, 36009);
                assert!(msg.contains("空间不足"), "实际：{msg}");
            }
            other => panic!("应为 Server(36009)，实际 {other:?}"),
        }

        let msg = cloud_add_error_message(36021, &json!({"error_code": 36021}));
        assert!(msg.contains("1 个"), "实际：{msg}");
        assert!(msg.contains("套餐"), "实际：{msg}");
        let msg = cloud_add_error_message(36023, &json!({"error_code": 36023}));
        assert!(msg.contains("5 个"), "实际：{msg}");

        // 未知码保留服务端原文。
        let msg = cloud_add_error_message(99999, &json!({"error_msg": "weird backend"}));
        assert!(msg.contains("weird backend"), "实际：{msg}");
        assert!(msg.contains("99999"), "实际：{msg}");
        let msg = cloud_add_error_message(99998, &json!({}));
        assert!(msg.contains("云添加失败"), "实际：{msg}");
    }

    #[test]
    fn add_task_response_task_id_number_or_string() {
        assert_eq!(
            parse_add_task_response(&json!({"error_code": 0, "task_id": 6273246743i64})).unwrap(),
            "6273246743"
        );
        assert_eq!(
            parse_add_task_response(&json!({"error_code": "0", "task_id": "abc123"})).unwrap(),
            "abc123"
        );
        assert_eq!(
            parse_add_task_response(&json!({"errorCode": "0", "taskId": 42})).unwrap(),
            "42"
        );
        // 成功但没有 task_id 属于异常响应，不能假装成功。
        assert!(parse_add_task_response(&json!({"error_code": 0})).is_err());
        assert!(parse_add_task_response(&json!({"error_code": 0, "task_id": 0})).is_err());
        // 非 0：错误字段是 error_code，不是 errno。
        assert!(parse_add_task_response(&json!({"errno": 0, "error_code": 36021})).is_err());
    }

    #[test]
    fn save_path_root_stays_root_and_paths_are_normalized() {
        assert_eq!(normalize_save_path("/"), "/");
        assert_eq!(normalize_save_path(""), "/");
        assert_eq!(normalize_save_path("///"), "/");
        assert_eq!(normalize_save_path("/我的资源/电影/"), "/我的资源/电影");

        let form = add_task_form("https://example.com/a.iso", "/").unwrap();
        assert_eq!(form_value(&form, "save_path"), Some("/"));
        let form = add_task_form("magnet:?xt=urn:btih:abc", "/我的资源/电影").unwrap();
        assert_eq!(form_value(&form, "save_path"), Some("/我的资源/电影"));
    }
}
