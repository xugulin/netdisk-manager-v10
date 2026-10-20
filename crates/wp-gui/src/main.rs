//! 网盘管理 · Rust 重写版 —— GUI 入口。
//!
//! 特性：
//! * 纯 Rust GUI（eframe/egui），单可执行文件，Windows 10 / Linux 同一套代码；
//! * 三家网盘（百度 / 夸克 / 光鸭）进程内适配器，无 Python 依赖、无子进程桥；
//! * tokio 异步网络 + 传输引擎，GUI 线程永不阻塞；
//! * 无账号也能用「演示网盘 / 本机」跑通全部界面。
//!
//! 命令行参数（验收/自动化用）：
//! ```text
//! --screenshot <png>   渲染若干帧后截图退出（无头环境配合 xvfb-run）
//! --drive <id>         启动时选中哪个网盘实例（如 demo / local / baidu）
//! --page  <name>       browser|transfers|logs|settings|guard|about
//! --frames <n>         截图/退出前渲染帧数（默认 20）
//! --demo               等价于 --drive demo
//! --version            打印版本
//! ```

// Windows 发布版：不弹控制台窗口（日志仍写文件）
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod browser_login;
mod dialogs;
mod fonts;
mod pages;
mod theme;
mod thumbs;
mod widgets;

use app::{App, DevOptions};
use wp_core::config::AppConfig;
use wp_core::Paths;

/// 应用图标（窗口/任务栏用；内嵌 256px PNG，无需外部资源文件）。
fn app_icon() -> std::sync::Arc<eframe::egui::IconData> {
    const PNG: &[u8] = include_bytes!("../../../assets/icons/netdisk-manager-256.png");
    match image::load_from_memory(PNG) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (w, h) = (rgba.width(), rgba.height());
            std::sync::Arc::new(eframe::egui::IconData {
                rgba: rgba.into_raw(),
                width: w,
                height: h,
            })
        }
        Err(_) => std::sync::Arc::new(eframe::egui::IconData {
            rgba: vec![0u8, 0, 0, 0],
            width: 1,
            height: 1,
        }),
    }
}

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut dev = DevOptions::default();
    let mut i = 1;
    while i < args.len() {
        let next = |i: usize| -> Option<String> { args.get(i + 1).cloned() };
        match args[i].as_str() {
            "--screenshot" => {
                if let Some(p) = next(i) {
                    dev.screenshot = Some(std::path::PathBuf::from(p));
                    i += 1;
                }
            }
            "--drive" => {
                if let Some(id) = next(i) {
                    dev.initial_drive = Some(id);
                    i += 1;
                }
            }
            "--page" => {
                if let Some(p) = next(i) {
                    dev.initial_page = Some(p);
                    i += 1;
                }
            }
            "--frames" => {
                if let Some(n) = next(i) {
                    dev.exit_after_frames = n.parse().ok();
                    i += 1;
                }
            }
            "--dialog" => {
                if let Some(d) = next(i) {
                    dev.dialog = Some(d);
                    i += 1;
                }
            }
            "--path" => {
                if let Some(p) = next(i) {
                    dev.initial_path = Some(p);
                    i += 1;
                }
            }
            "--tools-tab" => {
                if let Some(t) = next(i) {
                    dev.tools_tab = Some(t);
                    i += 1;
                }
            }
            "--wait-ms" => {
                if let Some(ms) = next(i) {
                    dev.wait_ms = ms.parse().ok();
                    i += 1;
                }
            }
            "--media" => {
                if let Some(p) = next(i) {
                    dev.media = Some(std::path::PathBuf::from(p));
                    i += 1;
                }
            }
            "--play-remote" => {
                // 用法：--play-remote <drive_id> <远端路径>（配合 WP_DATA_DIR=出/真机联调数据）
                if let (Some(drive), Some(path)) = (next(i), args.get(i + 2).cloned()) {
                    dev.play_remote = Some((drive, path));
                    i += 2;
                }
            }
            "--seek-ms" => {
                if let Some(ms) = next(i) {
                    dev.seek_ms = ms.parse().ok();
                    i += 1;
                }
            }
            "--wait-media-ms" => {
                if let Some(ms) = next(i) {
                    dev.wait_media_ms = ms.parse().ok();
                    i += 1;
                }
            }
            other if other.starts_with("--drive=") => {
                dev.initial_drive = Some(other["--drive=".len()..].to_string());
            }
            other if other.starts_with("--page=") => {
                dev.initial_page = Some(other["--page=".len()..].to_string());
            }
            "--demo" => {
                dev.initial_drive = Some("demo".into());
            }
            "--version" | "-V" => {
                println!("{} v{}", wp_core::APP_NAME, wp_core::APP_VERSION);
                return Ok(());
            }
            "--help" | "-h" => {
                println!("{} v{}", wp_core::APP_NAME, wp_core::APP_VERSION);
                println!("用法：网盘管理 [--demo] [--drive <id>] [--page <名称>] [--screenshot <png>] [--frames <n>]");
                return Ok(());
            }
            _ => {}
        }
        i += 1;
    }

    // 数据目录 + 配置
    let paths = Paths::detect();
    paths.ensure();
    let cfg_path = paths.config_file();
    let cfg = AppConfig::load(&cfg_path);
    let font_custom = cfg.ui.font_path.clone();
    let window_size = cfg.ui.window_size;

    // 崩溃也留下日志
    let log_dir = paths.log_dir().clone();
    std::panic::set_hook(Box::new(move |info| {
        let msg = format!("程序崩溃：{info}");
        let _ = std::fs::create_dir_all(&log_dir);
        let path = log_dir.join("崩溃.log");
        let line = format!(
            "[{}] {}\n",
            wp_core::util::format_stamp(wp_core::util::now_ts()),
            msg
        );
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            use std::io::Write;
            let _ = f.write_all(line.as_bytes());
        }
        eprintln!("{msg}");
    }));

    // tokio runtime（整个进程共用；GUI 线程只拿到 Handle）
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("wp-worker")
        .enable_all()
        .build()
        .expect("无法创建 tokio runtime");
    let handle = rt.handle().clone();

    let viewport = eframe::egui::ViewportBuilder::default()
        .with_title(format!("{} v{}", wp_core::APP_NAME, wp_core::APP_VERSION))
        .with_inner_size([window_size[0].max(960.0), window_size[1].max(600.0)])
        .with_min_inner_size([960.0, 600.0])
        .with_app_id("wangpan-v10")
        .with_icon(app_icon());
    let options = eframe::NativeOptions {
        viewport,
        // Mode A：进程内 GL render API 需要 glow 后端（mpv 画面贴进 egui）
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    let result = eframe::run_native(
        "网盘管理",
        options,
        Box::new(move |cc| {
            let font_custom: Option<&str> = if font_custom.trim().is_empty() {
                None
            } else {
                Some(font_custom.as_str())
            };
            fonts::install(&cc.egui_ctx, font_custom);
            // App 在 CreationContext 里创建：播放器需要 GL get_proc_address 建 Mode A 渲染桥。
            let app = App::new(paths, cfg, handle, dev, cc);
            theme::apply(&cc.egui_ctx, app.theme);
            Ok(Box::new(app))
        }),
    );
    // 退出前保存配置（App 里保存过；此处兜底 flush 日志）
    wp_core::logging::shutdown();
    drop(rt);
    let _ = cfg_path;
    result
}
