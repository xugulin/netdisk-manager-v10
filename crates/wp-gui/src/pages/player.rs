//! 播放器页（搭架子阶段）：GUI 专家在此实现视频区/控制条/字幕/弹幕/AI 面板。
//!
//! 当前为占位页：展示「播放器模块开发中」，并给演示盘一个可视化空态，
//! 保证 cargo check / 截图回归可跑。完整实现见 docs/PLAYER_TASKS.md（T5/T24/T26）。

use crate::app::App;
use crate::widgets;
use egui::Ui;

pub fn show(app: &mut App, ui: &mut Ui) {
    let t = app.theme;
    widgets::section_title(ui, &t, "播放器");
    ui.add_space(6.0);
    widgets::card(ui, &t, |ui| {
        widgets::section_title(ui, &t, "播放器模块（开发中）");
        ui.add_space(4.0);
        widgets::hint(
            ui,
            &t,
            "媒体内核：libmpv（pinned LGPL 构建）· 渲染：进程内 GL render API（Mode A）\n\
             字幕：libass + 位图字幕（PGS/DVD/DVB）· 弹幕：egui 覆盖层 / ASS 弹幕轨\n\
             AI：本地 + 远端 Provider（可分别启停/优先级）· 网盘流：本地磁盘预读缓存代理",
        );
        ui.add_space(6.0);
        widgets::hint(
            ui,
            &t,
            "实现进度见 docs/PLAYER_TASKS.md（M0 技术验证 → M1 播放内核 → …）",
        );
    });
}
