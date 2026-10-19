//! wp-transfer 集成测试：真正跑引擎（fake 驱动 + tokio runtime）。
//!
//! 每个用例使用独立临时目录，结束由 `TestEnv::drop` 清理。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wp_core::config::{DownloadConfig, TransferConfig};
use wp_core::guard::GuardDecision;
use wp_core::model::{DriveKind, TaskKind, TaskState, TransferTask};
use wp_drive::fake;
use wp_drive::{Drive, DriveInit};
use wp_transfer::{EngineOptions, TransferEngine, UploadGuardHook};

struct TestEnv {
    root: PathBuf,
}

impl TestEnv {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "wp-transfer-{tag}-{}-{}",
            std::process::id(),
            wp_core::util::random_hex(6)
        ));
        std::fs::create_dir_all(&root).expect("创建测试目录");
        Self { root }
    }

    fn options(&self) -> EngineOptions {
        self.options_with_guard(None)
    }

    fn options_with_guard(&self, guard: Option<Arc<dyn UploadGuardHook>>) -> EngineOptions {
        EngineOptions {
            transfer: TransferConfig {
                small_file_threshold: 1024 * 1024,
                small_file_concurrency: 4,
                large_file_concurrency: 2,
                retry: 1,
                retry_backoff_ms: 20,
                adaptive: false,
                instance_concurrency: std::collections::BTreeMap::new(),
                max_speed_bps: 0,
            },
            download: DownloadConfig {
                segments: 2,
                segment_threshold: 1024 * 1024,
                resume: true,
            },
            cache_dir: self.root.join("cache"),
            tasks_file: self.root.join("tasks.json"),
            download_dir: self.root.join("downloads"),
            keep_failed_temp: false,
            guard,
        }
    }

    fn drive(&self, id: &str) -> Arc<dyn Drive> {
        fake::create(DriveInit {
            id: id.to_string(),
            name: format!("演示-{id}"),
            data_dir: self.root.join(format!("drive-{id}")),
            threads: 0,
            verbose: false,
        })
        .expect("创建 fake 驱动")
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn engine_with_runtime(env: &TestEnv) -> TransferEngine {
    let mut engine = TransferEngine::new(env.options());
    engine.set_runtime(tokio::runtime::Handle::current());
    engine
}

fn find_task(engine: &TransferEngine, id: &str) -> Option<TransferTask> {
    engine.tasks().into_iter().find(|t| t.id == id)
}

async fn wait_state(engine: &TransferEngine, id: &str, want: TaskState) -> TransferTask {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(task) = find_task(engine, id) {
            if task.state == want {
                return task;
            }
            if task.state.is_finished() && task.state != want {
                panic!(
                    "任务 {id} 以 {:?} 结束，期望 {:?}；错误：{}",
                    task.state, want, task.error
                );
            }
        }
        if Instant::now() >= deadline {
            let summary: Vec<(String, TaskState, String)> = engine
                .tasks()
                .into_iter()
                .map(|t| (t.id, t.state, t.error))
                .collect();
            panic!("等待 {id} 进入 {want:?} 超时；当前任务：{summary:?}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ───────────────────────── 用例 ─────────────────────────

#[tokio::test]
async fn enqueue_and_persist_roundtrip() {
    let env = TestEnv::new("persist");
    let drive = env.drive("demo");
    let src = env.root.join("hello.txt");
    std::fs::write(&src, b"hello world").unwrap();

    // 不设置 runtime：任务保持排队，便于确定性地测试持久化。
    let first = TransferEngine::new(env.options());
    let up = first.enqueue_upload(drive.clone(), src, "/文档".into());
    let down = first.enqueue_download(
        drive,
        "/文档/使用说明.txt".into(),
        Some(env.root.join("saved.txt")),
    );
    assert_eq!(first.tasks().len(), 2);
    let up_task = find_task(&first, &up).expect("上传任务存在");
    assert_eq!(up_task.kind, TaskKind::Upload);
    assert_eq!(up_task.total, 11);
    assert_eq!(up_task.dst_dir, "/文档");

    first.save();
    let tasks_file = env.root.join("tasks.json");
    assert!(tasks_file.exists(), "save() 应写出任务库");
    let raw = std::fs::read_to_string(&tasks_file).unwrap();
    assert!(raw.contains("\"tasks\""), "任务库应包含 tasks 字段");

    drop(first);
    let mut restored = TransferEngine::new(env.options());
    restored.load();
    let tasks = restored.tasks();
    assert_eq!(tasks.len(), 2, "roundtrip 后任务数应一致");

    let up2 = tasks.iter().find(|t| t.id == up).expect("恢复上传任务");
    assert_eq!(up2.kind, TaskKind::Upload);
    assert_eq!(up2.state, TaskState::Queued);
    assert_eq!(up2.file_name, "hello.txt");
    assert_eq!(up2.dst_drive, "demo");
    assert_eq!(up2.total, 11);

    let down2 = tasks.iter().find(|t| t.id == down).expect("恢复下载任务");
    assert_eq!(down2.kind, TaskKind::Download);
    assert_eq!(down2.state, TaskState::Queued);
    assert_eq!(down2.src_drive, "demo");
    assert!(down2.local_path.ends_with("saved.txt"));
}

#[tokio::test]
async fn upload_completes() {
    let env = TestEnv::new("upload");
    let drive = env.drive("demo");
    let src = env.root.join("上传文件.txt");
    std::fs::write(&src, vec![7u8; 4096]).unwrap();

    let engine = engine_with_runtime(&env);
    let id = engine.enqueue_upload(drive.clone(), src, "/文档".into());
    let task = wait_state(&engine, &id, TaskState::Done).await;
    assert_eq!(task.total, 4096);
    assert_eq!(task.done, 4096);
    assert!(task.error.is_empty());

    let entry = drive.stat("/文档/上传文件.txt").await.unwrap();
    assert_eq!(entry.size, 4096);
    assert_eq!(engine.stats().done, 1);

    engine.remove(&id);
    assert!(engine.tasks().is_empty());
}

#[tokio::test]
async fn download_completes_and_dedups_names() {
    let env = TestEnv::new("download");
    let drive = env.drive("demo");
    let engine = engine_with_runtime(&env);

    let id1 = engine.enqueue_download(drive.clone(), "/文档/使用说明.txt".into(), None);
    let first = wait_state(&engine, &id1, TaskState::Done).await;
    let path1 = PathBuf::from(&first.local_path);
    assert!(path1.starts_with(engine.download_dir()));
    assert!(path1.exists(), "下载文件应落盘：{}", path1.display());
    assert_eq!(std::fs::metadata(&path1).unwrap().len(), 1024);
    assert_eq!(first.total, 1024);

    let id2 = engine.enqueue_download(drive, "/文档/使用说明.txt".into(), None);
    let second = wait_state(&engine, &id2, TaskState::Done).await;
    let path2 = PathBuf::from(&second.local_path);
    assert_ne!(path1, path2, "同名下载应自动避让");
    assert!(path2.exists());
    assert!(
        path2
            .file_name()
            .map(|n| n.to_string_lossy().contains("(1)"))
            .unwrap_or(false),
        "避让名应包含 (1)：{}",
        path2.display()
    );
}

#[tokio::test]
async fn cross_copy_completes_and_cleans_temp() {
    let env = TestEnv::new("cross");
    let src = env.drive("src");
    let dst = env.drive("dst");
    let engine = engine_with_runtime(&env);

    let id = engine.enqueue_cross_copy(
        src,
        "/文档/使用说明.txt".into(),
        dst.clone(),
        "/备份".into(),
    );
    let task = wait_state(&engine, &id, TaskState::Done).await;
    assert_eq!(task.total, 1024);
    assert_eq!(task.done, 1024);
    assert_eq!(task.src_drive, "src");
    assert_eq!(task.dst_drive, "dst");

    let entry = dst.stat("/备份/使用说明.txt").await.unwrap();
    assert_eq!(entry.size, 1024);

    let temp = env.root.join("cache").join(format!("{id}.tmp"));
    assert!(!temp.exists(), "成功后应清理中转文件");
}

#[tokio::test]
async fn pause_queued_then_resume() {
    let env = TestEnv::new("pause");
    let drive = env.drive("demo");
    let engine = engine_with_runtime(&env);

    let id = engine.enqueue_download(drive, "/文档/使用说明.txt".into(), None);
    engine.pause(&id);

    // 当前线程 runtime 下，pause 先于 spawn 的任务执行；排队的任务不得开跑。
    tokio::time::sleep(Duration::from_millis(200)).await;
    let paused = find_task(&engine, &id).expect("任务仍在列表");
    assert_eq!(paused.state, TaskState::Paused);
    let local = PathBuf::from(&paused.local_path);
    assert!(!local.exists(), "排队暂停的任务不得产生下载文件");

    engine.resume(&id);
    let done = wait_state(&engine, &id, TaskState::Done).await;
    assert!(PathBuf::from(&done.local_path).exists());
}

#[tokio::test]
async fn pause_running_then_resume() {
    let env = TestEnv::new("pauserun");
    let drive = env.drive("demo");
    let engine = engine_with_runtime(&env);

    let id = engine.enqueue_download(drive, "/音乐/示例音乐.flac".into(), None);
    wait_state(&engine, &id, TaskState::Running).await;
    engine.pause(&id);
    let paused = wait_state(&engine, &id, TaskState::Paused).await;
    assert!(paused.done < paused.total, "运行中暂停应保留中间进度");
    assert!(!PathBuf::from(&paused.local_path).exists());

    engine.resume(&id);
    let done = wait_state(&engine, &id, TaskState::Done).await;
    assert_eq!(done.done, done.total);
    assert!(PathBuf::from(&done.local_path).exists());
}

#[tokio::test]
async fn cancel_running_task() {
    let env = TestEnv::new("cancel");
    let drive = env.drive("demo");
    let engine = engine_with_runtime(&env);

    // 38 MB 的演示文件：进度按 256 KiB/2ms 模拟，足够在运行中取消。
    let id = engine.enqueue_download(drive, "/音乐/示例音乐.flac".into(), None);
    wait_state(&engine, &id, TaskState::Running).await;
    engine.cancel(&id);
    let task = wait_state(&engine, &id, TaskState::Canceled).await;
    assert_eq!(task.state, TaskState::Canceled);
    assert!(task.speed <= 0.001);
}

#[tokio::test]
async fn failure_retries_then_failed() {
    let env = TestEnv::new("retry");
    let drive = env.drive("demo");
    let engine = engine_with_runtime(&env);

    let id = engine.enqueue_download(drive, "/不存在/文件.bin".into(), None);
    let task = wait_state(&engine, &id, TaskState::Failed).await;
    assert_eq!(task.state, TaskState::Failed);
    assert_eq!(task.retries, 1, "配置 retry=1 应重试一次");
    assert!(!task.error.is_empty(), "失败任务应记录原因");

    // retry() 把失败任务重新排队，再次失败后仍可重试。
    engine.retry(&id);
    assert_eq!(
        find_task(&engine, &id).map(|t| t.state),
        Some(TaskState::Queued)
    );
    let again = wait_state(&engine, &id, TaskState::Failed).await;
    assert_eq!(again.retries, 1);
}

#[tokio::test]
async fn auth_error_is_not_retried() {
    let env = TestEnv::new("auth");
    let drive = env.drive("demo");
    drive.logout().await.unwrap();
    let engine = engine_with_runtime(&env);

    let id = engine.enqueue_download(drive, "/文档/使用说明.txt".into(), None);
    let task = wait_state(&engine, &id, TaskState::Failed).await;
    assert_eq!(task.retries, 0, "认证错误不应自动重试");
    assert!(
        task.error.contains("重新登录"),
        "认证错误应提示重新登录：{}",
        task.error
    );
}

#[tokio::test]
async fn pause_all_and_resume_all() {
    let env = TestEnv::new("pauseall");
    let engine = engine_with_runtime(&env);
    let id1 = engine.enqueue_download(env.drive("a"), "/文档/使用说明.txt".into(), None);
    let id2 = engine.enqueue_download(env.drive("b"), "/文档/设计稿.png".into(), None);

    engine.pause_all();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        find_task(&engine, &id1).map(|t| t.state),
        Some(TaskState::Paused)
    );
    assert_eq!(
        find_task(&engine, &id2).map(|t| t.state),
        Some(TaskState::Paused)
    );

    engine.resume_all();
    wait_state(&engine, &id1, TaskState::Done).await;
    wait_state(&engine, &id2, TaskState::Done).await;
}

struct RenameGuard;

impl UploadGuardHook for RenameGuard {
    fn precheck(&self, _instance: &str, _kind: DriveKind, name: &str) -> (GuardDecision, String) {
        let new_name = format!("safe-{name}");
        (
            GuardDecision::Rename {
                hit: "敏感".to_string(),
                new_name: new_name.clone(),
            },
            new_name,
        )
    }
}

#[tokio::test]
async fn upload_guard_rename_applied() {
    let env = TestEnv::new("guard");
    let drive = env.drive("demo");
    let guard: Arc<dyn UploadGuardHook> = Arc::new(RenameGuard);
    let mut engine = TransferEngine::new(env.options_with_guard(Some(guard)));
    engine.set_runtime(tokio::runtime::Handle::current());

    let src = env.root.join("片子.txt");
    std::fs::write(&src, b"guard test").unwrap();
    let id = engine.enqueue_upload(drive.clone(), src, "/文档".into());
    let task = wait_state(&engine, &id, TaskState::Done).await;
    assert!(task.note.contains("改名"), "note 应记录改名：{}", task.note);
    assert_eq!(task.file_name, "safe-片子.txt");

    let entry = drive.stat("/文档/safe-片子.txt").await.unwrap();
    assert_eq!(entry.size, 10);
}

struct BlockGuard;

impl UploadGuardHook for BlockGuard {
    fn precheck(&self, _instance: &str, _kind: DriveKind, name: &str) -> (GuardDecision, String) {
        (
            GuardDecision::Block {
                hit: "违禁".to_string(),
            },
            name.to_string(),
        )
    }
}

#[tokio::test]
async fn upload_guard_block_fails() {
    let env = TestEnv::new("block");
    let drive = env.drive("demo");
    let guard: Arc<dyn UploadGuardHook> = Arc::new(BlockGuard);
    let mut engine = TransferEngine::new(env.options_with_guard(Some(guard)));
    engine.set_runtime(tokio::runtime::Handle::current());

    let src = env.root.join("坏文件.txt");
    std::fs::write(&src, b"x").unwrap();
    let id = engine.enqueue_upload(drive, src, "/文档".into());
    let task = wait_state(&engine, &id, TaskState::Failed).await;
    assert!(
        task.error.contains("拦截"),
        "错误应说明拦截：{}",
        task.error
    );
}
