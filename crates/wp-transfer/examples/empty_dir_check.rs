//! 空目录传输验证：下载/跨盘/上传空目录应 Done 且备注「空目录」（不是静默 0 字节）。
//!
//! 真机用法：
//! ```text
//! WP_DATA_DIR=/path/to/数据 cargo run --release -p wp-transfer --example empty_dir_check
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use wp_core::config::{DownloadConfig, TransferConfig};
use wp_core::model::{TaskState, TransferTask};
use wp_drive::{Drive, DriveInit};
use wp_transfer::{EngineOptions, TransferEngine};

fn account_dir(dd: &std::path::Path, id: &str) -> PathBuf {
    dd.join("账号").join(id)
}

async fn wait(engine: &TransferEngine, id: &str) -> TransferTask {
    for _ in 0..600 {
        if let Some(t) = engine.tasks().into_iter().find(|t| t.id == id) {
            if t.state.is_finished() {
                return t;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("任务超时 {id}");
}

fn main() {
    let dd = std::env::var("WP_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("出/网盘管理-linux-x64/数据"));
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let base = std::env::temp_dir().join("wp-empty-dir-check");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("空源目录/子空目录")).unwrap();
    let mut engine = TransferEngine::new(EngineOptions {
        transfer: TransferConfig::default(),
        download: DownloadConfig::default(),
        cache_dir: base.join("缓存"),
        tasks_file: base.join("任务.json"),
        download_dir: base.join("下载"),
        keep_failed_temp: false,
        guard: None,
    });
    engine.set_runtime(rt.handle().clone());
    engine.load();

    let baidu2: Arc<dyn Drive> = wp_drive_baidu::create(DriveInit {
        id: "baidu_2".into(),
        name: "百度网盘（2）".into(),
        data_dir: account_dir(&dd, "baidu_2"),
        threads: 0,
        verbose: false,
    })
    .expect("baidu_2 凭证");
    let guangya: Arc<dyn Drive> = wp_drive_guangya::create(DriveInit {
        id: "guangya".into(),
        name: "光鸭云盘".into(),
        data_dir: account_dir(&dd, "guangya"),
        threads: 0,
        verbose: false,
    })
    .expect("光鸭凭证");

    // 0) 预扫描（GUI 入队前的同一 API）：第二账号 /IDA95 为空 → 应 0 文件、不建任务；
    //    主账号 /IDA95 有内容 → 应扫出文件。这正是"假完成"的根因证据。
    let baidu_main: Arc<dyn Drive> = wp_drive_baidu::create(DriveInit {
        id: "baidu".into(),
        name: "百度网盘".into(),
        data_dir: account_dir(&dd, "baidu"),
        threads: 0,
        verbose: false,
    })
    .expect("百度主账号凭证");
    rt.block_on(async {
        match wp_transfer::scan_remote_dir(&baidu2, "/IDA95").await {
            Ok(scan) => {
                println!(
                    "[预扫描] 百度网盘（2）/IDA95：{} 个文件 / {} 个子目录",
                    scan.files.len(),
                    scan.dirs.len()
                );
                assert!(
                    scan.files.is_empty(),
                    "该账号 /IDA95 应为空目录（若有内容则测试前提变化）"
                );
            }
            Err(e) => println!("[预扫描] 百度网盘（2）/IDA95 扫描失败（可接受）：{e}"),
        }
        match wp_transfer::scan_remote_dir(&baidu_main, "/IDA95").await {
            Ok(scan) => {
                println!(
                    "[预扫描] 百度网盘（主）/IDA95：{} 个文件 / {} 个子目录",
                    scan.files.len(),
                    scan.dirs.len()
                );
                assert!(!scan.files.is_empty(), "主账号 /IDA95 应有内容");
            }
            Err(e) => println!("[预扫描] 百度网盘（主）/IDA95 扫描失败（可接受）：{e}"),
        }

        // 1) 百度空目录 → 本地（用户截图里出问题的那类任务）
        let t = {
            let id = engine.enqueue_download(
                baidu2.clone(),
                "/IDA95".to_string(),
                Some(base.join("下载/IDA95")),
            );
            wait(&engine, &id).await
        };
        println!(
            "[下载空目录] state={:?} total={} done={} note={} progress={:.0}% text={}",
            t.state,
            t.total,
            t.done,
            t.note,
            t.progress() * 100.0,
            t.progress_text()
        );
        assert_eq!(t.state, TaskState::Done, "err={}", t.error);
        assert_eq!(t.total, 0);
        assert_eq!(t.progress(), 1.0);
        assert!(t.note.contains("空目录"), "note={}", t.note);
        assert!(base.join("下载/IDA95").is_dir(), "本地空目录应创建");

        // 2) 百度空目录 → 光鸭（空目录跨盘）
        let dst_root = "/V10空目录验证";
        let _ = guangya.delete(&[dst_root.to_string()]).await;
        let t = {
            let id = engine.enqueue_cross_copy(
                baidu2.clone(),
                "/IDA95".to_string(),
                guangya.clone(),
                dst_root.to_string(),
            );
            wait(&engine, &id).await
        };
        println!(
            "[跨盘空目录] state={:?} total={} note={} progress={:.0}%",
            t.state,
            t.total,
            t.note,
            t.progress() * 100.0
        );
        assert_eq!(t.state, TaskState::Done, "err={}", t.error);
        assert_eq!(t.progress(), 1.0);
        assert!(t.note.contains("空目录"), "note={}", t.note);
        let gy = guangya.list(dst_root).await.expect("列光鸭目标");
        assert!(
            gy.iter().any(|e| e.name == "IDA95" && e.is_dir),
            "远端应建出空目录"
        );
        let _ = guangya.delete(&[format!("{dst_root}/IDA95")]).await;
        let _ = guangya.delete(&[dst_root.to_string()]).await;

        // 3) 本地空目录 → 光鸭（上传空目录）
        let t = {
            let id = engine.enqueue_upload(
                guangya.clone(),
                base.join("空源目录"),
                "/V10空目录上传验证".to_string(),
            );
            wait(&engine, &id).await
        };
        println!(
            "[上传空目录] state={:?} total={} note={} progress={:.0}%",
            t.state,
            t.total,
            t.note,
            t.progress() * 100.0
        );
        assert_eq!(t.state, TaskState::Done, "err={}", t.error);
        assert_eq!(t.progress(), 1.0);
        assert!(t.note.contains("空目录"), "note={}", t.note);
        let remote = guangya
            .stat("/V10空目录上传验证/空源目录/子空目录")
            .await
            .expect("远端嵌套空目录应存在");
        assert!(remote.is_dir);
        let _ = guangya.delete(&["/V10空目录上传验证".to_string()]).await;
    });
    println!("✅ 空目录 下载/跨盘/上传 全部 Done=100% 且备注明确");
}
