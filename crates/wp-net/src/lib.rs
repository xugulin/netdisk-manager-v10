//! wp-net —— 三家网盘适配器共用的 HTTP / Cookie / 摘要 / 下载工具。
//!
//! 设计原则：
//! * 不封装「网盘业务」，只提供协议层原语，适配器各写各的请求；
//! * 所有错误统一成 [`wp_core::Error`]，分类由 `net_err` / `http_status_error` 决定；
//! * 下载器支持断点续传与多段并发，进度回调返回 false 表示取消（适配器据此返回
//!   [`wp_core::Error::Canceled`] 或暂停）。

pub mod client;
pub mod cookie;
pub mod crypto;
pub mod download;
pub mod retry;

pub use client::{
    build_client, build_client_default, check_status, http_status_error, json_value,
    json_value_loose, net_err, send_json, ClientOptions,
};
pub use cookie::CookieJar;
pub use download::{download, DownloadRequest};
pub use retry::{retry, RetryPolicy};
