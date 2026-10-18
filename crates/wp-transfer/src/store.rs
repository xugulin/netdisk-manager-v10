//! 任务台账持久化格式。
//!
//! 磁盘上是带版本号的 JSON 对象：
//! `{"version":1,"tasks":[...]}`；同时兼容旧式「裸数组」以便手工迁移。

use serde::{Deserialize, Serialize};
use wp_core::model::TransferTask;

pub(crate) const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct TaskFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub tasks: Vec<TransferTask>,
}

fn default_version() -> u32 {
    FORMAT_VERSION
}

impl TaskFile {
    pub(crate) fn new(tasks: Vec<TransferTask>) -> Self {
        Self {
            version: FORMAT_VERSION,
            tasks,
        }
    }
}

/// 序列化台账（速度字段由 `#[serde(skip)]` 自动排除）。
pub(crate) fn encode(tasks: &[TransferTask]) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec_pretty(&TaskFile::new(tasks.to_vec()))
}

/// 反序列化台账；兼容裸数组，失败返回可读错误文本。
pub(crate) fn decode(text: &str) -> Result<Vec<TransferTask>, String> {
    match serde_json::from_str::<TaskFile>(text) {
        Ok(file) => Ok(file.tasks),
        Err(_) => serde_json::from_str::<Vec<TransferTask>>(text).map_err(|e| e.to_string()),
    }
}
