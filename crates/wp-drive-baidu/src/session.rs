//! 百度网盘凭证（Cookie）会话与持久化。
//!
//! 百度的“登录态”不是 OAuth token，而是浏览器 Cookie 里的
//! `BDUSS`（可带 `BDUSS_BFESS` 备份）+ `STOKEN`。`bdstoken` 只是写操作
//! 需要的 CSRF 参数，会随会话刷新而变化，因此也一并持久化。
//!
//! 研究文档 §3.7 的结论：
//! * 最小有效集 = (`BDUSS` 或 `BDUSS_BFESS`) + `STOKEN`；
//! * 文件权限收紧到 0600，原子写入；
//! * 从多个 `Set-Cookie` 提取时必须**逐个**解析，不能 join 后按逗号切
//!   （`expires=Mon, 13-Sep-27` 会吃掉下一个 Cookie 名）。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Instant;
use wp_core::model::QrState;

/// 会话文件名（位于 `DriveInit.data_dir`）。
pub const SESSION_FILE: &str = "凭证.json";

/// 百度侧需要保存的 Cookie 名（大小写不敏感，写回时保持官方大小写）。
const COOKIE_NAMES: [&str; 5] = ["BDUSS", "BDUSS_BFESS", "STOKEN", "BAIDUID", "BAIDUID_BFESS"];

/// 持久化的会话结构。字段名保持英文，JSON 里也是英文键。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Session {
    #[serde(default)]
    pub bduss: String,
    #[serde(default)]
    pub bduss_bfess: String,
    #[serde(default)]
    pub stoken: String,
    #[serde(default)]
    pub baiduid: String,
    #[serde(default)]
    pub baiduid_bfess: String,
    /// CSRF 令牌（32 位 hex），写操作需要。
    #[serde(default)]
    pub bdstoken: String,
    /// 用户 id，gettemplatevariable / getinfo 回填。
    #[serde(default)]
    pub uk: Option<i64>,
    /// 最后更新时间（Unix 秒）。
    #[serde(default)]
    pub updated: i64,
}

impl Session {
    /// 最小有效集：BDUSS/BDUSS_BFESS 之一 + STOKEN。
    pub fn valid(&self) -> bool {
        (!self.bduss.trim().is_empty() || !self.bduss_bfess.trim().is_empty())
            && !self.stoken.trim().is_empty()
    }

    pub fn bduss_value(&self) -> &str {
        if self.bduss.trim().is_empty() {
            self.bduss_bfess.as_str()
        } else {
            self.bduss.as_str()
        }
    }

    pub fn uk(&self) -> Option<i64> {
        self.uk
    }

    /// 导出的 Cookie 头，顺序固定为 `BDUSS; BDUSS_BFESS; STOKEN`。
    /// 与 Python 版 `会话仓库.导出cookie头()` 一致，不发送 BAIDUID。
    pub fn cookie_header(&self) -> String {
        let mut pairs = Vec::new();
        if !self.bduss.trim().is_empty() {
            pairs.push(format!("BDUSS={}", self.bduss));
        }
        if !self.bduss_bfess.trim().is_empty() {
            pairs.push(format!("BDUSS_BFESS={}", self.bduss_bfess));
        }
        if !self.stoken.trim().is_empty() {
            pairs.push(format!("STOKEN={}", self.stoken));
        }
        pairs.join("; ")
    }

    /// 会话文件路径。
    pub fn path(dir: &Path) -> PathBuf {
        dir.join(SESSION_FILE)
    }

    /// 读取会话；不存在/损坏按未登录处理（与 Python 版一致）。
    pub fn load(dir: &Path) -> Self {
        let path = Self::path(dir);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(s) => s,
            Err(e) => {
                wp_core::log_warn!("百度", "会话文件解析失败，按未登录处理：{e}");
                Self::default()
            }
        }
    }

    /// 原子保存（0600）。
    pub fn save(&self, dir: &Path) -> wp_core::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = Self::path(dir);
        let text = serde_json::to_string_pretty(self)?;
        wp_core::paths::atomic_write(&path, text.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            {
                wp_core::log_warn!("百度", "会话文件 chmod 0600 失败：{e}");
            }
        }
        Ok(())
    }

    /// 删除本地会话文件（登出）。
    pub fn remove(dir: &Path) -> std::io::Result<()> {
        match std::fs::remove_file(Self::path(dir)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// 从手工粘贴的 Cookie 文本导入（分号/换行分隔，可带 `Cookie:` 前缀）。
    ///
    /// 返回实际命中的 Cookie 名（官方大小写），便于界面反馈。
    pub fn apply_cookie_text(&mut self, text: &str) -> Result<Vec<String>, String> {
        let mut hits = Vec::new();
        for raw in text.split([';', '\n']) {
            let mut part = raw.trim();
            if part.is_empty() {
                continue;
            }
            if part.len() >= 7 && part[..7].eq_ignore_ascii_case("cookie:") {
                part = part[7..].trim();
            }
            if part.is_empty() {
                continue;
            }
            let Some((name, value)) = part.split_once('=') else {
                continue;
            };
            let name = name.trim();
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            if let Some(canonical) = self.set_cookie(name, value) {
                if !hits.iter().any(|h: &String| h == canonical) {
                    hits.push(canonical.to_string());
                }
            }
        }
        if hits.is_empty() {
            return Err("未在文本中找到 BDUSS / BDUSS_BFESS / STOKEN，请检查粘贴内容".into());
        }
        Ok(hits)
    }

    /// 解析单条 `Set-Cookie` 头（只取第一个 `;` 前的主体）。
    ///
    /// 返回命中的官方 Cookie 名。多个头必须分别调用，不能先拼接。
    pub fn apply_set_cookie(&mut self, set_cookie: &str) -> Option<String> {
        let body = set_cookie.split(';').next()?.trim();
        let (name, value) = body.split_once('=')?;
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        self.set_cookie(name.trim(), value).map(str::to_string)
    }

    fn set_cookie(&mut self, name: &str, value: &str) -> Option<&'static str> {
        let canonical = COOKIE_NAMES
            .iter()
            .copied()
            .find(|k| k.eq_ignore_ascii_case(name))?;
        match canonical {
            "BDUSS" => self.bduss = value.to_string(),
            "BDUSS_BFESS" => self.bduss_bfess = value.to_string(),
            "STOKEN" => self.stoken = value.to_string(),
            "BAIDUID" => self.baiduid = value.to_string(),
            "BAIDUID_BFESS" => self.baiduid_bfess = value.to_string(),
            _ => return None,
        }
        Some(canonical)
    }

    /// 安全日志用：只输出字段长度，不泄漏明文。
    pub fn masked_summary(&self) -> String {
        format!(
            "BDUSS({}) BDUSS_BFESS({}) STOKEN({}) bdstoken({}) uk={}",
            self.bduss.len(),
            self.bduss_bfess.len(),
            self.stoken.len(),
            self.bdstoken.len(),
            self.uk
                .map(|v| v.to_string())
                .unwrap_or_else(|| "无".into())
        )
    }
}

/// 一次扫码登录的临时状态（全局只保留一份）。
#[derive(Debug, Clone)]
pub struct QrSession {
    /// 轮询标识，等于 getqrcode 返回的 sign。
    pub sign: String,
    /// 本轮登录复用的 gid。
    pub gid: String,
    pub started: Instant,
    pub last_state: QrState,
}

impl QrSession {
    pub fn new(sign: String, gid: String) -> Self {
        Self {
            sign,
            gid,
            started: Instant::now(),
            last_state: QrState::Waiting,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validity_requires_bduss_and_stoken() {
        let mut s = Session::default();
        assert!(!s.valid());
        s.bduss = "a".into();
        assert!(!s.valid(), "只有 BDUSS 不应判定有效（实测会 -6）");
        s.stoken = "b".into();
        assert!(s.valid());
        s.bduss.clear();
        s.bduss_bfess = "c".into();
        assert!(s.valid(), "BDUSS_BFESS 可替代 BDUSS");
    }

    #[test]
    fn cookie_header_order_and_fields() {
        let mut s = Session {
            stoken: "S".into(),
            bduss_bfess: "B".into(),
            baiduid: "U".into(),
            ..Default::default()
        };
        assert_eq!(
            s.cookie_header(),
            "BDUSS_BFESS=B; STOKEN=S",
            "BAIDUID 不发送"
        );
        s.bduss = "A".into();
        assert_eq!(s.cookie_header(), "BDUSS=A; BDUSS_BFESS=B; STOKEN=S");
    }

    #[test]
    fn import_cookie_text_variants() {
        let mut s = Session::default();
        let hits = s
            .apply_cookie_text(
                "Cookie: BDUSS=1; BDUSS_BFESS=2\nSTOKEN=3; BAIDUID=4; PANWEB=ignored",
            )
            .unwrap();
        assert_eq!(hits, vec!["BDUSS", "BDUSS_BFESS", "STOKEN", "BAIDUID"]);
        assert!(s.valid());
        assert_eq!(s.bduss, "1");
        assert_eq!(s.stoken, "3");

        let mut bad = Session::default();
        assert!(bad.apply_cookie_text("foo=bar; baz=qux").is_err());
    }

    #[test]
    fn set_cookie_expires_comma_is_safe() {
        let mut s = Session::default();
        let hit = s.apply_set_cookie(
            "BDUSS=abc123; expires=Mon, 13-Sep-27 00:00:00 GMT; path=/; domain=.baidu.com",
        );
        assert_eq!(hit.as_deref(), Some("BDUSS"));
        assert_eq!(s.bduss, "abc123");
        // 下一次 Set-Cookie 单独解析，不会被上一个头的 expires 逗号吃掉
        let hit = s.apply_set_cookie("STOKEN=def456; Path=/");
        assert_eq!(hit.as_deref(), Some("STOKEN"));
        assert_eq!(s.stoken, "def456");
    }

    #[test]
    fn json_roundtrip() {
        let s = Session {
            bduss: "x".into(),
            stoken: "y".into(),
            bdstoken: "z".into(),
            uk: Some(42),
            ..Default::default()
        };
        let text = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&text).unwrap();
        assert_eq!(s, back);
    }
}
