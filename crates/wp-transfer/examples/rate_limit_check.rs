//! T7 限流根治疗真机对照验证：
//! * 光鸭源 3 个文件跨盘 → 夸克（默认 16 全局并发）：修复前 3 并发即 127 全失败，修复后应 3/3 成功；
//! * 百度主账号单文件跨盘 → 光鸭：应成功（读方向只读也可）；
//! * 光鸭单文件跨盘 → 百度主账号（只读目标）：应在下载前快速失败并给出「只读/空间超限」可读提示。
//!
//! 运行：WP_DATA_DIR=/path/to/数据 cargo run --release -p wp-transfer --example rate_limit_check

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wp_core::config::{DownloadConfig, TransferConfig};
use wp_core::model::{TaskState, TransferTask};
use wp_drive::{Drive, DriveInit};
use wp_transfer::{EngineOptions, TransferEngine};

const 测试根: &str = "/V10限流验证";

fn account_dir(dd: &std::path::Path, id: &str) -> PathBuf {
    dd.join("账号").join(id)
}

fn make_engine(base: &std::path::Path, rt: &tokio::runtime::Handle) -> TransferEngine {
    let mut engine = TransferEngine::new(EngineOptions {
        transfer: TransferConfig::default(), // 故意用默认：small_file_concurrency=16
        download: DownloadConfig::default(),
        cache_dir: base.join("缓存"),
        tasks_file: base.join("任务.json"),
        download_dir: base.join("下载"),
        keep_failed_temp: false,
        guard: None,
    });
    engine.set_runtime(rt.clone());
    engine.load();
    engine
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

async fn wait_all(engine: &TransferEngine, ids: &[String]) -> Vec<TransferTask> {
    for _ in 0..900 {
        let tasks = engine.tasks();
        if ids.iter().all(|id| {
            tasks
                .iter()
                .find(|t| &t.id == id)
                .map(|t| t.state.is_finished())
                .unwrap_or(false)
        }) {
            return ids
                .iter()
                .map(|id| tasks.iter().find(|t| &t.id == id).unwrap().clone())
                .collect();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("批量任务超时");
}

fn main() {
    let dd = std::env::var("WP_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("出/网盘管理-linux-x64/数据"));
    let base = std::env::temp_dir().join(format!("wp-rate-limit-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("本地/in")).unwrap();
    for (i, name) in ["a.txt", "b.txt", "c.txt"].iter().enumerate() {
        std::fs::write(
            base.join("本地/in").join(name),
            format!("rate-limit-test-{i}-{}", "x".repeat(4096)),
        )
        .unwrap();
    }
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let mut engine = make_engine(&base, rt.handle());
    let guangya: Arc<dyn Drive> = wp_drive_guangya::create(DriveInit {
        id: "guangya".into(),
        name: "光鸭云盘".into(),
        data_dir: account_dir(&dd, "guangya"),
        threads: 0,
        verbose: false,
    })
    .expect("光鸭凭证");
    let quark: Arc<dyn Drive> = wp_drive_quark::create(DriveInit {
        id: "quark".into(),
        name: "夸克网盘".into(),
        data_dir: account_dir(&dd, "quark"),
        threads: 0,
        verbose: false,
    })
    .expect("夸克凭证");
    let baidu: Arc<dyn Drive> = wp_drive_baidu::create(DriveInit {
        id: "baidu".into(),
        name: "百度网盘".into(),
        data_dir: account_dir(&dd, "baidu"),
        threads: 0,
        verbose: false,
    })
    .expect("百度凭证");

    let mut failures: Vec<String> = Vec::new();
    rt.block_on(async {
        let _ = guangya.delete(&[测试根.to_string()]).await;
        let _ = quark.delete(&[测试根.to_string()]).await;

        // 准备：本地上传 3 个小文件到光鸭 /V10限流验证/src
        let mut up_ids = Vec::new();
        for name in ["a.txt", "b.txt", "c.txt"] {
            up_ids.push(engine.enqueue_upload(
                guangya.clone(),
                base.join("本地/in").join(name),
                format!("{测试根}/src"),
            ));
        }
        let ups = wait_all(&engine, &up_ids).await;
        if !ups.iter().all(|t| t.state == TaskState::Done) {
            failures.push(format!(
                "光鸭准备上传失败：{:?}",
                ups.iter().map(|t| &t.state).collect::<Vec<_>>()
            ));
        } else {
            println!("✅ 准备：本地上传 3 个小文件到光鸭 {测试根}/src 完成");
        }

        // 1) 光鸭源 3 文件跨盘 → 夸克（默认配置；修复前 3/3 全失败）
        let start = Instant::now();
        let mut cc_ids = Vec::new();
        for name in ["a.txt", "b.txt", "c.txt"] {
            cc_ids.push(engine.enqueue_cross_copy(
                guangya.clone(),
                format!("{测试根}/src/{name}"),
                quark.clone(),
                测试根.to_string(),
            ));
        }
        let ccs = wait_all(&engine, &cc_ids).await;
        let ok = ccs.iter().filter(|t| t.state == TaskState::Done).count();
        println!(
            "跨盘 光鸭→夸克 3 文件（默认并发）：{ok}/3 成功，耗时 {:?}；状态 {:?}",
            start.elapsed(),
            ccs.iter().map(|t| &t.state).collect::<Vec<_>>()
        );
        if ok != 3 {
            for t in &ccs {
                println!("  失败：{} err={}", t.file_name, t.error);
            }
            failures.push(format!("限流回归：光鸭→夸克 3 文件只成功 {ok}/3"));
        }
        // 目标端文件存在性
        for name in ["a.txt", "b.txt", "c.txt"] {
            if quark
                .stat(&format!("{测试根}/{name}"))
                .await
                .map(|e| e.is_dir)
                .unwrap_or(true)
            {
                failures.push(format!("夸克目标缺文件 {name}"));
            }
        }

        // 2) 百度主账号单文件跨盘 → 光鸭（读方向只读也应可用）
        let baidu_src = "/IDA95/ida Pro9.5安装步骤.docx".to_string();
        let bid = engine.enqueue_cross_copy(
            baidu.clone(),
            baidu_src,
            guangya.clone(),
            format!("{测试根}/baidu"),
        );
        let bt = wait(&engine, &bid).await;
        println!("跨盘 百度主→光鸭 单文件：{:?} err={}", bt.state, bt.error);
        if bt.state != TaskState::Done {
            failures.push(format!("百度主→光鸭单文件跨盘失败：{}", bt.error));
        }

        // 3) 写只读目标（百度主账号空间超限/write_ok=false）：
        //    必须「下载前」失败（不白下载）。用 689MB 大文件 + 不存在目标目录：
        //    若先下载源码，20 秒内不可能完成 → 可判定是否白下载。
        let rid = engine.enqueue_cross_copy(
            baidu.clone(),
            "/IDA95/linux/ida-pro_95_x64linux.run".to_string(),
            baidu.clone(),
            format!("{测试根}只读"),
        );
        let rstart = Instant::now();
        let mut rt_task: Option<TransferTask> = None;
        for _ in 0..200 {
            if let Some(t) = engine.tasks().into_iter().find(|t| t.id == rid) {
                if t.state.is_finished() {
                    rt_task = Some(t);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let elapsed = rstart.elapsed();
        match rt_task {
            Some(t) => {
                println!(
                    "写只读目标（百度主，下载前预检）：{:?}（{:?}）err={}",
                    t.state, elapsed, t.error
                );
                let readable = t.error.contains("只读")
                    || t.error.contains("空间")
                    || t.error.contains("重新登录");
                if t.state == TaskState::Done || !readable {
                    failures.push(format!(
                        "只读目标未给出可读失败（state={:?} err={}）",
                        t.state, t.error
                    ));
                }
                if elapsed > Duration::from_secs(20) {
                    failures.push(format!("只读目标失败太慢（{:?}），疑似白下载", elapsed));
                }
            }
            None => {
                engine.cancel(&rid);
                failures
                    .push("白下载：20s 内目标只读未失败（应在下载前 ensure_dir 就失败）".into());
            }
        }

        // 清理远端
        let _ = guangya.delete(&[测试根.to_string()]).await;
        let _ = quark.delete(&[测试根.to_string()]).await;
    });

    let _ = std::fs::remove_dir_all(&base);
    if failures.is_empty() {
        println!("✅✅ T7 限流根治疗真机验证全部通过（光鸭 3 文件跨盘默认并发 3/3）");
    } else {
        println!("❌ 验证失败：");
        for f in &failures {
            println!("  - {f}");
        }
        std::process::exit(1);
    }
}
