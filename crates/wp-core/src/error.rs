//! 统一错误类型。
//!
//! 三家网盘的错误语义各不相同，适配器层负责把自己的错误码翻译成这里的分类；
//! 传输引擎据此决定「重试 / 暂停 / 直接失败 / 要求重新登录」。

use std::fmt;

/// 统一 Result。
pub type Result<T> = std::result::Result<T, Error>;

/// 错误分类 —— 决定上层如何处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// 需要登录 / 会话失效（上层应弹出登录框）
    Auth,
    /// 网络层错误（可重试）
    Network,
    /// 服务端业务错误（可能可重试，看 code）
    Server,
    /// 路径 / 文件不存在
    NotFound,
    /// 当前网盘不支持该操作
    Unsupported,
    /// 本地 IO 错误
    Io,
    /// 其它
    Other,
}

impl ErrorKind {
    pub fn label(self) -> &'static str {
        match self {
            ErrorKind::Auth => "需要登录",
            ErrorKind::Network => "网络错误",
            ErrorKind::Server => "服务端错误",
            ErrorKind::NotFound => "不存在",
            ErrorKind::Unsupported => "不支持",
            ErrorKind::Io => "本地错误",
            ErrorKind::Other => "错误",
        }
    }
}

/// 应用统一错误。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("需要登录：{0}")]
    Auth(String),

    #[error("网络错误：{0}")]
    Network(String),

    #[error("服务端错误({code})：{msg}")]
    Server { code: i64, msg: String },

    #[error("未找到：{0}")]
    NotFound(String),

    #[error("不支持：{0}")]
    Unsupported(String),

    #[error("本地 IO 错误：{0}")]
    Io(#[from] std::io::Error),

    #[error("已取消：{0}")]
    Canceled(String),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn auth<S: Into<String>>(msg: S) -> Self {
        Self::Auth(msg.into())
    }
    pub fn network<S: Into<String>>(msg: S) -> Self {
        Self::Network(msg.into())
    }
    pub fn server<S: Into<String>>(code: i64, msg: S) -> Self {
        Self::Server {
            code,
            msg: msg.into(),
        }
    }
    pub fn not_found<S: Into<String>>(msg: S) -> Self {
        Self::NotFound(msg.into())
    }
    pub fn unsupported<S: Into<String>>(msg: S) -> Self {
        Self::Unsupported(msg.into())
    }
    pub fn other<S: Into<String>>(msg: S) -> Self {
        Self::Other(msg.into())
    }

    pub fn kind(&self) -> ErrorKind {
        match self {
            Error::Auth(_) => ErrorKind::Auth,
            Error::Network(_) => ErrorKind::Network,
            Error::Server { .. } => ErrorKind::Server,
            Error::NotFound(_) => ErrorKind::NotFound,
            Error::Unsupported(_) => ErrorKind::Unsupported,
            Error::Canceled(_) => ErrorKind::Other,
            Error::Io(_) => ErrorKind::Io,
            Error::Other(_) => ErrorKind::Other,
        }
    }

    /// 是否值得重试（网络错误 / 429 / 5xx / 部分业务限流码）。
    pub fn retryable(&self) -> bool {
        match self {
            Error::Network(_) => true,
            Error::Server { code, .. } => {
                *code == 429 || *code == -1 || (100..=599).contains(&code.abs()) || *code == 0
            }
            Error::Io(_) | Error::Canceled(_) => false,
            _ => false,
        }
    }

    /// 是否是用户取消/暂停（不代表失败，不应计入错误）。
    pub fn is_canceled(&self) -> bool {
        matches!(self, Error::Canceled(_))
    }

    /// 是否属于「重新登录能解决」。
    pub fn is_auth(&self) -> bool {
        matches!(self, Error::Auth(_))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Other(format!("JSON 解析失败：{e}"))
    }
}

impl From<std::string::FromUtf8Error> for Error {
    fn from(e: std::string::FromUtf8Error) -> Self {
        Error::Other(format!("UTF-8 解析失败：{e}"))
    }
}

/// 在日志/界面里显示错误时用的一行文本（不展开 source 链）。
pub fn err_line(e: &Error) -> String {
    let mut s = e.to_string();
    if let Error::Network(_) = e {
        s = s.replace('\n', " ");
    }
    if s.chars().count() > 300 {
        s = s.chars().take(300).collect::<String>() + "…";
    }
    s
}

/// 便于把 `Result<T, E2>` 转成统一错误。
pub trait IntoWpResult<T> {
    fn wp(self) -> Result<T>;
}

impl<T, E: fmt::Display> IntoWpResult<T> for std::result::Result<T, E> {
    fn wp(self) -> Result<T> {
        self.map_err(|e| Error::Other(e.to_string()))
    }
}
