//! E3 预演：**纯 HTTP 扫码登录**（不碰浏览器、不碰 JS 引擎），验证 pan 域 STOKEN 与写权限。
//!
//! 用法：
//! ```text
//! cargo run -p wp-drive-baidu --example baidu_qr_probe -- /tmp/wp-qr-probe
//! cargo run -p wp-drive-baidu --example baidu_qr_probe -- /tmp/wp-qr-probe --write-probe
//! ```
//!
//! * 默认只读：起扫码 → 打印二维码链接/PNG → 轮询 → 成功后读账号信息；
//! * `--write-probe`：登录成功后建一个随机目录再删掉（**唯一会动云盘的动作**，默认关闭）。
//!
//! 判据（对应 `docs/JS宿主替代浏览器-可行性.md` 的 E3）：
//! * 扫码成功 = 纯 HTTP 能完成登录（无浏览器参与）；
//! * `--write-probe` 成功 = 会话里带的是 pan 域 128 字符 STOKEN（有写权限）；
//! * 写探针返回 `errno:-6` = 只拿到 passport 域 64 字符 STOKEN，只能读。
//!
//! 凭证全部落在传入的数据目录（0600），探针不会打印任何 Cookie 值。

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
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(
        args.next()
            .unwrap_or_else(|| "/tmp/wp-qr-probe".to_string()),
    );
    let write_probe = args.any(|a| a == "--write-probe");
    std::fs::create_dir_all(&dir)?;

    let drive: Arc<dyn Drive> = wp_drive_baidu::create(DriveInit {
        id: "baidu_probe".into(),
        name: "百度扫码探针".into(),
        data_dir: dir.clone(),
        threads: 2,
        verbose: true,
    })?;
    println!("数据目录：{}", dir.display());
    println!("浏览器：不使用；JS 引擎：不使用（全程 Rust HTTP）");

    let ch = drive.begin_qr().await?;
    println!("二维码 URL：{}", ch.url.as_deref().unwrap_or("(无)"));
    if let Some(code) = ch.code.as_deref() {
        println!("设备码/内容：{code}");
    }
    if let Some(png) = &ch.image_png {
        let path = dir.join("qr.png");
        std::fs::write(&path, png)?;
        println!("二维码 PNG：{}（{} 字节）", path.display(), png.len());
    }
    println!("{}", ch.hint);
    println!("等待扫码，最长 {} 秒…", ch.expires_in_secs);

    let started = Instant::now();
    loop {
        if started.elapsed().as_secs() > ch.expires_in_secs {
            println!("超时未扫码（{} 秒）", ch.expires_in_secs);
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        let poll = match drive.poll_qr().await {
            Ok(p) => p,
            Err(e) => {
                println!("轮询出错：{e}");
                return Err(e.into());
            }
        };
        println!(
            "[{:>3}s] {:?} {}",
            started.elapsed().as_secs(),
            poll.state,
            poll.message
        );
        match poll.state {
            QrState::Success => break,
            QrState::Expired => {
                println!("二维码已过期，请重跑");
                return Ok(());
            }
            _ => {}
        }
    }

    match drive.account().await {
        Ok(acc) => println!(
            "登录成功：display_name={:?} user_name={:?} vip={:?}",
            acc.display_name, acc.user_name, acc.vip
        ),
        Err(e) => println!("登录成功，但读账号信息失败：{e}"),
    }

    if !write_probe {
        println!("未加 --write-probe，未做写权限探测");
        return Ok(());
    }

    let name = format!("/__wp_probe_{}", wp_core::util::now_ts());
    println!("写探针：创建目录 {name}");
    match drive.ensure_dir(&name).await {
        Ok(entry) => {
            println!("写探针：创建成功（{entry:?}）");
            match drive.delete(&[name.clone()]).await {
                Ok(rep) => println!("写探针：清理完成（{rep:?}）"),
                Err(e) => println!("写探针：清理失败（请手动删 {name}）：{e}"),
            }
            println!("结论：✅ 会话具备写权限（pan 域 STOKEN 有效）");
        }
        Err(e) => {
            println!("写探针：创建失败：{e}");
            println!("结论：❌ 无写权限 —— 若含 errno:-6，说明拿到的是只读 STOKEN");
        }
    }
    Ok(())
}
