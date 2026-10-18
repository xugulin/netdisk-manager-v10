//! AI 面板（T21 UI 骨架）：Provider 列表（启用/优先级）/ 翻译语言 / 进度占位 / 隐私提示。
//!
//! 配置对象用 `wp_ai::AiConfig`（当前只读写内存，不落盘）；
//! 「AI 翻译字幕」按钮在 T20 翻译管线就绪后接 `wp_ai` 任务队列 + `wp-subtitle::build_bilingual_ass`
//! + `PlayerCommand::AddSubtitleFile`（sub-add/sub-reload）注入。

use super::{PlayerAction, PlayerUi};
use crate::theme::Theme;
use crate::widgets;
use egui::{RichText, Ui};
use wp_ai::{AiConfig, ProviderConfig, ProviderKind};

/// AI 面板状态（跨媒体保留配置）。
pub struct AiUi {
    /// AI 全局配置（内存态；持久化见 T21）。
    pub cfg: AiConfig,
    /// 源语言（auto/zh/en/ja/ko）。
    pub source_lang: String,
    /// 目标语言。
    pub target_lang: String,
    /// 是否已经阅读隐私提示（勾选后才能点翻译）。
    pub privacy_ack: bool,
    /// 面板状态文本。
    pub status: String,
    /// 占位进度（0~1；真实接入后由任务队列事件驱动）。
    pub progress: f32,
    /// 是否已有占位任务在跑。
    pub busy: bool,
}

impl Default for AiUi {
    fn default() -> Self {
        Self {
            cfg: default_ai_config(),
            source_lang: "auto".into(),
            target_lang: "zh".into(),
            privacy_ack: false,
            status: "未开始：翻译管线（T20）接入后可一键生成双语字幕".into(),
            progress: 0.0,
            busy: false,
        }
    }
}

/// 默认 Provider 列表：远端 OpenAI 兼容 / 本地 LLM / 本地 Whisper。
pub fn default_ai_config() -> AiConfig {
    AiConfig {
        providers: vec![
            ProviderConfig {
                id: "remote-openai".into(),
                kind: ProviderKind::RemoteOpenAi,
                enabled: false,
                priority: 10,
                base_url: "https://api.openai.com/v1".into(),
                model: "gpt-4o-mini".into(),
                api_key: String::new(),
                ..Default::default()
            },
            ProviderConfig {
                id: "local-llm".into(),
                kind: ProviderKind::LocalLlm,
                enabled: true,
                priority: 20,
                base_url: String::new(),
                model: "qwen2.5-7b-instruct".into(),
                api_key: String::new(),
                ..Default::default()
            },
            ProviderConfig {
                id: "local-whisper".into(),
                kind: ProviderKind::LocalWhisper,
                enabled: false,
                priority: 30,
                base_url: String::new(),
                model: "whisper-medium".into(),
                api_key: String::new(),
                ..Default::default()
            },
        ],
        offline_only: true,
        allow_during_playback: false,
    }
}

/// Provider 类型 → 中文标签（保留 `_` 兜底以兼容 wp-ai 后续新增类型）。
pub fn kind_label(kind: &ProviderKind) -> &'static str {
    match kind {
        ProviderKind::RemoteOpenAi => "远端 OpenAI 兼容",
        ProviderKind::LocalLlm => "本地 LLM",
        ProviderKind::LocalWhisper => "本地 Whisper",
        _ => "其它 / ONNX",
    }
}

/// 优先级上移/下移：交换列表位置后重排优先级数值（10/20/30…，越小越优先）。
/// 返回是否发生移动（越界不移动）。
pub fn move_provider(providers: &mut [ProviderConfig], index: usize, up: bool) -> bool {
    if index >= providers.len() {
        return false;
    }
    let target = if up {
        if index == 0 {
            return false;
        }
        index - 1
    } else {
        if index + 1 >= providers.len() {
            return false;
        }
        index + 1
    };
    providers.swap(index, target);
    renumber_priorities(providers);
    true
}

/// 重排优先级数值：10, 20, 30…（越小越优先）。
pub fn renumber_priorities(providers: &mut [ProviderConfig]) {
    for (i, p) in providers.iter_mut().enumerate() {
        p.priority = ((i as u32 + 1) * 10).min(u8::MAX as u32) as u8;
    }
}

/// 启用的 Provider 概要（面板顶部状态行用）。
pub fn enabled_summary(cfg: &AiConfig) -> String {
    let enabled: Vec<&str> = cfg
        .providers
        .iter()
        .filter(|p| p.enabled)
        .map(|p| p.id.as_str())
        .collect();
    if enabled.is_empty() {
        "无可用 Provider（全部停用）".into()
    } else {
        format!("启用：{}", enabled.join(" / "))
    }
}

const LANGS: [(&str, &str); 6] = [
    ("auto", "自动识别"),
    ("zh", "中文"),
    ("en", "英语"),
    ("ja", "日语"),
    ("ko", "韩语"),
    ("zxx", "不翻译"),
];

fn lang_label(code: &str) -> String {
    LANGS
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, label)| (*label).to_string())
        .unwrap_or_else(|| code.to_string())
}

/// 渲染 AI 面板。
pub fn show(player: &mut PlayerUi, t: &Theme, ui: &mut Ui, actions: &mut Vec<PlayerAction>) {
    widgets::section_title(ui, t, "AI 助手");
    ui.add_space(2.0);

    // ── 状态行 ──
    let enabled = enabled_summary(&player.ai.cfg);
    widgets::hint(ui, t, &enabled);
    ui.label(
        RichText::new(&player.ai.status)
            .size(12.0)
            .color(if player.ai.busy {
                t.warning
            } else {
                t.text_weak
            }),
    );
    ui.add(egui::ProgressBar::new(player.ai.progress.clamp(0.0, 1.0)).show_percentage());

    // ── 翻译语言 ──
    ui.add_space(6.0);
    ui.label(RichText::new("字幕翻译").size(13.0).strong().color(t.text));
    ui.horizontal(|ui| {
        ui.label(RichText::new("源语言").size(12.0).color(t.text_weak));
        egui::ComboBox::from_id_salt("ai_source_lang")
            .width(110.0)
            .selected_text(lang_label(&player.ai.source_lang))
            .show_ui(ui, |ui| {
                for (code, label) in LANGS {
                    ui.selectable_value(&mut player.ai.source_lang, code.to_string(), label);
                }
            });
        ui.label(RichText::new("→").size(12.0).color(t.text_weak));
        egui::ComboBox::from_id_salt("ai_target_lang")
            .width(110.0)
            .selected_text(lang_label(&player.ai.target_lang))
            .show_ui(ui, |ui| {
                for (code, label) in LANGS {
                    if code == "auto" {
                        continue; // 目标语言不支持 auto
                    }
                    ui.selectable_value(&mut player.ai.target_lang, code.to_string(), label);
                }
            });
    });

    // ── 隐私提示 + 翻译按钮 ──
    widgets::hint(
        ui,
        t,
        "隐私：默认本地优先（离线模式）；远端 Provider 会把字幕文本发送到你配置的服务。\n\
         API Key 目前只保存在内存，不写日志；持久化/系统钥匙串见 T21。",
    );
    ui.checkbox(
        &mut player.ai.privacy_ack,
        "我已了解数据外发范围（远端 Provider 时必选）",
    );
    ui.horizontal(|ui| {
        let can_translate = player.ai.cfg.providers.iter().any(|p| p.enabled)
            && !player.ai.busy
            && (player.ai.privacy_ack
                || !player
                    .ai
                    .cfg
                    .providers
                    .iter()
                    .any(|p| p.enabled && p.kind == wp_ai::ProviderKind::RemoteOpenAi));
        let btn = widgets::primary_button(ui, t, "🌐 翻译当前字幕（占位）");
        if btn.clicked() {
            if !can_translate {
                actions.push(PlayerAction::Toast(
                    crate::app::ToastKind::Warn,
                    "请先启用 Provider 并勾选隐私提示".into(),
                ));
            } else {
                player.ai.busy = true;
                player.ai.progress = 0.0;
                player.ai.status = format!(
                    "占位任务已提交：{} → {}（等待 wp-ai 翻译管线 T20 接入）",
                    lang_label(&player.ai.source_lang),
                    lang_label(&player.ai.target_lang)
                );
                actions.push(PlayerAction::Toast(
                    crate::app::ToastKind::Info,
                    "AI 翻译为占位按钮：管线就绪后直接生成双语 ASS 并注入副字幕轨".into(),
                ));
            }
        }
        if player.ai.busy && ui.button("取消").clicked() {
            player.ai.busy = false;
            player.ai.status = "已取消（占位）".into();
        }
    });

    // ── 全局开关 ──
    ui.add_space(6.0);
    ui.checkbox(
        &mut player.ai.cfg.offline_only,
        "离线优先（只用本地 Provider）",
    );
    ui.checkbox(
        &mut player.ai.cfg.allow_during_playback,
        "播放期间允许 AI 重任务（默认关，保护 4K 解码）",
    );

    // ── Provider 列表 ──
    ui.add_space(6.0);
    ui.label(RichText::new("Provider").size(13.0).strong().color(t.text));
    let mut move_up: Option<usize> = None;
    let mut move_down: Option<usize> = None;
    let count = player.ai.cfg.providers.len();
    for (idx, provider) in player.ai.cfg.providers.iter_mut().enumerate() {
        egui::Frame::NONE
            .fill(t.card)
            .stroke(egui::Stroke::new(1.0, t.border))
            .corner_radius(crate::theme::radius(6))
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut provider.enabled, "");
                    ui.label(RichText::new(&provider.id).size(12.5).strong().color(
                        if provider.enabled {
                            t.text
                        } else {
                            t.text_weak
                        },
                    ));
                    ui.label(
                        RichText::new(kind_label(&provider.kind))
                            .size(11.0)
                            .color(t.text_weak),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(idx + 1 < count, egui::Button::new("↓").small())
                            .on_hover_text("降低优先级")
                            .clicked()
                        {
                            move_down = Some(idx);
                        }
                        if ui
                            .add_enabled(idx > 0, egui::Button::new("↑").small())
                            .on_hover_text("提高优先级")
                            .clicked()
                        {
                            move_up = Some(idx);
                        }
                        ui.label(
                            RichText::new(format!("#{}", provider.priority))
                                .size(10.5)
                                .color(t.text_weak),
                        );
                    });
                });
                // 远端 Provider 才需要 base_url / model / api_key
                if provider.kind == wp_ai::ProviderKind::RemoteOpenAi {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Base URL").size(11.0).color(t.text_weak));
                        ui.add(
                            egui::TextEdit::singleline(&mut provider.base_url)
                                .desired_width(190.0)
                                .hint_text("https://api.openai.com/v1"),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("模型").size(11.0).color(t.text_weak));
                        ui.add(
                            egui::TextEdit::singleline(&mut provider.model)
                                .desired_width(130.0)
                                .hint_text("gpt-4o-mini"),
                        );
                        ui.label(RichText::new("API Key").size(11.0).color(t.text_weak));
                        ui.add(
                            egui::TextEdit::singleline(&mut provider.api_key)
                                .password(true)
                                .desired_width(120.0)
                                .hint_text("仅内存"),
                        );
                    });
                } else {
                    ui.label(
                        RichText::new(format!("模型：{}", provider.model))
                            .size(11.0)
                            .color(t.text_weak),
                    );
                }
            });
        ui.add_space(4.0);
    }
    // 优先队列重排（在遍历结束后应用，避免遍历中改 Vec）
    if let Some(idx) = move_up {
        move_provider(&mut player.ai.cfg.providers, idx, true);
    } else if let Some(idx) = move_down {
        move_provider(&mut player.ai.cfg.providers, idx, false);
    }

    // ── 状态占位 ──
    ui.add_space(4.0);
    let queue_state = if player.ai.busy {
        "任务队列：1 个占位任务进行中（真实队列由 wp-ai T18 提供）"
    } else {
        "任务队列：空闲（真实队列由 wp-ai T18 提供）"
    };
    widgets::hint(ui, t, queue_state);
    widgets::hint(
        ui,
        t,
        "后续：整片/实时前瞻翻译 · 术语表 · 繁简转换 · ASR（P2）· 摘要/问答（P3）",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_expected_providers() {
        let cfg = default_ai_config();
        assert_eq!(cfg.providers.len(), 3);
        assert!(cfg.offline_only);
        assert!(!cfg.allow_during_playback);
        // 默认只有本地 LLM 启用
        let enabled: Vec<&str> = cfg
            .providers
            .iter()
            .filter(|p| p.enabled)
            .map(|p| p.id.as_str())
            .collect();
        assert_eq!(enabled, vec!["local-llm"]);
        // 优先级递增
        assert!(cfg.providers[0].priority < cfg.providers[1].priority);
    }

    #[test]
    fn move_provider_reorders_and_renumbers() {
        let mut cfg = default_ai_config();
        assert!(move_provider(&mut cfg.providers, 2, true));
        let ids: Vec<&str> = cfg.providers.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["remote-openai", "local-whisper", "local-llm"]);
        assert_eq!(
            cfg.providers.iter().map(|p| p.priority).collect::<Vec<_>>(),
            vec![10, 20, 30]
        );
        // 越界不移动
        assert!(!move_provider(&mut cfg.providers, 0, true));
        assert!(!move_provider(&mut cfg.providers, 2, false));
        assert!(!move_provider(&mut cfg.providers, 9, false));
    }

    #[test]
    fn enabled_summary_reports() {
        let mut cfg = default_ai_config();
        assert!(enabled_summary(&cfg).contains("local-llm"));
        for p in &mut cfg.providers {
            p.enabled = false;
        }
        assert!(enabled_summary(&cfg).contains("无可用"));
    }

    #[test]
    fn kind_labels_cover_known_kinds() {
        assert_eq!(kind_label(&ProviderKind::RemoteOpenAi), "远端 OpenAI 兼容");
        assert_eq!(kind_label(&ProviderKind::LocalLlm), "本地 LLM");
        assert_eq!(kind_label(&ProviderKind::LocalWhisper), "本地 Whisper");
    }

    #[test]
    fn lang_label_falls_back_to_code() {
        assert_eq!(lang_label("zh"), "中文");
        assert_eq!(lang_label("xx"), "xx");
    }
}
