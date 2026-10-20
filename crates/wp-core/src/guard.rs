//! 敏感词守卫与命名规避（对应 V2 `v8_3/敏感词/` 的上传守卫 + 命名规避）。
//!
//! 与 V2 的差异（有意简化，见 docs/PORTING.md）：
//! * 词库用 JSON 而不是 SQLite：`数据/敏感词.json`，结构见 [`SensitiveWordDb`]；
//! * V2 的「探测历史 / 敏感词树 / 置信度学习」不移植（P2）；保留「命中改名记录」；
//! * 匹配用 `aho-corasick`（与 V2 首选算法一致：区分大小写、纯子串、最长命中优先）；
//! * 用户交互补上 V2 缺的策略枚举：警告 / 拦截 / 自动改名。
//!
//! 命名规避（与敏感词是两回事）处理**目标网盘字符集限制**：
//! * 百度：非法字符用全角等价、控制字符变空格、星平面(emoji)移除；
//! * 夸克/光鸭：实测支持，不改名。

use crate::config::GuardAction;
use crate::model::DriveKind;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 词库（持久化为 `数据/敏感词.json`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SensitiveWordDb {
    /// 全局词（所有网盘生效）
    #[serde(default)]
    pub global: Vec<String>,
    /// 按网盘实例标识分组的词
    #[serde(default)]
    pub instances: BTreeMap<String, Vec<String>>,
    /// 改名记录（最近若干条）
    #[serde(default)]
    pub rename_records: Vec<RenameRecord>,
}

/// 一次命中改名记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameRecord {
    pub instance: String,
    pub original: String,
    pub renamed: String,
    pub hit_word: String,
    pub strategy: String,
    /// Unix 秒
    pub time: i64,
}

impl SensitiveWordDb {
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(t) => serde_json::from_str(&t).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> crate::Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        crate::paths::atomic_write(path, text.as_bytes())?;
        Ok(())
    }

    /// 某个实例生效的全部词（去重、去空）。
    pub fn words_for(&self, instance: &str) -> Vec<String> {
        let mut v: Vec<String> = self.global.clone();
        if let Some(list) = self.instances.get(instance) {
            v.extend(list.iter().cloned());
        }
        v.retain(|w| !w.trim().is_empty());
        v.sort();
        v.dedup();
        v
    }

    pub fn word_count(&self, instance: &str) -> usize {
        self.words_for(instance).len()
    }

    pub fn add_word(&mut self, instance: &str, word: impl Into<String>) {
        let w = word.into();
        let w = w.trim();
        if w.is_empty() {
            return;
        }
        let list = self.instances.entry(instance.to_string()).or_default();
        if !list.iter().any(|x| x == w) {
            list.push(w.to_string());
        }
    }

    pub fn remove_word(&mut self, instance: &str, word: &str) {
        if let Some(list) = self.instances.get_mut(instance) {
            list.retain(|x| x != word);
        }
        self.global.retain(|x| x != word);
    }

    /// 迁移旧数据：把「按实例 id」的词并入「按网盘类型（kind slug）」，
    /// 并删除本机/演示网盘的词（本机不需要敏感词守卫）。
    /// `pairs` 为 (实例 id, 网盘类型 slug)，如 ("baidu_2", "baidu")。
    pub fn migrate_instances_to_kinds(&mut self, pairs: &[(String, String)]) -> bool {
        let mut changed = false;
        // 先收集要合并的旧实例词
        let mut merged: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (id, kind) in pairs {
            if let Some(words) = self.instances.remove(id) {
                merged.entry(kind.clone()).or_default().extend(words);
                changed = changed || id != kind;
            }
        }
        for (kind, words) in merged {
            let list = self.instances.entry(kind).or_default();
            for w in words {
                let w = w.trim().to_string();
                if !w.is_empty() && !list.iter().any(|x| x == &w) {
                    list.push(w);
                }
            }
            changed = true;
        }
        // 本机/演示网盘不应有敏感词
        for key in ["local", "fake", "本机", "演示网盘"] {
            if self.instances.remove(key).is_some() {
                changed = true;
            }
        }
        // 改名记录里的实例 id 也改成类型 slug（页面展示统一）
        let map: std::collections::HashMap<&str, &str> = pairs
            .iter()
            .map(|(id, k)| (id.as_str(), k.as_str()))
            .collect();
        for r in &mut self.rename_records {
            if let Some(k) = map.get(r.instance.as_str()) {
                if r.instance != *k {
                    r.instance = (*k).to_string();
                    changed = true;
                }
            }
        }
        changed
    }
}

/// 一次命中。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub word: String,
    pub start: usize,
    pub end: usize,
}

/// 匹配器（按需构建，构造代价 O(词数)；GUI 侧缓存）。
pub struct Matcher {
    ac: aho_corasick::AhoCorasick,
    words: Vec<String>,
}

impl Matcher {
    pub fn new(words: &[String]) -> Option<Self> {
        if words.is_empty() {
            return None;
        }
        let ac = aho_corasick::AhoCorasick::builder()
            .ascii_case_insensitive(false)
            // LeftmostLongest：同一位置优先命中更长的词，符合 V2「最长命中」语义
            .match_kind(aho_corasick::MatchKind::LeftmostLongest)
            .build(words)
            .ok()?;
        Some(Self {
            ac,
            words: words.to_vec(),
        })
    }

    /// 全部命中（去掉被外层完全包含的内层命中，按起点排序）。
    pub fn hits(&self, text: &str) -> Vec<Hit> {
        let mut out: Vec<Hit> = self
            .ac
            .find_iter(text)
            .map(|m| Hit {
                word: self.words[m.pattern().as_usize()].clone(),
                start: m.start(),
                end: m.end(),
            })
            .collect();
        out.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
        // 去掉被前一个命中完全包含的
        let mut filtered: Vec<Hit> = Vec::new();
        for h in out {
            if let Some(last) = filtered.last() {
                if h.start >= last.start && h.end <= last.end {
                    continue;
                }
            }
            filtered.push(h);
        }
        filtered
    }

    /// 最长命中（V2 的「匹配敏感词」语义）。
    pub fn longest_hit(&self, text: &str) -> Option<Hit> {
        self.hits(text)
            .into_iter()
            .max_by_key(|h| h.word.chars().count())
    }
}

/// 守卫：对一次上传做「预检 → 决定是否改名/拦截」。
pub struct UploadGuard {
    pub instance: String,
    pub action: GuardAction,
    pub enabled: bool,
    matcher: Option<Matcher>,
    pub word_count: usize,
}

/// 预检结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// 未启用 / 未命中：原样上传
    Pass,
    /// 命中但策略是「只警告」：继续上传，附提示
    Warn { hit: String },
    /// 命中且策略是「拦截」：不上传
    Block { hit: String },
    /// 命中且策略是「自动改名」：用新名字上传
    Rename { hit: String, new_name: String },
}

impl UploadGuard {
    pub fn new(
        instance: impl Into<String>,
        action: GuardAction,
        enabled: bool,
        words: &[String],
    ) -> Self {
        let matcher = Matcher::new(words);
        Self {
            instance: instance.into(),
            action,
            enabled,
            matcher,
            word_count: words.len(),
        }
    }

    pub fn from_db(
        instance: &str,
        action: GuardAction,
        enabled: bool,
        db: &SensitiveWordDb,
    ) -> Self {
        let words = db.words_for(instance);
        Self::new(instance, action, enabled, &words)
    }

    /// 预检文件名。`kind` 用于命名规避（字符集限制）。
    pub fn check_name(&self, name: &str, kind: DriveKind) -> (GuardDecision, String) {
        if !self.enabled {
            return (GuardDecision::Pass, name.to_string());
        }
        let hit = self.matcher.as_ref().and_then(|m| m.longest_hit(name));
        let Some(hit) = hit else {
            return (GuardDecision::Pass, name.to_string());
        };
        match self.action {
            GuardAction::Warn => (GuardDecision::Warn { hit: hit.word }, name.to_string()),
            GuardAction::Block => (GuardDecision::Block { hit: hit.word }, name.to_string()),
            GuardAction::Rename => {
                let renamed = safe_rename(name, &hit);
                let renamed = avoid_illegal_chars(&renamed, kind).0;
                (
                    GuardDecision::Rename {
                        hit: hit.word,
                        new_name: renamed.clone(),
                    },
                    renamed,
                )
            }
        }
    }
}

/// 生成安全词：命中词转拼音；无拼音数据时退化为 `u<hex>`。
pub fn safe_rename(name: &str, hit: &Hit) -> String {
    let pinyin = to_pinyin(&hit.word);
    let mut out = String::new();
    out.push_str(&name[..hit.start]);
    out.push_str(&pinyin);
    out.push_str(&name[hit.end..]);
    out
}

/// 中文转拼音（无声调、小写）；非中文原样保留。
pub fn to_pinyin(text: &str) -> String {
    use pinyin::ToPinyin;
    let mut out = String::new();
    for ch in text.chars() {
        if ('\u{4e00}'..='\u{9fff}').contains(&ch) {
            match ch.to_pinyin() {
                Some(py) if !py.plain().is_empty() => out.push_str(py.plain()),
                _ => out.push_str(&format!("u{:04x}", ch as u32)),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// 命名规避：按目标网盘字符集限制清洗文件名。
/// 返回 `(新文件名, 说明)`；说明为空表示未改名。
pub fn avoid_illegal_chars(name: &str, kind: DriveKind) -> (String, String) {
    if !matches!(kind, DriveKind::Baidu | DriveKind::Local) {
        // 夸克 / 光鸭 / 演示盘：实测支持这些字符，不擅自改名
        return (name.to_string(), String::new());
    }

    // Windows/百度都不允许的字符用全角等价（百度会拒绝；Windows 本地落盘也会炸）
    let mut out = String::with_capacity(name.len());
    let mut changed = false;
    let fullwidth = |ch: char| -> Option<char> {
        match ch {
            '\\' => Some('＼'),
            '/' => Some('／'),
            ':' => Some('：'),
            '*' => Some('＊'),
            '?' => Some('？'),
            '"' => Some('＂'),
            '<' => Some('＜'),
            '>' => Some('＞'),
            '|' => Some('｜'),
            _ => None,
        }
    };
    let mut last_space = false;
    for ch in name.chars() {
        if let Some(fw) = fullwidth(ch) {
            out.push(fw);
            changed = true;
            continue;
        }
        // 控制字符 → 单个空格（连续折叠）
        if (ch as u32) < 0x20 || (0x7f..=0x9f).contains(&(ch as u32)) {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
            changed = true;
            continue;
        }
        // 星平面（emoji）→ 移除
        if (ch as u32) > 0xffff {
            changed = true;
            continue;
        }
        last_space = false;
        out.push(ch);
    }
    let mut out = out.trim().to_string();
    let mut note = String::new();
    if out.is_empty() {
        // 名字被清空：保留扩展名，前缀用原名的 md5 前 4 位
        let digest = md5_hex(name.as_bytes());
        let ext = std::path::Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
            .unwrap_or_default();
        out = format!("未命名_{}{}", &digest[..4], ext);
        changed = true;
    }
    if changed {
        note = "命名规避：目标网盘不允许的字符已替换为全角等价/移除".into();
    }
    (out, note)
}

pub fn md5_hex(bytes: &[u8]) -> String {
    use md5::Digest;
    let mut h = md5::Md5::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// 守卫统计。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct GuardStats {
    pub checked: u64,
    pub passed: u64,
    pub warned: u64,
    pub blocked: u64,
    pub renamed: u64,
}

/// 守卫管理：词库 + 统计 + 改名记录写入。
pub struct GuardManager {
    pub db: SensitiveWordDb,
    pub path: PathBuf,
    pub stats: GuardStats,
    pub action: GuardAction,
    pub enabled: bool,
}

impl GuardManager {
    pub fn load(path: &Path, action: GuardAction, enabled: bool) -> Self {
        Self {
            db: SensitiveWordDb::load(path),
            path: path.to_path_buf(),
            stats: GuardStats::default(),
            action,
            enabled,
        }
    }

    pub fn save(&self) -> crate::Result<()> {
        self.db.save(&self.path)
    }

    /// 迁移旧词库（按实例 id → 按网盘类型）并保存。
    pub fn migrate(&mut self, pairs: &[(String, String)]) {
        if self.db.migrate_instances_to_kinds(pairs) {
            let _ = self.save();
        }
    }

    /// 取某「网盘类型」的守卫（同一网盘的不同账号实例共用词库）。
    pub fn guard_for(&mut self, kind_key: &str) -> UploadGuard {
        let words = self.db.words_for(kind_key);
        UploadGuard::new(kind_key, self.action, self.enabled, &words)
    }

    /// 预检并记录统计/改名记录；返回最终用于上传的文件名。
    ///
    /// 语义：词库按**网盘类型**共享（baidu/baidu_2 同一词库）；本机（Local）不启用敏感词守卫。
    pub fn precheck(
        &mut self,
        _instance: &str,
        kind: DriveKind,
        name: &str,
    ) -> (GuardDecision, String) {
        if kind == DriveKind::Local {
            // 上传本机文件不需要敏感词守卫
            return (GuardDecision::Pass, name.to_string());
        }
        let kind_key = kind.slug().to_string();
        let guard = self.guard_for(&kind_key);
        let (decision, final_name) = guard.check_name(name, kind);
        self.stats.checked += 1;
        match &decision {
            GuardDecision::Pass => self.stats.passed += 1,
            GuardDecision::Warn { .. } => self.stats.warned += 1,
            GuardDecision::Block { .. } => self.stats.blocked += 1,
            GuardDecision::Rename { hit, new_name } => {
                self.stats.renamed += 1;
                self.db.rename_records.push(RenameRecord {
                    instance: kind_key.clone(),
                    original: name.to_string(),
                    renamed: new_name.clone(),
                    hit_word: hit.clone(),
                    strategy: "敏感词自动改名".into(),
                    time: crate::util::now_ts(),
                });
                // 只留最近 500 条
                let len = self.db.rename_records.len();
                if len > 500 {
                    self.db.rename_records.drain(0..len - 500);
                }
            }
        }
        (decision, final_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_shared_by_drive_kind_and_local_skipped() {
        let path = std::env::temp_dir().join(format!("wp-guard-kind-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut gm = GuardManager::load(&path, GuardAction::Warn, true);
        gm.db
            .instances
            .insert("baidu".into(), vec!["敏感词".into()]);
        gm.db
            .instances
            .insert("local".into(), vec!["本地词".into()]);
        // 迁移：旧实例 id 归并到网盘类型；本机词库删除
        gm.migrate(&[
            ("baidu_2".to_string(), "baidu".to_string()),
            ("local".to_string(), "local".to_string()),
        ]);
        assert!(gm.db.instances.get("local").is_none(), "本机不应有敏感词库");

        // 同一网盘不同账号实例共用词库
        let before = gm.stats.checked;
        let (d1, _) = gm.precheck("baidu_2", DriveKind::Baidu, "这是敏感词文件.txt");
        assert!(
            !matches!(d1, GuardDecision::Pass),
            "同网盘实例应共享词库：{d1:?}"
        );
        assert_eq!(gm.stats.checked, before + 1);

        // 本机跳过守卫：直接 Pass 且不计统计
        let before = gm.stats.checked;
        let (d2, name) = gm.precheck("local", DriveKind::Local, "敏感词文件.txt");
        assert!(matches!(d2, GuardDecision::Pass));
        assert_eq!(name, "敏感词文件.txt");
        assert_eq!(gm.stats.checked, before, "本机不应计入敏感词检查次数");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn match_longest() {
        let m = Matcher::new(&["敏感".to_string(), "敏感词".to_string()]).unwrap();
        let hit = m.longest_hit("这是敏感词测试").unwrap();
        assert_eq!(hit.word, "敏感词");
    }

    #[test]
    fn nested_hits_filtered() {
        let m = Matcher::new(&["abc".to_string(), "bc".to_string()]).unwrap();
        let hits = m.hits("xabcx");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].word, "abc");
    }

    #[test]
    fn pinyin_convert() {
        assert_eq!(to_pinyin("测试"), "ceshi");
        assert_eq!(to_pinyin("a中"), "azhong");
    }

    #[test]
    fn rename_uses_pinyin() {
        let m = Matcher::new(&["敏感".to_string()]).unwrap();
        let hit = m.longest_hit("这是敏感文件.txt").unwrap();
        let new_name = safe_rename("这是敏感文件.txt", &hit);
        assert_eq!(new_name, "这是mingan文件.txt");
    }

    #[test]
    fn baidu_illegal_chars() {
        let (n, note) = avoid_illegal_chars("a/b:c.txt", DriveKind::Baidu);
        assert_eq!(n, "a／b：c.txt");
        assert!(!note.is_empty());
        // 夸克不改
        let (n2, note2) = avoid_illegal_chars("a/b:c.txt", DriveKind::Quark);
        assert_eq!(n2, "a/b:c.txt");
        assert!(note2.is_empty());
    }

    #[test]
    fn control_chars_and_emoji() {
        let (n, _) = avoid_illegal_chars("a\u{1}b😀c.txt", DriveKind::Baidu);
        assert_eq!(n, "a bc.txt");
    }

    #[test]
    fn guard_decision_matrix() {
        let words = vec!["禁词".to_string()];
        let g = UploadGuard::new("baidu", GuardAction::Block, true, &words);
        assert!(matches!(
            g.check_name("含禁词名字.txt", DriveKind::Baidu).0,
            GuardDecision::Block { .. }
        ));
        let g = UploadGuard::new("baidu", GuardAction::Rename, true, &words);
        let (d, name) = g.check_name("含禁词名字.txt", DriveKind::Baidu);
        assert!(matches!(d, GuardDecision::Rename { .. }));
        assert_eq!(name, "含jinci名字.txt");
        let g = UploadGuard::new("baidu", GuardAction::Block, false, &words);
        assert_eq!(
            g.check_name("含禁词名字.txt", DriveKind::Baidu).0,
            GuardDecision::Pass
        );
    }
}
