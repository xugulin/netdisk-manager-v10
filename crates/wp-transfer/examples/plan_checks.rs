//! T5 全矩阵真机验证（计划层 + 逐文件任务，与 GUI 完全同一套语义）：
//! 1) 上传文件夹：本地多级目录（含空子目录/中文/空格名）→ 光鸭，远端树与源一致；
//! 2) 目录下载：光鸭目录 → 本地，本地树与源一致（含空目录）；
//! 3) 目录跨盘：光鸭目录 → 夸克，目标树一致；
//! 4) 跳过：重复下载 / 重复跨盘 → plan 层 skip 生效（skipped_bytes / skip 文件数）；
//! 5) 只读目标：跨盘写入只读百度主账号 → 下载前快速失败且错误可读（不白下载）。
//!
//! 用法：WP_DATA_DIR=/path/to/数据 cargo run --release -p wp-transfer --example plan_checks

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wp_core::config::{DownloadConfig, TransferConfig};
use wp_core::model::{TaskState, TransferTask};
use wp_drive::{Drive, DriveInit};
use wp_transfer::plan::{self, PlanKind};
use wp_transfer::{EngineOptions, TransferEngine};

const 根: &str = "/V10计划层验证";

fn account_dir(dd: &Path, id: &str) -> PathBuf {
    dd.join("账号").join(id)
}

fn engine(base: &Path, rt: &tokio::runtime::Handle) -> TransferEngine {
    let mut e = TransferEngine::new(EngineOptions {
        transfer: TransferConfig::default(),
        download: DownloadConfig::default(),
        cache_dir: base.join("缓存"),
        tasks_file: base.join("任务.json"),
        download_dir: base.join("下载"),
        keep_failed_temp: false,
        guard: None,
    });
    e.set_runtime(rt.clone());
    e.load();
    e
}

async fn wait(engine: &TransferEngine, id: &str) -> TransferTask {
    for _ in 0..1200 {
        if let Some(t) = engine.tasks().into_iter().find(|t| t.id == id) {
            if t.state.is_finished() {
                return t;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("任务超时 {id}");
}

async fn wait_all(engine: &TransferEngine, ids: &[String]) -> Vec<TransferTask> {
    let mut out = Vec::new();
    for id in ids {
        out.push(wait(engine, id).await);
    }
    out
}

/// 递归列远端目录树：相对路径 + 是否目录。
async fn remote_tree(drive: &Arc<dyn Drive>, root: &str) -> Vec<(String, bool, u64)> {
    let mut out = Vec::new();
    let mut queue = vec![(root.to_string(), String::new())];
    while let Some((dir, prefix)) = queue.pop() {
        let Ok(entries) = drive.list(&dir).await else {
            continue;
        };
        for e in entries {
            let rel = if prefix.is_empty() {
                e.name.clone()
            } else {
                format!("{prefix}/{}", e.name)
            };
            out.push((rel.clone(), e.is_dir, e.size));
            if e.is_dir {
                queue.push((e.path.clone(), rel));
            }
        }
    }
    out.sort();
    out
}

/// 本地目录树：相对路径 + 是否目录 + 大小。
fn local_tree(root: &Path) -> Vec<(String, bool, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let rel = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let p = entry.path();
            if p.is_dir() {
                out.push((rel.clone(), true, 0));
                stack.push((p, rel));
            } else {
                out.push((rel, false, entry.metadata().map(|m| m.len()).unwrap_or(0)));
            }
        }
    }
    out.sort();
    out
}

fn main() {
    let dd = std::env::var("WP_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("出/网盘管理-linux-x64/数据"));
    let base = std::env::temp_dir().join(format!("wp-plan-checks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    // 造本地源树：中文/空格名 + 多级嵌套 + 空子目录
    std::fs::create_dir_all(base.join("源/sub1/sub2")).unwrap();
    std::fs::create_dir_all(base.join("源/空子目录")).unwrap();
    std::fs::create_dir_all(base.join("源/中文 目录")).unwrap();
    std::fs::write(base.join("源/a.txt"), b"hello").unwrap();
    std::fs::write(base.join("源/sub1/b.txt"), vec![1u8; 2048]).unwrap();
    std::fs::write(base.join("源/sub1/sub2/c.bin"), vec![2u8; 8192]).unwrap();
    std::fs::write(base.join("源/中文 目录/d.txt"), "中文内容".as_bytes()).unwrap();

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let mut eng = engine(&base, rt.handle());
    let mk = |id: &str, name: &str, creator: fn(DriveInit) -> wp_core::Result<Arc<dyn Drive>>| {
        creator(DriveInit {
            id: id.into(),
            name: name.into(),
            data_dir: account_dir(&dd, id),
            threads: 0,
            verbose: false,
        })
        .unwrap_or_else(|e| panic!("{name} 凭证失败：{e}"))
    };
    let guangya = mk("guangya", "光鸭云盘", wp_drive_guangya::create);
    let quark = mk("quark", "夸克网盘", wp_drive_quark::create);
    let baidu = mk("baidu", "百度网盘", wp_drive_baidu::create);

    let mut fails: Vec<String> = Vec::new();
    rt.block_on(async {
        let _ = guangya.delete(&[根.to_string()]).await;
        let _ = quark.delete(&[根.to_string()]).await;
        let remote_root = format!("{根}/src");

        // 1) 上传文件夹（plan_upload）
        let scan = wp_transfer::scan_local_dir(&base.join("源")).expect("扫描本地");
        let up_plan = plan::plan_upload(&scan, &remote_root);
        assert_eq!(up_plan.kind, PlanKind::Upload);
        for d in &up_plan.dirs_to_ensure {
            guangya.ensure_dir(d).await.expect("建远端目录");
        }
        let mut ids = Vec::new();
        for f in &up_plan.files {
            ids.push(eng.enqueue_upload(
                guangya.clone(),
                PathBuf::from(&f.source),
                f.dst_dir.clone(),
            ));
        }
        let ts = wait_all(&eng, &ids).await;
        let done = ts.iter().filter(|t| t.state == TaskState::Done).count();
        println!(
            "[1] 上传文件夹：{done}/{} 文件完成（{}）",
            ids.len(),
            up_plan.plan_summary()
        );
        if done != ids.len() {
            fails.push(format!("上传文件夹失败 {done}/{}", ids.len()));
        }
        let gy_tree = remote_tree(&guangya, &remote_root).await;
        let src_tree = local_tree(&base.join("源"));
        if gy_tree != src_tree {
            fails.push(format!(
                "光鸭远端树不一致：\n远端={gy_tree:?}\n本地={src_tree:?}"
            ));
        }

        // 2) 目录下载（plan_download）
        let scan = wp_transfer::scan_remote_dir(&guangya, &remote_root)
            .await
            .expect("扫描远端");
        let local_root = base.join("下载/源");
        let dl_plan = plan::plan_download(&scan, &remote_root, &local_root, |p, s| {
            s > 0
                && std::fs::metadata(p)
                    .map(|m| m.is_file() && m.len() == s)
                    .unwrap_or(false)
        });
        for d in &dl_plan.dirs_to_ensure {
            let _ = std::fs::create_dir_all(d);
        }
        let mut ids = Vec::new();
        for f in &dl_plan.files {
            if f.skip {
                continue;
            }
            ids.push(eng.enqueue_download(
                guangya.clone(),
                f.source.clone(),
                Some(f.local.clone().unwrap_or_default()),
            ));
        }
        let ts = wait_all(&eng, &ids).await;
        let done = ts.iter().filter(|t| t.state == TaskState::Done).count();
        println!(
            "[2] 目录下载：{done}/{} 文件完成（{}）",
            ids.len(),
            dl_plan.plan_summary()
        );
        if done != ids.len() {
            fails.push(format!("目录下载失败 {done}/{}", ids.len()));
        }
        if local_tree(&local_root) != src_tree {
            fails.push("下载后的本地树与源不一致".into());
        }

        // 3) 目录跨盘（plan_cross_copy）
        let scan = wp_transfer::scan_remote_dir(&guangya, &remote_root)
            .await
            .expect("扫描远端");
        let cc_plan = plan::plan_cross_copy(&quark, &scan, &remote_root, 根)
            .await
            .expect("跨盘计划");
        for d in &cc_plan.dirs_to_ensure {
            quark.ensure_dir(d).await.expect("建夸克目录");
        }
        let mut ids = Vec::new();
        for f in &cc_plan.files {
            if f.skip {
                continue;
            }
            ids.push(eng.enqueue_cross_copy(
                guangya.clone(),
                f.source.clone(),
                quark.clone(),
                f.dst_dir.clone(),
            ));
        }
        let ts = wait_all(&eng, &ids).await;
        let done = ts.iter().filter(|t| t.state == TaskState::Done).count();
        println!(
            "[3] 目录跨盘：{done}/{} 文件完成（{}）",
            ids.len(),
            cc_plan.plan_summary()
        );
        if done != ids.len() {
            fails.push(format!("目录跨盘失败 {done}/{}", ids.len()));
        }
        if remote_tree(&quark, &format!("{根}/src")).await != src_tree {
            fails.push("夸克目标树与源不一致".into());
        }

        // 4) 跳过：重复下载/重复跨盘应全部 skip
        let dl2 = plan::plan_download(&scan, &remote_root, &local_root, |p, s| {
            s > 0
                && std::fs::metadata(p)
                    .map(|m| m.is_file() && m.len() == s)
                    .unwrap_or(false)
        });
        let skipped_dl = dl2.files.iter().filter(|f| f.skip).count();
        println!(
            "[4a] 重复下载跳过 {skipped_dl}/{} 文件（skipped_bytes={}）",
            dl2.files.len(),
            dl2.skipped_bytes
        );
        if skipped_dl != dl2.files.len() {
            fails.push("重复下载未全部跳过".into());
        }
        let cc2 = plan::plan_cross_copy(&quark, &scan, &remote_root, 根)
            .await
            .expect("跨盘计划2");
        let skipped_cc = cc2.files.iter().filter(|f| f.skip).count();
        println!("[4b] 重复跨盘跳过 {skipped_cc}/{} 文件", cc2.files.len());
        if skipped_cc != cc2.files.len() {
            fails.push("重复跨盘未全部跳过".into());
        }

        // 5) 只读目标：大文件跨盘到只读百度主账号 → 下载前快速失败（不白下载）
        let start = Instant::now();
        let id = eng.enqueue_cross_copy(
            baidu.clone(),
            "/IDA95/linux/ida-pro_95_x64linux.run".to_string(),
            baidu.clone(),
            format!("{根}只读"),
        );
        let mut t: Option<TransferTask> = None;
        for _ in 0..200 {
            if let Some(x) = eng.tasks().into_iter().find(|x| x.id == id) {
                if x.state.is_finished() {
                    t = Some(x);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let elapsed = start.elapsed();
        match t {
            Some(t) => {
                let readable = t.error.contains("只读")
                    || t.error.contains("空间")
                    || t.error.contains("重新登录");
                println!(
                    "[5] 只读目标：{:?}（{:?}）err={}",
                    t.state, elapsed, t.error
                );
                if t.state == TaskState::Done || !readable {
                    fails.push("只读目标未给出可读失败".into());
                }
                if elapsed > Duration::from_secs(20) {
                    fails.push("只读目标失败太慢（疑似白下载）".into());
                }
            }
            None => {
                eng.cancel(&id);
                fails.push("只读目标 20s 内未失败（疑似白下载）".into());
            }
        }

        // 清理远端
        let _ = guangya.delete(&[根.to_string()]).await;
        let _ = quark.delete(&[根.to_string()]).await;
    });

    let _ = std::fs::remove_dir_all(&base);
    if fails.is_empty() {
        println!("✅✅ T5 计划层全矩阵真机验证全部通过（上传/下载/跨盘/跳过/只读目标）");
    } else {
        println!("❌ 验证失败：");
        for f in &fails {
            println!("  - {f}");
        }
        std::process::exit(1);
    }
}
