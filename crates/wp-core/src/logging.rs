//! 进程内日志：环形缓冲（给 GUI 实时展示）+ 文件落盘（排查用）。
//!
//! 用法：
//! ```ignore
//! logging::init(&paths.log_dir(), LogLevel::Info, true);
//! log_info!("百度", "开始列目录 {}", path);
//! ```
//! GUI 每帧调用 [`recent_since`] 拉新增行，自身维护 `last_seq`。

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 日志级别（数值越大越严重）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn label(self) -> &'static str {
        match self {
            LogLevel::Debug => "调试",
            LogLevel::Info => "信息",
            LogLevel::Warn => "警告",
            LogLevel::Error => "错误",
        }
    }
    pub fn slug(self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "debug" | "调试" => LogLevel::Debug,
            "warn" | "warning" | "警告" => LogLevel::Warn,
            "error" | "错误" => LogLevel::Error,
            _ => LogLevel::Info,
        }
    }
}

/// 一条日志。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    /// 自增序号（GUI 增量拉取用）
    pub seq: u64,
    /// Unix 秒
    pub time: i64,
    pub level: LogLevel,
    /// 模块标签（如「传输」「百度」）
    pub target: String,
    pub message: String,
}

struct LoggerState {
    lines: VecDeque<LogLine>,
    capacity: usize,
    next_seq: u64,
    file: Option<std::fs::File>,
    file_path: Option<PathBuf>,
    terminal_level: LogLevel,
    /// 低于该级别的日志既不进缓冲也不落盘（调试级别控制）
    min_level: LogLevel,
}

fn state() -> &'static Mutex<LoggerState> {
    static STATE: OnceLock<Mutex<LoggerState>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(LoggerState {
            lines: VecDeque::new(),
            capacity: 5000,
            next_seq: 1,
            file: None,
            file_path: None,
            terminal_level: LogLevel::Info,
            min_level: LogLevel::Debug,
        })
    })
}

/// 初始化日志系统（幂等，可重复调用以切换日志目录）。
pub fn init(log_dir: &Path, terminal_level: LogLevel, to_file: bool) {
    let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
    st.terminal_level = terminal_level;
    st.min_level = LogLevel::Debug;
    if !to_file {
        return;
    }
    let _ = std::fs::create_dir_all(log_dir);
    // 文件名带日期，按天切分
    let day = crate::util::format_stamp(crate::util::now_ts());
    let day = &day[..8.min(day.len())];
    let path = log_dir.join(format!("运行-{day}.log"));
    if st.file_path.as_deref() != Some(path.as_path()) {
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(mut f) => {
                let _ = writeln!(
                    f,
                    "\n========== 启动 {} v{} ==========",
                    crate::APP_NAME,
                    crate::APP_VERSION
                );
                st.file = Some(f);
                st.file_path = Some(path);
            }
            Err(_) => {
                st.file = None;
                st.file_path = None;
            }
        }
    }
}

/// 设置最低记录级别。
pub fn set_min_level(level: LogLevel) {
    let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
    st.min_level = level;
}

/// 当前日志文件路径。
pub fn current_file() -> Option<PathBuf> {
    let st = state().lock().unwrap_or_else(|e| e.into_inner());
    st.file_path.clone()
}

/// 写一条日志（宏最终调用这里）。
pub fn push(level: LogLevel, target: &str, message: String) {
    if level
        < state()
            .lock()
            .map(|s| s.min_level)
            .unwrap_or(LogLevel::Debug)
    {
        return;
    }
    let now = crate::util::now_ts();
    let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
    let seq = st.next_seq;
    st.next_seq += 1;
    let line = LogLine {
        seq,
        time: now,
        level,
        target: target.to_string(),
        message: message.clone(),
    };

    // 文件落盘（先写，避免终端/缓冲影响）
    if let Some(f) = st.file.as_mut() {
        let _ = writeln!(
            f,
            "[{}] [{}] [{}] {}",
            crate::util::format_stamp(now),
            level.label(),
            target,
            message
        );
    }

    // 终端输出
    if level >= st.terminal_level {
        let color = match level {
            LogLevel::Error => "\x1b[31m",
            LogLevel::Warn => "\x1b[33m",
            LogLevel::Debug => "\x1b[90m",
            LogLevel::Info => "",
        };
        eprintln!("{color}[{} {}] {}\x1b[0m", level.label(), target, message);
    }

    if st.lines.len() >= st.capacity {
        st.lines.pop_front();
    }
    st.lines.push_back(line);
}

/// 拉取序号大于 `last_seq` 的所有日志行。
pub fn recent_since(last_seq: u64) -> Vec<LogLine> {
    let st = state().lock().unwrap_or_else(|e| e.into_inner());
    st.lines
        .iter()
        .filter(|l| l.seq > last_seq)
        .cloned()
        .collect()
}

/// 取最后 N 行（导出/复制用）。
pub fn tail(n: usize) -> Vec<LogLine> {
    let st = state().lock().unwrap_or_else(|e| e.into_inner());
    let start = st.lines.len().saturating_sub(n);
    st.lines.iter().skip(start).cloned().collect()
}

/// 清空内存缓冲（文件保留）。
pub fn clear() {
    let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
    st.lines.clear();
}

/// 导出全部日志到文本文件。
pub fn export(path: &Path) -> std::io::Result<()> {
    let st = state().lock().unwrap_or_else(|e| e.into_inner());
    let mut f = std::fs::File::create(path)?;
    for l in &st.lines {
        writeln!(
            f,
            "[{}] [{}] [{}] {}",
            crate::util::format_stamp(l.time),
            l.level.label(),
            l.target,
            l.message
        )?;
    }
    Ok(())
}

/// 关掉日志文件句柄（退出时用，保证 flush）。
pub fn shutdown() {
    let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(f) = st.file.as_mut() {
        let _ = f.flush();
    }
    st.file = None;
}

/// 日志宏：`log_info!("传输", "开始下载 {}", name)`。
#[macro_export]
macro_rules! log_info {
    ($target:expr, $($arg:tt)*) => {
        $crate::logging::push($crate::logging::LogLevel::Info, $target, format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($target:expr, $($arg:tt)*) => {
        $crate::logging::push($crate::logging::LogLevel::Warn, $target, format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_error {
    ($target:expr, $($arg:tt)*) => {
        $crate::logging::push($crate::logging::LogLevel::Error, $target, format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_debug {
    ($target:expr, $($arg:tt)*) => {
        $crate::logging::push($crate::logging::LogLevel::Debug, $target, format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_read_incremental() {
        clear();
        push(LogLevel::Info, "测试", "第一条".into());
        push(LogLevel::Warn, "测试", "第二条".into());
        let lines = recent_since(0);
        assert_eq!(lines.len(), 2);
        let last = lines.last().unwrap().seq;
        assert!(recent_since(last).is_empty());
        clear();
    }

    #[test]
    fn level_order() {
        assert!(LogLevel::Debug < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Warn);
        assert!(LogLevel::Warn < LogLevel::Error);
        assert_eq!(LogLevel::parse("警告"), LogLevel::Warn);
    }
}
