//! 字幕面板（T8/T10 UI）：轨道选择 / 主副字幕 / 延迟 / 字号·位置·样式预览 / 外挂注入。
//!
//! 所有可映射的操作都会发出 [`PlayerCommand`]（Mock 引擎忽略，MpvEngine 生效）；
//! 字号/位置/描边/颜色属于 libass 样式覆盖，先做 UI + 本地预览，
//! 等 `wp-media-mpv` 暴露 `sub-ass-override/--secondary-sub-*` 属性后再发命令。

use super::{PlayerAction, PlayerUi};
use crate::theme::Theme;
use crate::widgets;
use egui::{Color32, RichText, Ui};
use std::path::PathBuf;
use wp_media::{PlayerCommand, TrackInfo, TrackKind};

/// 字幕面板状态（跨媒体保留用户偏好）。
pub struct SubtitleUi {
    /// 主字幕开关（S 快捷键 / 面板开关）。
    pub enabled: bool,
    /// 副字幕可见性（`--secondary-sub-visible`）。
    pub secondary_visible: bool,
    /// 副字幕轨 id（mpv `secondary-sid`；None=关闭）。多字幕轨时默认第二条。
    pub secondary_track: Option<i64>,
    /// 字体目录（`MpvConfig.fonts_dir` / `--sub-fonts-dir`；重建引擎后生效）。
    pub fonts_dir: String,
    /// 当前选中的字幕轨 id（None=关闭）。
    pub selected: Option<i64>,
    /// 字幕延迟（秒，正=延后）。
    pub delay_secs: f64,
    /// 预览字号（egui 点，叠加视频高度缩放）。
    pub font_size: f32,
    /// 预览位置（距视频底部比例 0~0.4）。
    pub margin_ratio: f32,
    /// 预览描边粗细（0~4）。
    pub outline: f32,
    /// 预览字色。
    pub color: Color32,
    /// 样式预设（UI 占位，后续映射 libass 样式覆盖）。
    pub style_preset: String,
    /// 待导入的外挂字幕路径（用户手输）。
    pub import_path: String,
    /// 当前主字幕文本（`PlayerEvent::SubtitleCue` 回填；Mock 用演示 cue）。
    pub current_cue: Option<String>,
    /// 当前副字幕文本。
    pub secondary_cue: Option<String>,
    /// 面板内提示（导入结果/错误）。
    pub status: String,
}

impl Default for SubtitleUi {
    fn default() -> Self {
        Self {
            enabled: true,
            secondary_visible: false,
            secondary_track: None,
            fonts_dir: String::new(),
            selected: None,
            delay_secs: 0.0,
            font_size: 1.0,
            margin_ratio: 0.085,
            outline: 2.0,
            color: Color32::WHITE,
            style_preset: "默认（跟随片源）".into(),
            import_path: String::new(),
            current_cue: None,
            secondary_cue: None,
            status: String::new(),
        }
    }
}

impl SubtitleUi {
    /// 字号倍率（1.0 = 基准字号，叠加视频高度计算像素）。
    pub fn font_scale(&self) -> f32 {
        self.font_size.clamp(0.5, 3.0)
    }

    pub fn color(&self) -> Color32 {
        self.color
    }

    /// 媒体打开/轨道变化时同步轨道选择。
    pub fn on_tracks(&mut self, tracks: &[TrackInfo]) {
        let subs: Vec<&TrackInfo> = tracks
            .iter()
            .filter(|tr| tr.kind == Some(TrackKind::Subtitle))
            .collect();
        let selected = subs
            .iter()
            .find(|tr| tr.selected)
            .or_else(|| subs.first())
            .map(|tr| tr.id);
        self.selected = selected;
        self.enabled = selected.is_some();
        // 副字幕轨默认取第二条字幕轨（多字幕时）；少于 2 条则关闭（mpv secondary-sid=no）。
        let sub_ids: Vec<i64> = subs.iter().map(|tr| tr.id).collect();
        self.secondary_track = if sub_ids.len() >= 2 {
            Some(sub_ids[1])
        } else {
            None
        };
        self.current_cue = None;
        self.secondary_cue = None;
    }

    /// 清空当前 cue（关闭媒体时）。
    pub fn reset(&mut self) {
        self.current_cue = None;
        self.secondary_cue = None;
        self.status.clear();
    }

    /// 生成「关字幕/开字幕」对应的引擎命令。
    pub fn toggle_command(&mut self) -> PlayerCommand {
        self.enabled = !self.enabled;
        let id = if self.enabled {
            self.selected.unwrap_or(1)
        } else {
            -1
        };
        PlayerCommand::SelectTrack {
            kind: TrackKind::Subtitle,
            id,
        }
    }
}

/// 渲染字幕面板。
pub fn show(player: &mut PlayerUi, t: &Theme, ui: &mut Ui, actions: &mut Vec<PlayerAction>) {
    let mut cmds: Vec<PlayerCommand> = Vec::new();
    widgets::section_title(ui, t, "字幕");
    ui.add_space(2.0);

    // ── 主/副字幕开关 ──
    let mut enabled = player.subtitle.enabled;
    if ui
        .checkbox(&mut enabled, "主字幕显示（S）")
        .on_hover_text("也可以通过键盘 S 快速开关")
        .changed()
    {
        player.subtitle.enabled = enabled;
        let id = if enabled {
            player.subtitle.selected.unwrap_or(1)
        } else {
            -1
        };
        cmds.push(PlayerCommand::SelectTrack {
            kind: TrackKind::Subtitle,
            id,
        });
    }
    let mut secondary = player.subtitle.secondary_visible;
    if ui
        .checkbox(&mut secondary, "副字幕（双语/AI 译文轨）")
        .on_hover_text("对应 mpv --secondary-sub-visible；AI 双语字幕注入后作为副轨显示")
        .changed()
    {
        player.subtitle.secondary_visible = secondary;
        cmds.push(PlayerCommand::SetSecondarySubtitleVisible(secondary));
    }

    // ── 轨道列表 ──
    ui.add_space(6.0);
    ui.label(RichText::new("字幕轨").size(13.0).strong().color(t.text));
    let tracks: Vec<TrackInfo> = player
        .snapshot
        .tracks
        .iter()
        .filter(|tr| tr.kind == Some(TrackKind::Subtitle))
        .cloned()
        .collect();
    if tracks.is_empty() {
        widgets::hint(ui, t, "当前媒体没有内嵌字幕轨；可导入外挂 SRT/ASS（见下）");
    } else {
        ui.horizontal_wrapped(|ui| {
            let is_off = player.subtitle.selected.is_none() || !player.subtitle.enabled;
            if ui.selectable_label(is_off, "关闭").clicked() {
                player.subtitle.selected = None;
                player.subtitle.enabled = false;
                cmds.push(PlayerCommand::SelectTrack {
                    kind: TrackKind::Subtitle,
                    id: -1,
                });
            }
            for tr in &tracks {
                let label = format!(
                    "{} {}{}",
                    tr.id,
                    if tr.title.is_empty() {
                        "字幕轨".to_string()
                    } else {
                        tr.title.clone()
                    },
                    if tr.lang.is_empty() {
                        String::new()
                    } else {
                        format!("（{}）", tr.lang)
                    }
                );
                let selected = player.subtitle.enabled && player.subtitle.selected == Some(tr.id);
                if ui.selectable_label(selected, label).clicked() {
                    player.subtitle.selected = Some(tr.id);
                    player.subtitle.enabled = true;
                    cmds.push(PlayerCommand::SelectTrack {
                        kind: TrackKind::Subtitle,
                        id: tr.id,
                    });
                }
            }
        });
        // 轨道详情（编码/外挂标记）
        if let Some(tr) = tracks
            .iter()
            .find(|tr| Some(tr.id) == player.subtitle.selected)
        {
            let mut meta = vec![
                super::track_kind_label(tr.kind).to_string(),
                tr.codec.clone(),
            ];
            if !tr.lang.is_empty() {
                meta.push(format!("语言 {}", tr.lang));
            }
            meta.push(if tr.external {
                "外挂字幕".into()
            } else {
                "内嵌字幕".into()
            });
            widgets::hint(ui, t, &meta.join(" · "));
        }
    }

    // ── 副字幕轨（mpv `secondary-sid`；AI 双语/译文轨走副字幕层） ──
    ui.add_space(6.0);
    ui.label(RichText::new("副字幕轨").size(13.0).strong().color(t.text));
    ui.horizontal(|ui| {
        let current = player.subtitle.secondary_track;
        let selected_text = match current {
            Some(id) => tracks
                .iter()
                .find(|tr| tr.id == id)
                .map(|tr| {
                    if tr.title.is_empty() {
                        format!("轨 {id}（{}）", tr.codec)
                    } else {
                        format!("轨 {id} {}", tr.title)
                    }
                })
                .unwrap_or_else(|| format!("轨 {id}")),
            None => "关闭（无副字幕）".to_string(),
        };
        egui::ComboBox::from_id_salt("secondary_subtitle_track")
            .width(190.0)
            .selected_text(selected_text)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(current.is_none(), "关闭（无副字幕）")
                    .clicked()
                {
                    player.subtitle.secondary_track = None;
                    cmds.push(PlayerCommand::SelectSecondarySubtitleTrack { id: -1 });
                }
                for tr in &tracks {
                    let label = if tr.title.is_empty() {
                        format!("轨 {}（{}）", tr.id, tr.codec)
                    } else {
                        format!("轨 {} {}", tr.id, tr.title)
                    };
                    if ui.selectable_label(current == Some(tr.id), label).clicked() {
                        player.subtitle.secondary_track = Some(tr.id);
                        cmds.push(PlayerCommand::SelectSecondarySubtitleTrack { id: tr.id });
                    }
                }
            });
    });
    widgets::hint(
        ui,
        t,
        "副字幕轨 = mpv secondary-sid（AI 双语译文轨放这里；主+副可同屏）",
    );

    // ── 字体目录（MpvConfig.fonts_dir / --sub-fonts-dir，open 前生效） ──
    ui.add_space(6.0);
    ui.label(RichText::new("字体目录").size(13.0).strong().color(t.text));
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut player.subtitle.fonts_dir)
                .desired_width(210.0)
                .hint_text("/path/to/fonts"),
        );
        if ui.small_button("应用").clicked() {
            let dir = player.subtitle.fonts_dir.trim().to_string();
            player.subtitle.status = if dir.is_empty() {
                "字体目录已清空（重建播放引擎后生效）".to_string()
            } else {
                format!("字体目录已记录（重建播放引擎后生效）：{dir}")
            };
        }
    });
    widgets::hint(
        ui,
        t,
        "对应 MpvConfig.fonts_dir；mpv 初始化时扫描字体目录，需重建引擎生效",
    );

    // ── 同步（延迟） ──
    ui.add_space(8.0);
    ui.label(
        RichText::new("同步 / 延迟")
            .size(13.0)
            .strong()
            .color(t.text),
    );
    ui.horizontal(|ui| {
        let resp = ui.add(
            egui::Slider::new(&mut player.subtitle.delay_secs, -10.0..=10.0)
                .step_by(0.05)
                .suffix(" s")
                .text("延迟"),
        );
        if resp.changed() {
            cmds.push(PlayerCommand::SetSubtitleDelay(player.subtitle.delay_secs));
        }
        if ui.small_button("归零").clicked() {
            player.subtitle.delay_secs = 0.0;
            cmds.push(PlayerCommand::SetSubtitleDelay(0.0));
        }
    });
    ui.horizontal(|ui| {
        if ui.small_button("－0.1s").clicked() {
            player.subtitle.delay_secs = (player.subtitle.delay_secs - 0.1).clamp(-10.0, 10.0);
            cmds.push(PlayerCommand::SetSubtitleDelay(player.subtitle.delay_secs));
        }
        if ui.small_button("＋0.1s").clicked() {
            player.subtitle.delay_secs = (player.subtitle.delay_secs + 0.1).clamp(-10.0, 10.0);
            cmds.push(PlayerCommand::SetSubtitleDelay(player.subtitle.delay_secs));
        }
        widgets::hint(ui, t, "正=字幕延后出现");
    });

    // ── 样式（预览；libass 样式覆盖后续接入） ──
    ui.add_space(8.0);
    ui.label(RichText::new("样式").size(13.0).strong().color(t.text));
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("sub_style_preset")
            .width(150.0)
            .selected_text(player.subtitle.style_preset.clone())
            .show_ui(ui, |ui| {
                for preset in [
                    "默认（跟随片源）",
                    "大字号高对比",
                    "描边加粗",
                    "底部贴边",
                    "顶部显示",
                ] {
                    if ui
                        .selectable_label(player.subtitle.style_preset == preset, preset)
                        .clicked()
                    {
                        player.subtitle.style_preset = preset.to_string();
                        apply_style_preset(&mut player.subtitle, preset);
                    }
                }
            });
        if ui.small_button("重置样式").clicked() {
            player.subtitle.font_size = 1.0;
            player.subtitle.margin_ratio = 0.085;
            player.subtitle.outline = 2.0;
            player.subtitle.color = Color32::WHITE;
            player.subtitle.style_preset = "默认（跟随片源）".into();
        }
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("字号").size(12.0).color(t.text_weak));
        ui.add(
            egui::Slider::new(&mut player.subtitle.font_size, 0.5..=3.0)
                .show_value(true)
                .fixed_decimals(2)
                .suffix("x"),
        );
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("位置").size(12.0).color(t.text_weak));
        ui.add(
            egui::Slider::new(&mut player.subtitle.margin_ratio, 0.0..=0.40)
                .show_value(true)
                .fixed_decimals(2)
                .suffix(" 屏高"),
        );
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("描边").size(12.0).color(t.text_weak));
        ui.add(egui::Slider::new(&mut player.subtitle.outline, 0.0..=4.0).show_value(true));
        ui.label(RichText::new("颜色").size(12.0).color(t.text_weak));
        ui.color_edit_button_srgba(&mut player.subtitle.color);
    });
    widgets::hint(
        ui,
        t,
        "样式覆盖（--sub-ass-override / 位置 / 颜色）在 MpvEngine 属性接入后生效；当前为预览。",
    );

    // ── 外挂字幕导入 / 重新加载 ──
    ui.add_space(8.0);
    ui.label(RichText::new("外挂字幕").size(13.0).strong().color(t.text));
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut player.subtitle.import_path)
                .desired_width(180.0)
                .hint_text("字幕文件路径（.srt/.ass/.vtt）"),
        );
        if ui.button("导入").clicked() {
            let path = player.subtitle.import_path.trim().to_string();
            if path.is_empty() {
                player.subtitle.status = "请先填写字幕文件路径".into();
            } else {
                cmds.push(PlayerCommand::AddSubtitleFile {
                    path: PathBuf::from(&path),
                    select: true,
                });
                player.subtitle.status = format!("已请求加载外挂字幕：{path}");
            }
        }
    });
    ui.horizontal(|ui| {
        if ui.button("↻ 重新加载字幕").clicked() {
            cmds.push(PlayerCommand::ReloadSubtitles);
            player.subtitle.status = "已请求 sub-reload（AI 更新字幕后调用）".into();
        }
        if ui.button("📂 浏览字幕文件…").clicked() {
            // 复用内置本地文件选择器（文件过滤器只显示字幕扩展；见 FilePicker PlayMedia 逻辑）。
            actions.push(PlayerAction::PickSubtitleFile);
        }
    });
    if !player.subtitle.status.is_empty() {
        widgets::hint(ui, t, &player.subtitle.status);
    }

    // ── AI 翻译入口 ──
    ui.add_space(8.0);
    if widgets::primary_button(ui, t, "🤖 AI 翻译字幕（双语）").clicked() {
        actions.push(PlayerAction::SwitchPanel(super::PlayerPanel::Ai));
        actions.push(PlayerAction::Toast(
            crate::app::ToastKind::Info,
            "已切到 AI 面板：翻译管线（T20）接入后此处一键生成双语字幕".into(),
        ));
    }
    widgets::hint(
        ui,
        t,
        "AI 双语字幕 → wp-subtitle 生成 ASS → sub-add + sub-reload 注入",
    );

    for cmd in cmds {
        actions.push(PlayerAction::Command(cmd));
    }
}

/// 预设 → 本地预览参数（后续与 libass 样式覆盖同源）。
fn apply_style_preset(sub: &mut SubtitleUi, preset: &str) {
    match preset {
        "大字号高对比" => {
            sub.font_size = 1.45;
            sub.outline = 3.0;
            sub.color = Color32::WHITE;
        }
        "描边加粗" => {
            sub.font_size = 1.0;
            sub.outline = 4.0;
        }
        "底部贴边" => {
            sub.margin_ratio = 0.02;
        }
        "顶部显示" => {
            sub.margin_ratio = 0.36;
        }
        _ => {
            sub.font_size = 1.0;
            sub.margin_ratio = 0.085;
            sub.outline = 2.0;
            sub.color = Color32::WHITE;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: i64, selected: bool) -> TrackInfo {
        TrackInfo {
            id,
            kind: Some(TrackKind::Subtitle),
            title: format!("轨道{id}"),
            lang: "zh".into(),
            codec: "ass".into(),
            selected,
            ..Default::default()
        }
    }

    #[test]
    fn on_tracks_prefers_selected_then_first() {
        let mut ui = SubtitleUi::default();
        ui.on_tracks(&[track(2, false), track(5, true)]);
        assert_eq!(ui.selected, Some(5));
        assert!(ui.enabled);

        ui.on_tracks(&[track(2, false), track(5, false)]);
        assert_eq!(ui.selected, Some(2));
    }

    #[test]
    fn on_tracks_without_subtitle_disables() {
        let mut ui = SubtitleUi::default();
        ui.on_tracks(&[TrackInfo {
            id: 1,
            kind: Some(TrackKind::Video),
            ..Default::default()
        }]);
        assert_eq!(ui.selected, None);
        assert!(!ui.enabled);
    }

    #[test]
    fn on_tracks_picks_second_subtitle_as_secondary() {
        let mut ui = SubtitleUi::default();
        ui.on_tracks(&[track(2, true), track(5, false)]);
        assert_eq!(ui.selected, Some(2));
        assert_eq!(ui.secondary_track, Some(5), "多字幕轨时第二条作副字幕");
        assert!(ui.fonts_dir.is_empty());

        ui.on_tracks(&[track(2, true)]);
        assert_eq!(ui.secondary_track, None, "单字幕轨时无副字幕");
    }

    #[test]
    fn toggle_command_uses_minus_one_to_hide() {
        let mut ui = SubtitleUi {
            enabled: true,
            selected: Some(3),
            ..Default::default()
        };
        match ui.toggle_command() {
            PlayerCommand::SelectTrack { kind, id } => {
                assert_eq!(kind, TrackKind::Subtitle);
                assert_eq!(id, -1);
            }
            other => panic!("unexpected {other:?}"),
        }
        match ui.toggle_command() {
            PlayerCommand::SelectTrack { id, .. } => assert_eq!(id, 3),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn style_presets_update_preview() {
        let mut sub = SubtitleUi::default();
        apply_style_preset(&mut sub, "描边加粗");
        assert_eq!(sub.outline, 4.0);
        apply_style_preset(&mut sub, "顶部显示");
        assert!(sub.margin_ratio > 0.3);
        apply_style_preset(&mut sub, "默认（跟随片源）");
        assert_eq!(sub.font_size, 1.0);
        assert!((sub.margin_ratio - 0.085).abs() < 1e-6);
    }
}
