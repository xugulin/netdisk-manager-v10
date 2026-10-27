//! 夸克登录起点自检：**纯 HTTP** 取二维码（不启动任何浏览器/JS 引擎）。
//!
//! 用法：cargo run -p wp-drive-quark --example quark_qr_probe -- /tmp/wp-quark-probe
//! 只验证「二维码能拿到、轮询能通」；真正扫码登录需要用户手机确认。
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wp_core::model::QrState;
use wp_drive::{Drive, DriveInit};

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("探针失败：{e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let dir = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "/tmp/wp-quark-probe".into()));
    std::fs::create_dir_all(&dir)?;
    let drive: Arc<dyn Drive> = wp_drive_quark::create(DriveInit {
        id: "quark_probe".into(),
        name: "夸克扫码探针".into(),
        data_dir: dir.clone(),
        threads: 2,
        verbose: true,
    })?;
    println!("数据目录：{}", dir.display());
    println!("浏览器：不使用；JS 引擎：不使用（全程 Rust HTTP）");

    let ch = drive.begin_qr().await?;
    println!("二维码 URL：{}", ch.url.as_deref().unwrap_or("(无)"));
    if let Some(code) = ch.code.as_deref() {
        println!("内容/设备码：{code}");
    }
    if let Some(png) = &ch.image_png {
        let p = dir.join("qr.png");
        std::fs::write(&p, png)?;
        println!("二维码 PNG：{}（{} 字节）", p.display(), png.len());
    }
    println!("{}", ch.hint);

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let poll = drive.poll_qr().await?;
        println!("[{:>2}s] {:?} {}", start.elapsed().as_secs(), poll.state, poll.message);
        if matches!(poll.state, QrState::Expired) {
            break;
        }
    }
    println!("未扫码，退出（只验证登录起点不依赖浏览器）");
    Ok(())
}
