//! Cookie 容器：解析/生成 Cookie 头、持久化。
//!
//! 三家网盘都用原生的 `name=value; name2=value2` Cookie，不需要浏览器的
//! domain/path 语义；因此实现成简单的有序映射，避免引入 cookie_store 的行为差异。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// 简单的 Cookie 容器（保序，方便日志与比对）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CookieJar {
    #[serde(default)]
    pub map: BTreeMap<String, String>,
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    /// 解析请求头形式的 Cookie 串：`a=1; b=2`。
    pub fn parse(text: &str) -> Self {
        let mut jar = Self::new();
        for part in text.split(';') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            match part.split_once('=') {
                Some((k, v)) => {
                    let k = k.trim();
                    if !k.is_empty() {
                        jar.map.insert(k.to_string(), v.trim().to_string());
                    }
                }
                None => {
                    jar.map.insert(part.to_string(), String::new());
                }
            }
        }
        jar
    }

    /// 解析 Set-Cookie 响应头（只取 name=value，忽略属性）。
    pub fn apply_set_cookie(&mut self, set_cookie: &str) {
        let first = set_cookie.split(';').next().unwrap_or("").trim();
        if let Some((k, v)) = first.split_once('=') {
            let k = k.trim();
            if !k.is_empty() {
                self.map.insert(k.to_string(), v.trim().to_string());
            }
        }
    }

    /// 生成请求头 Cookie 串。
    pub fn header(&self) -> String {
        self.map
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(String::as_str)
    }

    /// 取第一个非空的候选键（例如 BDUSS 有多个变体）。
    pub fn get_any(&self, keys: &[&str]) -> Option<&str> {
        for k in keys {
            if let Some(v) = self.get(k) {
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
        None
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.map.insert(key.into(), value.into());
    }

    pub fn set_if_empty(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let k = key.into();
        if self.map.get(&k).map(|v| v.is_empty()).unwrap_or(true) {
            self.map.insert(k, value.into());
        }
    }

    pub fn remove(&mut self, key: &str) {
        self.map.remove(key);
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// 合并另一个 jar（后者覆盖前者）。
    pub fn merge(&mut self, other: &CookieJar) {
        for (k, v) in &other.map {
            self.map.insert(k.clone(), v.clone());
        }
    }

    /// 从 JSON 文件加载。
    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// 原子保存到 JSON 文件。
    pub fn save(&self, path: &Path) -> wp_core::Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        wp_core::paths::atomic_write(path, text.as_bytes())?;
        Ok(())
    }

    /// 打码摘要（日志用，不泄漏 Cookie 值）。
    pub fn masked_summary(&self) -> String {
        self.map
            .keys()
            .map(|k| format!("{k}({})", self.get(k).unwrap_or("").len()))
            .collect::<Vec<_>>()
            .join("、")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_header() {
        let jar = CookieJar::parse("BDUSS=abc; STOKEN=def;BAIDUID=ghi");
        assert_eq!(jar.get("BDUSS"), Some("abc"));
        assert_eq!(jar.get("STOKEN"), Some("def"));
        assert_eq!(jar.header(), "BAIDUID=ghi; BDUSS=abc; STOKEN=def");
    }

    #[test]
    fn set_cookie() {
        let mut jar = CookieJar::new();
        jar.apply_set_cookie("__kps=xyz; Path=/; Domain=pan.quark.cn; HttpOnly");
        assert_eq!(jar.get("__kps"), Some("xyz"));
    }

    #[test]
    fn get_any_variants() {
        let jar = CookieJar::parse("BDUSS_BFESS=1; STOKEN=2");
        assert_eq!(jar.get_any(&["BDUSS", "BDUSS_BFESS"]), Some("1"));
    }

    #[test]
    fn roundtrip_json() {
        let jar = CookieJar::parse("a=1; b=2");
        let text = serde_json::to_string(&jar).unwrap();
        let back: CookieJar = serde_json::from_str(&text).unwrap();
        assert_eq!(jar, back);
    }
}
