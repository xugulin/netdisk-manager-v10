//! 弹幕屏蔽与去重辅助。
//!
//! 屏蔽规则（全部大小写可选）：
//! - 关键词：命中任意子串即屏蔽
//! - 简单通配符模式：`*`（任意串）/`?`（任意单字符），**整段匹配**；
//!   想「包含」可用 `*词*`。不引入 regex crate（简单匹配已够用，且 Windows 打包更轻）
//! - 用户：精确匹配发送者（uid/哈希）
//! - 类型：按滚动/顶部/底部整类屏蔽
//!
//! 另提供：重复弹幕折叠（同文本 + 时间窗内只留一条）、时间轴密度曲线（热力图用）。

use crate::model::{Comment, DanmakuMode};
use serde::{Deserialize, Serialize};

/// 屏蔽原因（面板展示/调试用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterReason {
    /// 命中关键词
    Keyword,
    /// 命中通配符模式
    Pattern,
    /// 发送者被屏蔽
    User,
    /// 该类型弹幕被屏蔽
    Mode,
}

/// 弹幕屏蔽规则集。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DanmakuFilter {
    /// 关键词（子串匹配）。
    pub keywords: Vec<String>,
    /// 通配符模式（`*`/`?`，整段匹配）。
    pub patterns: Vec<String>,
    /// 屏蔽的发送者（精确匹配）。
    pub blocked_users: Vec<String>,
    /// 屏蔽的弹幕类型。
    pub blocked_modes: Vec<DanmakuMode>,
    /// 是否区分大小写（默认不区分；中文无影响）。
    pub case_sensitive: bool,
}

impl DanmakuFilter {
    /// 空规则（不屏蔽任何弹幕）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否屏蔽该弹幕。
    pub fn is_blocked(&self, comment: &Comment) -> bool {
        self.reason(comment).is_some()
    }

    /// 命中哪条规则（未命中返回 None）。
    pub fn reason(&self, comment: &Comment) -> Option<FilterReason> {
        if self.blocked_modes.contains(&comment.mode) {
            return Some(FilterReason::Mode);
        }
        let text = normalize(&comment.text, self.case_sensitive);
        for keyword in &self.keywords {
            let keyword = normalize(keyword, self.case_sensitive);
            if !keyword.is_empty() && text.contains(&keyword) {
                return Some(FilterReason::Keyword);
            }
        }
        for pattern in &self.patterns {
            if !pattern.is_empty() && glob_match(&text, &normalize(pattern, self.case_sensitive)) {
                return Some(FilterReason::Pattern);
            }
        }
        if let Some(user) = &comment.user {
            if self.blocked_users.iter().any(|blocked| {
                blocked == user
                    || normalize(blocked, self.case_sensitive)
                        == normalize(user, self.case_sensitive)
            }) {
                return Some(FilterReason::User);
            }
        }
        None
    }

    /// 过滤后保留的弹幕（克隆；保持原顺序）。
    pub fn apply(&self, comments: &[Comment]) -> Vec<Comment> {
        comments
            .iter()
            .filter(|comment| !self.is_blocked(comment))
            .cloned()
            .collect()
    }

    /// 就地过滤。
    pub fn retain(&self, comments: &mut Vec<Comment>) {
        comments.retain(|comment| !self.is_blocked(comment));
    }

    /// 屏蔽的弹幕数（统计展示用）。
    pub fn blocked_count(&self, comments: &[Comment]) -> usize {
        comments
            .iter()
            .filter(|comment| self.is_blocked(comment))
            .count()
    }
}

fn normalize(text: &str, case_sensitive: bool) -> String {
    if case_sensitive {
        text.to_string()
    } else {
        text.to_lowercase()
    }
}

/// 简单通配符匹配：`*` 任意长度、`?` 单字符，整段匹配（迭代实现，O(n*m) 最坏）。
pub fn glob_match(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let (mut ti, mut pi) = (0usize, 0usize);
    let (mut star, mut match_from) = (None::<usize>, 0usize);
    while ti < text.len() {
        if pi < pattern.len() && (pattern[pi] == '?' || pattern[pi] == text[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < pattern.len() && pattern[pi] == '*' {
            star = Some(pi);
            match_from = ti;
            pi += 1;
        } else if let Some(star_pi) = star {
            pi = star_pi + 1;
            match_from += 1;
            ti = match_from;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }
    pi == pattern.len()
}

/// 重复弹幕折叠：相同文本且出现时间在 `window_ms` 内的只保留第一条。
pub fn dedupe_comments(comments: &[Comment], window_ms: i64) -> Vec<Comment> {
    let window_ms = window_ms.max(0);
    let mut seen: std::collections::HashMap<&str, i64> = std::collections::HashMap::new();
    let mut out: Vec<Comment> = Vec::with_capacity(comments.len());
    for comment in comments {
        match seen.get(comment.text.as_str()) {
            Some(last) if comment.time_ms - *last <= window_ms => continue,
            _ => {
                seen.insert(comment.text.as_str(), comment.time_ms);
                out.push(comment.clone());
            }
        }
    }
    out
}

/// 时间轴密度曲线：按 `bucket_ms` 分桶统计弹幕数（热力图/精华片段用）。
///
/// 返回 `(桶起点毫秒, 条数)`，桶从 0 开始按时间轴排列，连续无空洞。
pub fn density_curve(comments: &[Comment], bucket_ms: i64) -> Vec<(i64, usize)> {
    if comments.is_empty() {
        return Vec::new();
    }
    let bucket_ms = bucket_ms.max(1);
    let max_time = comments.iter().map(|c| c.time_ms.max(0)).max().unwrap_or(0);
    let buckets = (max_time / bucket_ms + 1) as usize;
    let mut counts = vec![0usize; buckets];
    for comment in comments {
        let index = (comment.time_ms.max(0) / bucket_ms) as usize;
        if let Some(slot) = counts.get_mut(index) {
            *slot += 1;
        }
    }
    counts
        .into_iter()
        .enumerate()
        .map(|(i, n)| (i as i64 * bucket_ms, n))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(text: &str) -> Comment {
        Comment::new(1000, DanmakuMode::Scroll, text)
    }

    #[test]
    fn keyword_filter() {
        let filter = DanmakuFilter {
            keywords: vec!["剧透".into(), "spoiler".into()],
            ..DanmakuFilter::new()
        };
        assert!(filter.is_blocked(&comment("这里有剧透")));
        assert!(filter.is_blocked(&comment("A SPOILER here"))); // 默认不区分大小写
        assert!(!filter.is_blocked(&comment("正常弹幕")));
        assert_eq!(filter.reason(&comment("剧透")), Some(FilterReason::Keyword));
    }

    #[test]
    fn case_sensitive_option() {
        let filter = DanmakuFilter {
            keywords: vec!["ABC".into()],
            case_sensitive: true,
            ..DanmakuFilter::new()
        };
        assert!(!filter.is_blocked(&comment("abc")));
        assert!(filter.is_blocked(&comment("ABC")));
    }

    #[test]
    fn glob_patterns() {
        assert!(glob_match("abc", "abc"));
        assert!(glob_match("abcdef", "abc*"));
        assert!(glob_match("xyzabc", "*abc"));
        assert!(glob_match("aXc", "a?c"));
        assert!(glob_match("", "*"));
        assert!(!glob_match("abc", "abd"));
        assert!(!glob_match("abc", "a?"));
        let filter = DanmakuFilter {
            patterns: vec!["*广告*".into(), "233+".into()],
            ..DanmakuFilter::new()
        };
        assert!(filter.is_blocked(&comment("这是广告内容")));
        assert!(!filter.is_blocked(&comment("普通内容")));
        assert_eq!(filter.reason(&comment("广告")), Some(FilterReason::Pattern));
    }

    #[test]
    fn user_and_mode_filter() {
        let mut c1 = comment("用户弹幕");
        c1.user = Some("uid123".into());
        let mut c2 = Comment::new(0, DanmakuMode::Top, "顶部弹幕");
        c2.user = Some("uid456".into());
        let filter = DanmakuFilter {
            blocked_users: vec!["uid123".into()],
            blocked_modes: vec![DanmakuMode::Top],
            ..DanmakuFilter::new()
        };
        assert_eq!(filter.reason(&c1), Some(FilterReason::User));
        assert_eq!(filter.reason(&c2), Some(FilterReason::Mode));
        assert!(!filter.is_blocked(&comment("普通")));
    }

    #[test]
    fn apply_and_retain() {
        let filter = DanmakuFilter {
            keywords: vec!["屏蔽".into()],
            ..DanmakuFilter::new()
        };
        let comments = vec![comment("保留1"), comment("屏蔽掉"), comment("保留2")];
        let kept = filter.apply(&comments);
        assert_eq!(kept.len(), 2);
        assert_eq!(filter.blocked_count(&comments), 1);
        let mut owned = comments.clone();
        filter.retain(&mut owned);
        assert_eq!(owned.len(), 2);
    }

    #[test]
    fn empty_filter_passes_all() {
        let filter = DanmakuFilter::new();
        let comments = vec![comment("a"), comment("b")];
        assert_eq!(filter.apply(&comments).len(), 2);
        assert_eq!(filter.blocked_count(&comments), 0);
    }

    #[test]
    fn dedupe_within_window() {
        let comments = vec![
            Comment::new(0, DanmakuMode::Scroll, "233"),
            Comment::new(1000, DanmakuMode::Scroll, "233"),
            Comment::new(9000, DanmakuMode::Scroll, "233"),
            Comment::new(1200, DanmakuMode::Scroll, "666"),
        ];
        // 5s 窗口：前两条折叠，9s 那条保留（保持原顺序）
        let out = dedupe_comments(&comments, 5000);
        assert_eq!(out.len(), 3);
        assert_eq!(out[1].text, "233");
        assert_eq!(out[1].time_ms, 9000);
        assert_eq!(out[2].text, "666");
        // 0 窗口只折叠完全同时间的重复
        let out0 = dedupe_comments(&comments, 0);
        assert_eq!(out0.len(), 4);
    }

    #[test]
    fn density_curve_buckets() {
        let comments = vec![
            Comment::new(0, DanmakuMode::Scroll, "a"),
            Comment::new(500, DanmakuMode::Scroll, "b"),
            Comment::new(1500, DanmakuMode::Scroll, "c"),
            Comment::new(3500, DanmakuMode::Scroll, "d"),
        ];
        let curve = density_curve(&comments, 1000);
        assert_eq!(curve.len(), 4);
        assert_eq!(curve[0], (0, 2));
        assert_eq!(curve[1], (1000, 1));
        assert_eq!(curve[2], (2000, 0));
        assert_eq!(curve[3], (3000, 1));
        assert!(density_curve(&[], 1000).is_empty());
    }
}
