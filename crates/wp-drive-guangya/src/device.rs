//! 设备身份：`did`（设备 ID）、`smid`（设备指纹）、`dt`（设备类型）。
//!
//! Python 原实现把身份写在 `~/.光鸭云盘/设备信息.json`；本 crate 按统一约定
//! 持久化到 `DriveInit.data_dir/device.json`，字段名用英文：
//!
//! ```json
//! {"device_id":"<32 位小写 hex>","smid":"<base64>","dt":"4"}
//! ```
//!
//! 设备 ID 一旦生成就必须跨进程稳定，否则服务端会视为新设备（风控/登录态都可能失效）。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;
use wp_core::Result;

/// 设备身份（请求头 `did` / `smid` / `dt` 的来源）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceIdentity {
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub smid: String,
    #[serde(default = "default_dt")]
    pub dt: String,
}

fn default_dt() -> String {
    "4".to_string()
}

impl DeviceIdentity {
    /// 从 `dir/device.json` 读取；不存在或损坏时生成并落盘。
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("device.json");
        let mut dev = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<DeviceIdentity>(&text).unwrap_or_default(),
            Err(_) => DeviceIdentity::default(),
        };
        let mut dirty = false;
        if !is_valid_device_id(&dev.device_id) {
            dev.device_id = new_device_id();
            dirty = true;
        }
        if dev.smid.trim().is_empty() {
            dev.smid = platform_fingerprint();
            dirty = true;
        }
        if dev.dt.trim().is_empty() {
            dev.dt = default_dt();
            dirty = true;
        }
        if dirty {
            write_json_atomic(&path, &dev)?;
        }
        Ok(dev)
    }

    /// 每个请求新生成的 W3C traceparent（`00-<32hex>-<16hex>-01`）。
    pub fn traceparent(&self) -> String {
        let trace = Uuid::new_v4().simple().to_string();
        let span = Uuid::new_v4().simple().to_string();
        format!("00-{trace}-{}-01", &span[..16])
    }
}

impl Default for DeviceIdentity {
    fn default() -> Self {
        Self {
            device_id: String::new(),
            smid: String::new(),
            dt: default_dt(),
        }
    }
}

/// 32 位小写十六进制（Python `uuid.uuid4().hex` 等价）。
pub fn new_device_id() -> String {
    Uuid::new_v4().simple().to_string()
}

fn is_valid_device_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) && id == id.to_ascii_lowercase()
}

/// 设备指纹：`base64("os|arch|rust|版本|机器标识")`。
///
/// 不照抄 Python 的 `platform.python_version()`（Rust 侧没有该概念），但保持
/// “稳定的平台五元组 + base64”结构与 smid 语义一致；首启后固定持久化，
/// 后续不再重算，因此不会因升级 crate 而漂移。
fn platform_fingerprint() -> String {
    let machine = machine_id();
    let raw = format!(
        "{}|{}|rust|{}|{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        env!("CARGO_PKG_VERSION"),
        machine
    );
    wp_net::crypto::base64_encode(raw.as_bytes())
}

fn machine_id() -> String {
    for p in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(s) = std::fs::read_to_string(p) {
            let s = s.trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    for key in ["HOSTNAME", "COMPUTERNAME"] {
        if let Ok(v) = std::env::var(key) {
            if !v.trim().is_empty() {
                return v.trim().to_string();
            }
        }
    }
    // 找不到系统机器标识时也不能每次随机（会破坏稳定性），退化为常量 + 设备 ID 无关值。
    "unknown-machine".to_string()
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(value)?;
    let tmp: PathBuf = path.with_extension("json.tmp");
    std::fs::write(&tmp, text.as_bytes())?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            // Windows 上目标已存在时 rename 会失败，退化为直接覆盖。
            let _ = std::fs::remove_file(&tmp);
            std::fs::write(path, text.as_bytes())?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "wp-gy-{tag}-{}-{}",
            std::process::id(),
            wp_core::util::random_hex(4)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn device_is_generated_and_stable() {
        let dir = tmpdir("device");
        let a = DeviceIdentity::load_or_create(&dir).unwrap();
        assert_eq!(a.device_id.len(), 32);
        assert!(a.device_id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(a.device_id, a.device_id.to_ascii_lowercase());
        assert_eq!(a.dt, "4");
        let raw = wp_net::crypto::base64_decode(&a.smid).unwrap();
        let text = String::from_utf8(raw).unwrap();
        assert!(text.contains('|'), "smid 应为 base64 的平台五元组：{text}");

        let b = DeviceIdentity::load_or_create(&dir).unwrap();
        assert_eq!(a, b, "第二次加载必须复用同一身份");
        assert!(dir.join("device.json").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn traceparent_shape() {
        let dev = DeviceIdentity::default();
        let t = dev.traceparent();
        let parts: Vec<&str> = t.split('-').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "00");
        assert_eq!(parts[1].len(), 32);
        assert_eq!(parts[2].len(), 16);
        assert_eq!(parts[3], "01");
        assert!(parts[1].bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(parts[2].bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
