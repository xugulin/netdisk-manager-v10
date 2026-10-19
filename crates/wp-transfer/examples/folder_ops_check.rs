//! 目录（多重嵌套）传输验证工具：上传/下载/跨盘复制端到端自测。
//!
//! 用真实凭证数据目录（默认 `出/网盘管理-linux-x64/数据`）跑：
//! 本地多层目录树（含中文名/空目录/二进制文件）
//!   → 引擎递归上传到光鸭 → 校验远端目录树
//!   → 引擎递归下载回本地 → 与源目录树逐文件比对
//!   → 引擎跨盘复制目录 光鸭 → 夸克 → 校验夸克远端目录树
//!   → 清理远端测试目录。
//!
//! 用法：
//! ```text
//! WP_DATA_DIR=/path/to/数据 cargo run --release -p wp-transfer --example folder_ops_check
//! ```
//! 说明：百度账号当前超容（上传会被服务端拒绝），所以上传目标用光鸭/夸克；
//! 百度侧目录下载另由 `baidu_empty_dir_check` 覆盖（mkdir + 列空目录）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use wp_core::config::{DownloadConfig, TransferConfig};
use wp_core::model::{TaskState, TransferTask};
use wp_drive::{Drive, DriveInit};
use wp_transfer::{EngineOptions, TransferEngine};

fn account_dir(dd: &Path, id: &str) -> PathBuf {
    dd.join("账号").join(id)
}

fn make_tree(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root.join("sub1").join("sub2")).unwrap();
    std::fs::create_dir_all(root.join("empty_dir")).unwrap();
    std::fs::create_dir_all(root.join("中文 目录")).unwrap();
    std::fs::write(root.join("a.txt"), b"hello folder upload\n").unwrap();
    std::fs::write(root.join("sub1").join("b.txt"), vec![b'b'; 2048]).unwrap();
    std::fs::write(
        root.join("sub1").join("sub2").join("c.bin"),
        (0..8192u32).map(|i| (i % 251) as u8).collect::<Vec<u8>>(),
    )
    .unwrap();
    std::fs::write(root.join("中文 目录").join("d.txt"), "中文文件名\n").unwrap();
}

fn walk_local(root: &Path) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for item in rd.flatten() {
            let path = item.path();
            let name = item.file_name().to_string_lossy().to_string();
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                stack.push((path, child));
            } else if meta.is_file() {
                out.push((child, meta.len()));
            }
        }
    }
    out.sort();
    out
}

fn walk_local_dirs(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for item in rd.flatten() {
            let path = item.path();
            let name = item.file_name().to_string_lossy().to_string();
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if path.is_dir() {
                out.push(child.clone());
                stack.push((path, child));
            }
        }
    }
    out.sort();
    out
}

async fn walk_remote(drive: &Arc<dyn Drive>, root: &str) -> (Vec<(String, u64)>, Vec<String>) {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    let mut stack = vec![(root.to_string(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let entries = drive.list(&dir).await.expect("列远端目录");
        for e in entries {
            let child = if rel.is_empty() {
                e.name.clone()
            } else {
                format!("{rel}/{}", e.name)
            };
            if e.is_dir {
                dirs.push(child.clone());
                stack.push((e.path.clone(), child));
            } else {
                files.push((child, e.size));
            }
        }
    }
    files.sort();
    dirs.sort();
    (files, dirs)
}

async fn wait_task(engine: &TransferEngine, id: &str) -> TransferTask {
    for _ in 0..900 {
        let task = engine
            .tasks()
            .into_iter()
            .find(|t| t.id == id)
            .expect("任务应存在");
        if task.state.is_finished() {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("任务超时：{id}");
}

fn show_task(label: &str, task: &TransferTask) {
    println!(
        "    {label}：{:?} done={}/{} note={} err={}",
        task.state, task.done, task.total, task.note, task.error
    );
}

fn main() {
    let dd = std::env::var("WP_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("出/网盘管理-linux-x64/数据"));
    println!("数据目录：{}", dd.display());

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let guangya: Arc<dyn Drive> = wp_drive_guangya::create(DriveInit {
        id: "guangya".into(),
        name: "光鸭云盘".into(),
        data_dir: account_dir(&dd, "guangya"),
        threads: 0,
        verbose: false,
    })
    .expect("光鸭凭证");
    let baidu: Arc<dyn Drive> = wp_drive_baidu::create(DriveInit {
        id: "baidu".into(),
        name: "百度网盘".into(),
        data_dir: account_dir(&dd, "baidu"),
        threads: 0,
        verbose: false,
    })
    .expect("百度凭证");
    let quark: Arc<dyn Drive> = wp_drive_quark::create(DriveInit {
        id: "quark".into(),
        name: "夸克网盘".into(),
        data_dir: account_dir(&dd, "quark"),
        threads: 0,
        verbose: false,
    })
    .expect("夸克凭证");

    let cache = std::env::temp_dir().join("wp-folder-check-cache");
    let dl_root = std::env::temp_dir().join("wp-folder-check-dl");
    let _ = std::fs::create_dir_all(&cache);
    let _ = std::fs::remove_dir_all(&dl_root);
    let mut engine = TransferEngine::new(EngineOptions {
        transfer: TransferConfig::default(),
        download: DownloadConfig::default(),
        cache_dir: cache.clone(),
        tasks_file: std::env::temp_dir().join("wp-folder-check-tasks.json"),
        download_dir: dl_root.clone(),
        keep_failed_temp: false,
        guard: None,
    });
    engine.set_runtime(rt.handle().clone());
    engine.load();

    let src = std::env::temp_dir().join("wp-folder-check-src");
    make_tree(&src);
    let src_files = walk_local(&src);
    let src_dirs = walk_local_dirs(&src);
    println!(
        "本地源目录：{}（{} 文件 / {} 目录）",
        src.display(),
        src_files.len(),
        src_dirs.len()
    );

    let gy_root = "/V10文件夹验证";
    let qk_root = "/V10文件夹验证";

    rt.block_on(async {
        // 清理上次残留
        let _ = guangya.delete(&[gy_root.to_string()]).await;
        let _ = quark.delete(&[qk_root.to_string()]).await;

        // ── 百度侧：建多层目录 + 递归下载 + 跨盘复制（账号超容只影响上传，建目录/读/下载可用）──
        println!("\n[0/3] 百度：建多层目录 → 递归下载 → 跨盘复制到光鸭 …");
        let b_root = "/V10文件夹验证";
        let b_dir = format!("{b_root}/百度空目录/子目录");
        let _ = baidu.delete(&[b_root.to_string()]).await;
        match baidu.ensure_dir(&b_dir).await {
            Ok(_entry) => {
                let target = dl_root.join("百度空目录");
                let id = engine.enqueue_download(
                    baidu.clone(),
                    format!("{b_root}/百度空目录"),
                    Some(target.clone()),
                );
                let task = wait_task(&engine, &id).await;
                show_task("百度目录下载", &task);
                assert_eq!(
                    task.state,
                    TaskState::Done,
                    "百度目录下载失败：{}",
                    task.error
                );
                assert!(target.join("子目录").is_dir(), "百度目录下载未还原嵌套目录");
                let id = engine.enqueue_cross_copy(
                    baidu.clone(),
                    format!("{b_root}/百度空目录"),
                    guangya.clone(),
                    gy_root.to_string(),
                );
                let task = wait_task(&engine, &id).await;
                show_task("百度→光鸭目录跨盘", &task);
                assert_eq!(
                    task.state,
                    TaskState::Done,
                    "百度→光鸭目录跨盘失败：{}",
                    task.error
                );
                let (_, gy_dirs_after) = walk_remote(&guangya, gy_root).await;
                assert!(
                    gy_dirs_after.iter().any(|d| d.ends_with("百度空目录")),
                    "光鸭端未生成 百度空目录"
                );
                println!("    ✅ 百度：多层目录下载 + 跨盘目录复制通过");
                let _ = guangya.delete(&[format!("{gy_root}/百度空目录")]).await;
                let _ = baidu.delete(&[b_root.to_string()]).await;
            }
            Err(e) => println!("    ⚠ 跳过（百度账号超容，服务端拒绝建目录）：{e}"),
        }

        println!("\n[1/3] 递归上传目录到光鸭 …");
        let id = engine.enqueue_upload(guangya.clone(), src.clone(), gy_root.to_string());
        let task = wait_task(&engine, &id).await;
        show_task("上传结果", &task);
        assert_eq!(task.state, TaskState::Done, "上传失败：{}", task.error);
        assert_eq!(task.done, task.total);
        let (gy_files, gy_dirs) =
            walk_remote(&guangya, &format!("{gy_root}/wp-folder-check-src")).await;
        assert_eq!(gy_files, src_files, "光鸭远端文件树与源不一致");
        assert_eq!(gy_dirs, src_dirs, "光鸭远端目录树与源不一致");
        println!(
            "    ✅ 光鸭远端目录树一致（{} 文件 / {} 目录，含空目录）",
            gy_files.len(),
            gy_dirs.len()
        );

        println!("[2/3] 从光鸭递归下载回本地 …");
        let dl_target = dl_root.join("层级下载");
        let id = engine.enqueue_download(
            guangya.clone(),
            format!("{gy_root}/wp-folder-check-src"),
            Some(dl_target.clone()),
        );
        let task = wait_task(&engine, &id).await;
        show_task("下载结果", &task);
        assert_eq!(task.state, TaskState::Done, "下载失败：{}", task.error);
        assert_eq!(
            walk_local(&dl_target),
            src_files,
            "下载后的文件树与源不一致"
        );
        assert_eq!(
            walk_local_dirs(&dl_target),
            src_dirs,
            "下载后的目录树与源不一致"
        );
        println!("    ✅ 下载目录树与源一致（含空目录/中文名/多级嵌套）");

        println!("[3/3] 跨盘复制目录 光鸭 → 夸克 …");
        let id = engine.enqueue_cross_copy(
            guangya.clone(),
            format!("{gy_root}/wp-folder-check-src"),
            quark.clone(),
            qk_root.to_string(),
        );
        let task = wait_task(&engine, &id).await;
        show_task("跨盘结果", &task);
        assert_eq!(task.state, TaskState::Done, "跨盘失败：{}", task.error);
        let (qk_files, qk_dirs) =
            walk_remote(&quark, &format!("{qk_root}/wp-folder-check-src")).await;
        assert_eq!(qk_files, src_files, "夸克远端文件树与源不一致");
        assert_eq!(qk_dirs, src_dirs, "夸克远端目录树与源不一致");
        println!(
            "    ✅ 夸克远端目录树一致（{} 文件 / {} 目录）",
            qk_files.len(),
            qk_dirs.len()
        );

        // 清理远端测试目录（失败不致命）
        let _ = quark
            .delete(&[format!("{qk_root}/wp-folder-check-src")])
            .await;
        let _ = guangya
            .delete(&[format!("{gy_root}/wp-folder-check-src")])
            .await;
    });

    println!("\n✅ 目录上传 / 下载 / 跨盘复制（多重嵌套 + 空目录 + 中文名）全部通过");
}
