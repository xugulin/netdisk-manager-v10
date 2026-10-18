//! wp-core —— 与 UI / 网络无关的基础层：数据模型、配置、路径、日志、敏感词守卫、工具函数。

pub mod config;
pub mod error;
pub mod guard;
pub mod logging;
pub mod model;
pub mod paths;
pub mod util;

pub use error::{Error, ErrorKind, Result};
pub use model::*;
pub use paths::Paths;

/// 应用中文名（窗口标题等）。
pub const APP_NAME: &str = "网盘管理";
/// 版本号（与 Cargo.toml 同步）。
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
