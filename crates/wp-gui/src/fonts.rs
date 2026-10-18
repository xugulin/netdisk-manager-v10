//! 中文字体加载：优先用户配置，其后按平台常见路径探测。
//!
//! egui 默认字体不含 CJK，必须注入系统里的中文字体，否则全屏豆腐块。
//! * Windows 10：`msyh.ttc`（微软雅黑）几乎必有；
//! * Linux：Noto Sans CJK / 文泉驿 / Dream Han；
//! * 也可在设置里手填 .ttf/.otf/.ttc 路径。
//!
//! TTC（字体集合）通过 `FontData.index` 选面；Noto Sans CJK 的 index 2 是简体中文。

use std::path::Path;
use std::sync::Arc;

/// 探测到的字体信息（目前仅日志用，保留字段便于设置页展示）。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct LoadedFont {
    pub path: String,
    pub index: u32,
}

/// 按优先级列出候选（路径, TTC 面索引）。
fn candidates(custom: Option<&str>) -> Vec<(String, u32)> {
    let mut v: Vec<(String, u32)> = Vec::new();
    if let Some(p) = custom {
        if !p.trim().is_empty() {
            v.push((p.trim().to_string(), 0));
        }
    }
    #[cfg(target_os = "windows")]
    {
        let win = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
        for (f, idx) in [
            ("msyh.ttc", 0), // 微软雅黑
            ("msyh.ttf", 0),
            ("Deng.ttf", 0),   // 等线
            ("simhei.ttf", 0), // 黑体
            ("simsun.ttc", 0), // 宋体
            ("msyhbd.ttc", 0),
        ] {
            v.push((format!("{win}\\Fonts\\{f}"), idx));
        }
        // 新版系统也可能装 Noto
        v.push((format!("{win}\\Fonts\\NotoSansCJK-Regular.ttc"), 2));
    }
    #[cfg(target_os = "macos")]
    {
        for (p, idx) in [
            ("/System/Library/Fonts/PingFang.ttc", 0),
            ("/System/Library/Fonts/Hiragino Sans GB.ttc", 0),
            ("/Library/Fonts/Arial Unicode.ttf", 0),
        ] {
            v.push((p.to_string(), idx));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for (p, idx) in [
            ("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc", 2),
            ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 2),
            ("/usr/share/fonts/noto-cjk/NotoSansCJK-Medium.ttc", 2),
            (
                "/usr/share/fonts/dream-han-cjk/sans/DreamHanSansCN-W14.ttf",
                0,
            ),
            ("/usr/share/fonts/truetype/wqy/wqy-microhei.ttc", 0),
            ("/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc", 0),
            (
                "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc",
                0,
            ),
            (
                "/usr/share/fonts/adobe-source-han-sans/SourceHanSansCN-Regular.otf",
                0,
            ),
            // 兜底：其它发行版常见位置（由调用方再补 fc-match 结果）
            ("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 0),
        ] {
            v.push((p.to_string(), idx));
        }
        // 再探测一遍用户字体目录（不递归太深）
        if let Some(home) = dirs::home_dir() {
            for d in [home.join(".local/share/fonts"), home.join(".fonts")] {
                if let Ok(rd) = std::fs::read_dir(&d) {
                    for item in rd.flatten() {
                        let name = item.file_name().to_string_lossy().to_lowercase();
                        if (name.ends_with(".ttf")
                            || name.ends_with(".otf")
                            || name.ends_with(".ttc"))
                            && (name.contains("cjk")
                                || name.contains("han")
                                || name.contains("wqy")
                                || name.contains("noto")
                                || name.contains("yahei"))
                        {
                            v.push((item.path().to_string_lossy().to_string(), 0));
                        }
                    }
                }
            }
        }
    }
    v
}

/// 安装中文字体（成功返回实际加载的文件；失败返回 None）。
pub fn install(ctx: &egui::Context, custom: Option<&str>) -> Option<LoadedFont> {
    for (path, index) in candidates(custom) {
        let p = Path::new(&path);
        if !p.is_file() {
            continue;
        }
        let Ok(bytes) = std::fs::read(p) else {
            continue;
        };
        // 小于 100KB 的字体文件基本不可能覆盖常用汉字，跳过（避免加载到占位字体）
        if bytes.len() < 100 * 1024 {
            continue;
        }
        let data = egui::FontData::from_blob(Arc::new(bytes), index);
        ctx.add_font(egui::epaint::text::FontInsert::new(
            "cjk",
            data,
            vec![
                egui::epaint::text::InsertFontFamily {
                    family: egui::FontFamily::Proportional,
                    priority: egui::epaint::text::FontPriority::Lowest,
                },
                egui::epaint::text::InsertFontFamily {
                    family: egui::FontFamily::Monospace,
                    priority: egui::epaint::text::FontPriority::Lowest,
                },
            ],
        ));
        wp_core::log_info!("界面", "已加载中文字体：{}（索引 {}）", path, index);
        return Some(LoadedFont { path, index });
    }
    wp_core::log_warn!(
        "界面",
        "未找到可用的中文字体，界面中文会显示为方块；请在设置里指定字体文件路径"
    );
    None
}

/// 用整段字节生成 egui 纹理（二维码 PNG 等）。
pub fn png_texture(ctx: &egui::Context, name: &str, png: &[u8]) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(png).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let pixels: Vec<egui::Color32> = img
        .pixels()
        .map(|p| egui::Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3]))
        .collect();
    let color_image = egui::ColorImage::new(size, pixels);
    Some(ctx.load_texture(name, color_image, egui::TextureOptions::NEAREST))
}
