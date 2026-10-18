//! 关于页：版本 / 架构 / 数据路径 / 功能矩阵 / 待验证说明。

use crate::app::App;
use crate::theme::{self, Theme};
use crate::widgets;
use egui::{RichText, Ui};

pub fn show(app: &mut App, ui: &mut Ui) {
    let t = app.theme;
    egui::ScrollArea::vertical()
        .id_salt("about_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if let Some(logo) = &app.logo {
                    ui.add(egui::Image::new(egui::load::SizedTexture::new(
                        logo.id(),
                        egui::vec2(56.0, 56.0),
                    )));
                    ui.add_space(4.0);
                }
                ui.label(RichText::new("网盘管理 · Rust 重写版").size(22.0).strong().color(t.accent));
                ui.label(
                    RichText::new(format!("v{}", wp_core::APP_VERSION))
                        .size(13.0)
                        .color(t.text_weak),
                );
            });
            widgets::hint(
                ui,
                &t,
                "从工作区四个 Python 项目（百度网盘适配器 / 夸克网盘适配器 / 光鸭云盘适配器 / 网盘管理_V2）只读学习后，\n\
                 用 Rust 从最底层协议重写：纯 Rust GUI（eframe/egui），单可执行文件，Windows 10 与 Linux 同一套代码。",
            );
            ui.add_space(10.0);

            widgets::card(ui, &t, |ui| {
                widgets::section_title(ui, &t, "运行环境");
                ui.add_space(4.0);
                widgets::kv_row(ui, &t, "操作系统", &format!("{} / {}", std::env::consts::OS, std::env::consts::ARCH));
                widgets::kv_row(ui, &t, "渲染后端", "egui (glow/OpenGL)，跨平台无系统 GUI 依赖");
                widgets::kv_row(ui, &t, "数据目录", &app.paths.root.display().to_string());
                widgets::kv_row(ui, &t, "配置文件", &app.cfg_path.display().to_string());
                widgets::kv_row(ui, &t, "任务库", &app.paths.tasks_file().display().to_string());
                widgets::kv_row(ui, &t, "日志文件", &wp_core::logging::current_file().map(|p| p.display().to_string()).unwrap_or_else(|| "（未启用）".into()));
                widgets::kv_row(ui, &t, "中文字体", "自动探测系统字体；可在设置里指定字体文件");
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("📂 打开数据目录").clicked() {
                        let _ = wp_core::paths::open_path(&app.paths.root);
                    }
                    if ui.button("📂 打开下载目录").clicked() {
                        let _ = wp_core::paths::open_path(&app.download_dir());
                    }
                });
            });

            ui.add_space(10.0);
            widgets::card(ui, &t, |ui| {
                widgets::section_title(ui, &t, "重写范围（功能矩阵）");
                ui.add_space(4.0);
                egui::Grid::new("matrix")
                    .num_columns(3)
                    .spacing([16.0, 6.0])
                    .show(ui, |ui| {
                        for h in ["能力", "状态", "说明"] {
                            ui.label(RichText::new(h).size(12.0).color(t.text_weak));
                        }
                        ui.end_row();
                        let rows = [
                            ("三家网盘登录（扫码/Cookie/短信/令牌）", "已实现", "协议逐条移植自 Python 适配器"),
                            ("文件浏览（列目录/统计/新建/重命名/删除/移动/复制/搜索）", "已实现", "统一 Drive trait"),
                            ("上传（分片/秒传/并发）", "已实现", "按各家协议；带敏感词守卫"),
                            ("下载（直链/断点续传/分段并发）", "已实现", "wp-net 下载器"),
                            ("传输队列（并发/重试/暂停/继续/取消/持久化）", "已实现", "wp-transfer 引擎"),
                            ("跨盘传输", "已实现", "下载到缓存 → 上传（不经第三方）"),
                            ("敏感词守卫 + 命名规避", "已实现", "AC 匹配 + 拼音改名 + 百度非法字符规避"),
                            ("主题（10 套）", "已实现", "对齐 V2 主题管理器"),
                            ("多账号实例", "已实现", "每实例独立凭证目录"),
                            ("分享管理 / 链接校验 / 转存", "已实现", "百度/夸克支持转存；光鸭仅创建+列表"),
                            ("回收站（列表/恢复/彻底删除/清空）", "已实现", "按各家实际接口能力动态显示"),
                            ("云添加（离线下载/BT）+ 云任务", "已实现", "光鸭云盘；百度创建参数未抓到暂不实现"),
                            ("上传记录 / 操作记录", "已实现", "光鸭云盘"),
                            ("分类浏览（全盘视频/图片/文档…）", "已实现", "百度 categorylist"),
                            ("缩略图（列表小图 + 详情大图）", "已实现", "三家，含夸克 WebP 解码"),
                            ("播放内核 / AI 助手 / 刮削 / 弹幕", "不移植", "V2 的视频播放与 AI 是另一套大工程，非「网盘管理」核心"),
                        ];
                        for (cap, status, note) in rows {
                            ui.label(RichText::new(cap).size(12.5).color(t.text));
                            let color = if status == "已实现" {
                                t.success
                            } else if status == "不移植" {
                                t.text_weak
                            } else {
                                t.warning
                            };
                            ui.label(RichText::new(status).size(12.0).color(color));
                            ui.label(RichText::new(note).size(11.5).color(t.text_weak));
                            ui.end_row();
                        }
                    });
            });

            ui.add_space(10.0);
            widgets::card(ui, &t, |ui| {
                widgets::section_title(ui, &t, "架构");
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "crates/wp-core     数据模型 / 配置 / 路径 / 日志 / 敏感词守卫\n\
                         crates/wp-net      HTTP 客户端 / Cookie / 摘要签名 / 下载器 / 重试\n\
                         crates/wp-drive    统一 Drive trait + 本机驱动 + 演示网盘\n\
                         crates/wp-drive-baidu / -quark / -guangya   三家网盘协议实现\n\
                         crates/wp-transfer 传输引擎（队列/并发/暂停/持久化/跨盘）\n\
                         crates/wp-gui      eframe/egui 图形界面（主程序）",
                    )
                    .size(12.0)
                    .monospace()
                    .color(t.text),
                );
                ui.add_space(4.0);
                widgets::hint(
                    ui,
                    &t,
                    "与 V2 的关键差异：去掉 Python 子进程桥（Rust 无包名冲突）→ 进程内 trait 对象；\n\
                     网络/传输全异步（tokio），GUI 永不阻塞；强类型模型（serde）替代 dict 桥协议。",
                );
            });

            ui.add_space(10.0);
            widgets::card(ui, &t, |ui| {
                widgets::section_title(ui, &t, "待真机验证清单");
                ui.add_space(4.0);
                widgets::hint(
                    ui,
                    &t,
                    "本机无三家网盘的真实账号，协议实现按 Python 源码逐行移植并有单元测试，但以下需要真实账号回归：\n\
                     · 百度：扫码登录 → 列目录 → 小文件/大文件上传 → 下载 → 分享\n\
                     · 夸克：扫码/Cookie 登录 → fid 路径解析 → 秒传 → 分片上传 → 下载\n\
                     · 光鸭：扫码/短信/令牌登录 → OSS 分片上传 → 云添加 → 下载\n\
                     · 三者：风控/限流/会话过期后的自动恢复与重新登录\n\
                     验收脚本与说明见 docs/待验证清单.md。",
                );
            });
            ui.add_space(10.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("只读学习 · 独立重写 · 不使用 Alist/rclone 等第三方网盘中间层")
                        .size(11.0)
                        .color(t.text_weak),
                );
            });
            ui.add_space(30.0);
        });
    let _ = theme::radius(0);
    let _: &Theme = &t;
}
