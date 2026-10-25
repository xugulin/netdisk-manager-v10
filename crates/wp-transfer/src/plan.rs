#![allow(dead_code)] // 计划层先于 GUI/CLI 接线合入；调用方接完后可移除本行。
//! 统一传输计划层：把「目录扫描结果 → 逐文件任务」的展开规则下沉到引擎底座。
//!
//! 背景：GUI 原先在 `app.rs` 里对目录下载/跨盘/上传先做预扫描，再手工拼
//! `enqueue_*` 参数；CLI 与其它入口没有同样语义，导致「目录任务」的跳过、
//! 目录补建、路径拼接行为各写一份、容易漂移。本模块把这些规则收敛成纯函数
//! （仅 [`plan_cross_copy`] 会做一次目标目录探测），GUI / CLI 共用同一套计划。
//!
//! # 分层约定
//! * **本层只做规划**：不创建任务、不建目录、不传输、不对源做额外 IO
//!   （唯一例外是跨盘时 `list` 目标目录做幂等跳过判定，且错误按空目录容忍）。
//! * [`TransferPlan::dirs_to_ensure`] 只是「需要确保存在」的目录清单：
//!   下载给本地绝对路径（调用方转 `create_dir_all`），上传/跨盘给远端逻辑路径
//!   （调用方在合适时机 `ensure_dir`）。
//! * 空源目录也会返回计划：`files` 为空、`dirs_to_ensure` 至少包含根目录；
//!   是否建任务、是否引导切换实例由上层决定（见 [`TransferPlan::is_empty`]）。
//!
//! # 路径约定
//! * 远端一律使用统一逻辑路径（`/` 开头、无尾斜杠、`/` 分隔），经
//!   [`wp_core::util::norm_path`] 规范化；中文 / 空格 / 混合大小写原样保留。
//! * 本地一律使用 [`std::path::PathBuf`]（按宿主机规则拼接）。
//! * 相对路径来自 [`crate::scan_remote_dir`] / [`crate::scan_local_dir`]：一律
//!   `a/b/c` 形式，不含前导/尾随 `/`。
//!
//! # 跳过（幂等）语义
//! * 下载：`local_exists(path, size)` 返回 `true` ⇒ 跳过，原因见
//!   [`SKIP_REASON_LOCAL`]；是否校验 `is_file` 由调用方的闭包决定。
//! * 跨盘：目标目录里已有**同名且同大小**的文件 ⇒ 跳过，原因见
//!   [`SKIP_REASON_REMOTE`]。目标目录不存在/不可列时按空目录处理（不跳过），
//!   真正的失败交给后续 `ensure_dir` / 传输任务报错。
//! * 上传：本层不探测远端（避免逐文件 `stat`），由引擎在传输时做幂等处理。
//!
//! [`wp_core::util::norm_path`]: wp_core::util::norm_path

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use wp_core::model::RemoteEntry;
use wp_core::util::{base_name, format_size, join_path, norm_path};
use wp_drive::Drive;

use crate::{DirScan, LocalDirScan};

/// 下载计划中本地已存在同大小文件时的跳过原因。
pub const SKIP_REASON_LOCAL: &str = "本地已存在同大小文件";
/// 跨盘计划中目标盘已存在同名同大小文件时的跳过原因。
pub const SKIP_REASON_REMOTE: &str = "目标盘已存在同大小文件";

/// 传输类型（决定 [`FileSpec`] 各字段的含义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanKind {
    /// 网盘 → 本地。
    Download,
    /// 本地 → 网盘。
    Upload,
    /// 源网盘 → 目标网盘（引擎经本地中转，本层只做规划）。
    CrossCopy,
}

/// 计划里的一个文件（对应未来的一条传输任务）。
///
/// `kind` 与字段的对应关系：
///
/// | 字段 | Download | Upload | CrossCopy |
/// |------|----------|--------|-----------|
/// | `source` | 源远端绝对路径 | 本地绝对路径 | 源远端绝对路径 |
/// | `local` | `Some(本地落点)` | `None` | `None` |
/// | `dst_dir` | 空串 | 目标远端目录 | 目标远端目录 |
#[derive(Debug, Clone)]
pub struct FileSpec {
    /// 下载/跨盘 = 远端源文件绝对路径；上传 = 本地源文件绝对路径。
    pub source: String,
    /// 下载落点（本地绝对路径）；上传/跨盘为 `None`。
    pub local: Option<PathBuf>,
    /// 上传/跨盘 = 目标远端目录（`/` 开头、无尾斜杠）；下载 = 空串。
    pub dst_dir: String,
    /// 文件字节数（来自预扫描，非本层 `stat`）。
    pub size: u64,
    /// 是否跳过（已存在且大小一致）。
    pub skip: bool,
    /// 跳过原因（未跳过时为空串）；下载见 [`SKIP_REASON_LOCAL`]，
    /// 跨盘见 [`SKIP_REASON_REMOTE`]。
    pub skip_reason: String,
}

/// 一次目录级传输的完整计划。
#[derive(Debug, Clone)]
pub struct TransferPlan {
    /// 计划类型。
    pub kind: PlanKind,
    /// 被传输的目录名（源目录或目标根目录的最后一段）。
    pub root_name: String,
    /// 逐文件计划（保持扫描顺序：相对路径字典序）。
    pub files: Vec<FileSpec>,
    /// 需要确保存在的目录（下载 = 本地绝对路径；上传/跨盘 = 远端逻辑路径）。
    /// 包含根目录与所有子目录（含空目录），去重排序。
    pub dirs_to_ensure: Vec<String>,
    /// 计划内所有文件的总字节数（**包含**被跳过的文件）。
    pub total_bytes: u64,
    /// 被跳过文件的字节数；实际待传字节 = `total_bytes - skipped_bytes`。
    pub skipped_bytes: u64,
}

impl TransferPlan {
    /// 源目录是否为空（没有任何文件；空目录时 `dirs_to_ensure` 仍非空）。
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// 给 Toast / 状态栏用的一句话摘要，如
    /// `10 个文件 / 5 个目录 / 1.47 GB，跳过 3 个`。
    pub fn plan_summary(&self) -> String {
        let skipped = self.files.iter().filter(|f| f.skip).count();
        let mut out = format!(
            "{} 个文件 / {} 个目录 / {}",
            self.files.len(),
            self.dirs_to_ensure.len(),
            format_size(self.total_bytes)
        );
        if skipped > 0 {
            out.push_str(&format!("，跳过 {skipped} 个"));
        }
        out
    }
}

/// 规划「远端目录 → 本地」。
///
/// * `remote_root`：源远端目录（如 `/影视/电影 A`，尾随 `/` 会被规范化去掉）；
/// * `local_root`：本地落点根目录（**已包含**要下载的目录名，如
///   `下载目录/电影 A`），文件落点为 `local_root.join(相对路径)`；
/// * `local_exists`：`(本地路径, 期望大小) -> 是否已存在且大小一致`。
///
/// 返回的 `dirs_to_ensure` 为本地绝对路径集合（根 + 全部子目录，含空目录）。
pub fn plan_download(
    scan: &DirScan,
    remote_root: &str,
    local_root: &Path,
    local_exists: impl Fn(&Path, u64) -> bool,
) -> TransferPlan {
    let root = norm_path(remote_root);
    let mut files = Vec::with_capacity(scan.files.len());
    let mut total_bytes = 0u64;
    let mut skipped_bytes = 0u64;
    for (rel, size) in &scan.files {
        let local = local_root.join(rel_to_path(rel));
        let skip = local_exists(&local, *size);
        total_bytes += *size;
        if skip {
            skipped_bytes += *size;
        }
        files.push(FileSpec {
            source: join_path(&root, rel),
            local: Some(local),
            dst_dir: String::new(),
            size: *size,
            skip,
            skip_reason: if skip {
                SKIP_REASON_LOCAL.to_string()
            } else {
                String::new()
            },
        });
    }
    TransferPlan {
        kind: PlanKind::Download,
        root_name: base_name(&root),
        files,
        dirs_to_ensure: collect_local_dirs(scan, local_root),
        total_bytes,
        skipped_bytes,
    }
}

/// 规划「本地目录 → 远端」。
///
/// `remote_root` 是**目标根目录**（已包含上传文件夹名，如
/// `/影视/我的文件夹`；尾随 `/` 会被规范化去掉）。由于 [`LocalDirScan`]
/// 不含本地根路径，本层无法自行推导文件夹名，需由调用方拼好传入。
///
/// * `source` = scan 里的本地绝对路径；
/// * `dst_dir` = `remote_root` 或 `remote_root/相对父目录`；
/// * `dirs_to_ensure` = 目标根 + 全部子目录（含空目录），去重排序。
/// * 本层不探测远端，`skip` 恒为 `false`（上传幂等由引擎负责）。
pub fn plan_upload(scan: &LocalDirScan, remote_root: &str) -> TransferPlan {
    let root = norm_path(remote_root);
    let mut files = Vec::with_capacity(scan.files.len());
    let mut total_bytes = 0u64;
    for (rel, local, size) in &scan.files {
        total_bytes += *size;
        files.push(FileSpec {
            source: local.to_string_lossy().into_owned(),
            local: None,
            dst_dir: remote_dir_for(&root, rel),
            size: *size,
            skip: false,
            skip_reason: String::new(),
        });
    }
    TransferPlan {
        kind: PlanKind::Upload,
        root_name: base_name(&root),
        files,
        dirs_to_ensure: collect_remote_dirs(
            &root,
            &scan.dirs,
            scan.files.iter().map(|(rel, _, _)| rel.as_str()),
        ),
        total_bytes,
        skipped_bytes: 0,
    }
}

/// 规划「源网盘目录 → 目标网盘目录」（引擎经本地中转，本层不传输）。
///
/// * `remote_root`：源远端目录（其最后一段作为目标根目录名）；
/// * `dst_root`：目标盘上的**父目录**（如 `/备份`）；目标根为
///   `dst_root/root_name`，与 GUI 目录浏览位置语义一致。
///
/// 对每个文件的相对父目录 `list` 一次目标目录（同一目录只列一次，错误容忍为空），
/// 同名且同大小 ⇒ `skip = true`，原因见 [`SKIP_REASON_REMOTE`]。
/// `dirs_to_ensure` 为目标端目录树（根 + 全部子目录，含空目录）。
pub async fn plan_cross_copy(
    dst: &Arc<dyn Drive>,
    scan: &DirScan,
    remote_root: &str,
    dst_root: &str,
) -> wp_core::Result<TransferPlan> {
    let src_root = norm_path(remote_root);
    let root_name = base_name(&src_root);
    let target_root = join_path(dst_root, &root_name);

    // 每个目标目录只 list 一次；不存在/出错按空目录处理（首次拷贝时目录通常还没建）。
    let mut listings: HashMap<String, Vec<RemoteEntry>> = HashMap::new();
    let mut files = Vec::with_capacity(scan.files.len());
    let mut total_bytes = 0u64;
    let mut skipped_bytes = 0u64;
    for (rel, size) in &scan.files {
        let dst_dir = remote_dir_for(&target_root, rel);
        if !listings.contains_key(&dst_dir) {
            let entries = dst.list(&dst_dir).await.unwrap_or_default();
            listings.insert(dst_dir.clone(), entries);
        }
        let name = rel_name(rel);
        let skip = listings[&dst_dir]
            .iter()
            .any(|e| !e.is_dir && e.name == name && e.size == *size);
        total_bytes += *size;
        if skip {
            skipped_bytes += *size;
        }
        files.push(FileSpec {
            source: join_path(&src_root, rel),
            local: None,
            dst_dir,
            size: *size,
            skip,
            skip_reason: if skip {
                SKIP_REASON_REMOTE.to_string()
            } else {
                String::new()
            },
        });
    }

    Ok(TransferPlan {
        kind: PlanKind::CrossCopy,
        root_name,
        files,
        dirs_to_ensure: collect_remote_dirs(
            &target_root,
            &scan.dirs,
            scan.files.iter().map(|(rel, _)| rel.as_str()),
        ),
        total_bytes,
        skipped_bytes,
    })
}

// ───────────── 内部辅助 ─────────────

/// 相对路径 `a/b/c` → `PathBuf`（按 `/` 拆段，与宿主机分隔符无关）。
fn rel_to_path(rel: &str) -> PathBuf {
    rel.split('/')
        .filter(|s| !s.is_empty())
        .fold(PathBuf::new(), |acc, part| acc.join(part))
}

/// 文件相对路径的父目录（`a/b.txt` → `a`；`b.txt` → `""`）。
fn rel_dir(rel: &str) -> &str {
    rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
}

/// 文件相对路径的文件名（`a/b.txt` → `b.txt`）。
fn rel_name(rel: &str) -> &str {
    rel.rsplit_once('/').map(|(_, n)| n).unwrap_or(rel)
}

/// 上传/跨盘：文件的目标目录（相对父目录为空时直接是根）。
fn remote_dir_for(root: &str, rel: &str) -> String {
    let parent = rel_dir(rel);
    if parent.is_empty() {
        root.to_string()
    } else {
        join_path(root, parent)
    }
}

/// 下载：需要确保的本地目录（根 + 扫描子目录 + 文件父目录），去重排序。
fn collect_local_dirs(scan: &DirScan, local_root: &Path) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    set.insert(local_root.to_string_lossy().into_owned());
    for rel in scan.dirs.iter().filter(|d| !d.is_empty()) {
        set.insert(
            local_root
                .join(rel_to_path(rel))
                .to_string_lossy()
                .into_owned(),
        );
    }
    for (rel, _) in &scan.files {
        let parent = rel_dir(rel);
        if !parent.is_empty() {
            set.insert(
                local_root
                    .join(rel_to_path(parent))
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    set.into_iter().collect()
}

/// 上传/跨盘：需要确保的远端目录（根 + 扫描子目录 + 文件父目录），去重排序。
fn collect_remote_dirs<'a>(
    root: &str,
    dirs: &'a [String],
    file_rels: impl Iterator<Item = &'a str>,
) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    set.insert(root.to_string());
    for rel in dirs.iter().filter(|d| !d.is_empty()) {
        set.insert(join_path(root, rel));
    }
    for rel in file_rels {
        let parent = rel_dir(rel);
        if !parent.is_empty() {
            set.insert(join_path(root, parent));
        }
    }
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wp_core::model::UploadOptions;
    use wp_drive::{fake, DriveInit};

    fn temp_base(tag: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("wp-transfer-plan-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn fake_drive(base: &Path) -> Arc<dyn Drive> {
        fake::create(DriveInit {
            id: "demo".into(),
            name: "演示网盘".into(),
            data_dir: base.join("data"),
            threads: 0,
            verbose: false,
        })
        .expect("创建演示网盘")
    }

    /// 下载：本地已有同大小文件 → skip+原因；路径/大小/汇总/目录清单正确。
    #[test]
    fn download_skips_existing_same_size() {
        let scan = DirScan {
            dirs: vec!["子".into()],
            files: vec![("a.txt".into(), 5), ("子/b 文件.bin".into(), 100)],
        };
        let local_root = Path::new("/tmp/下载 目标");
        let existing = local_root.join("a.txt");
        let plan = plan_download(&scan, "/影视/电影 A/", local_root, |p, size| {
            p == existing && size == 5
        });

        assert_eq!(plan.kind, PlanKind::Download);
        assert_eq!(plan.root_name, "电影 A");
        assert_eq!(plan.files.len(), 2);
        assert_eq!(plan.total_bytes, 105);
        assert_eq!(plan.skipped_bytes, 5);
        assert!(!plan.is_empty());

        let first = &plan.files[0];
        assert_eq!(first.source, "/影视/电影 A/a.txt");
        assert_eq!(
            first.local.as_deref(),
            Some(Path::new("/tmp/下载 目标/a.txt"))
        );
        assert_eq!(first.dst_dir, "");
        assert!(first.skip);
        assert_eq!(first.skip_reason, SKIP_REASON_LOCAL);

        let second = &plan.files[1];
        assert_eq!(second.source, "/影视/电影 A/子/b 文件.bin");
        assert_eq!(
            second.local.as_deref(),
            Some(Path::new("/tmp/下载 目标/子/b 文件.bin"))
        );
        assert!(!second.skip);
        assert!(second.skip_reason.is_empty());

        // 本地根 + 子目录（scan.dirs 与文件父目录去重）。
        // 注意用 join 生成期望值：Windows 下分隔符是 `\`，不能硬编码 `/`。
        assert_eq!(
            plan.dirs_to_ensure,
            vec![
                local_root.to_string_lossy().to_string(),
                local_root.join("子").to_string_lossy().to_string(),
            ]
        );
        assert_eq!(
            plan.plan_summary(),
            "2 个文件 / 2 个目录 / 105 B，跳过 1 个"
        );
    }

    /// 中文/空格路径与远端尾随斜杠规范化。
    #[test]
    fn keeps_chinese_and_spaces_and_strips_trailing_slash() {
        let scan = DirScan {
            dirs: vec!["子 目录".into()],
            files: vec![("子 目录/文件 名.mp4".into(), 7)],
        };
        let local_root = PathBuf::from("/tmp/本地 根");
        let plan = plan_download(&scan, "/我的 网盘/电影 收藏///", &local_root, |_, _| false);

        assert_eq!(plan.root_name, "电影 收藏");
        let f = &plan.files[0];
        assert_eq!(f.source, "/我的 网盘/电影 收藏/子 目录/文件 名.mp4");
        assert_eq!(
            f.local.as_deref(),
            Some(Path::new("/tmp/本地 根/子 目录/文件 名.mp4"))
        );
        assert_eq!(
            plan.dirs_to_ensure,
            vec![
                local_root.to_string_lossy().to_string(),
                local_root.join("子 目录").to_string_lossy().to_string(),
            ]
        );
    }

    /// 下载空源目录：files 空但 dirs 非空（根 + 各层空目录），不在此层决定建任务。
    #[test]
    fn download_empty_dir_returns_dirs_without_files() {
        let scan = DirScan {
            dirs: vec!["子空目录".into(), "子空目录/更深".into()],
            files: Vec::new(),
        };
        let local_root = Path::new("/tmp/空 目录");
        let plan = plan_download(&scan, "/网盘/空 目录", local_root, |_, _| false);

        assert!(plan.is_empty());
        assert!(plan.files.is_empty());
        assert_eq!(plan.total_bytes, 0);
        assert_eq!(plan.skipped_bytes, 0);
        assert_eq!(
            plan.dirs_to_ensure,
            vec![
                local_root.to_string_lossy().to_string(),
                local_root.join("子空目录").to_string_lossy().to_string(),
                local_root
                    .join("子空目录")
                    .join("更深")
                    .to_string_lossy()
                    .to_string(),
            ]
        );
        assert_eq!(plan.plan_summary(), "0 个文件 / 3 个目录 / 0 B");
    }

    /// 上传：source=本地绝对路径，dst_dir=远端根/相对父目录，
    /// dirs_to_ensure=远端根 + 全部子目录（含空目录）。
    #[test]
    fn upload_builds_remote_dst_dir_structure() {
        let scan = LocalDirScan {
            dirs: vec!["子 目录".into(), "空目录".into()],
            files: vec![
                (
                    "a.txt".into(),
                    PathBuf::from("/tmp/本地/上传 文件夹/a.txt"),
                    3,
                ),
                (
                    "子 目录/b.bin".into(),
                    PathBuf::from("/tmp/本地/上传 文件夹/子 目录/b.bin"),
                    9,
                ),
            ],
        };
        let plan = plan_upload(&scan, "/目标 盘/上传 文件夹/");

        assert_eq!(plan.kind, PlanKind::Upload);
        assert_eq!(plan.root_name, "上传 文件夹");
        assert_eq!(plan.total_bytes, 12);
        assert_eq!(plan.skipped_bytes, 0);

        assert_eq!(plan.files[0].source, "/tmp/本地/上传 文件夹/a.txt");
        assert_eq!(plan.files[0].dst_dir, "/目标 盘/上传 文件夹");
        assert_eq!(plan.files[0].local, None);
        assert!(!plan.files[0].skip);
        assert_eq!(plan.files[1].source, "/tmp/本地/上传 文件夹/子 目录/b.bin");
        assert_eq!(plan.files[1].dst_dir, "/目标 盘/上传 文件夹/子 目录");
        assert_eq!(plan.files[1].local, None);

        // BTreeSet 字典序：根 → /子 目录 → /空目录。
        assert_eq!(
            plan.dirs_to_ensure,
            vec![
                "/目标 盘/上传 文件夹".to_string(),
                "/目标 盘/上传 文件夹/子 目录".to_string(),
                "/目标 盘/上传 文件夹/空目录".to_string(),
            ]
        );
        assert!(!plan.is_empty());
    }

    /// 跨盘：目标盘预置同名同大小文件 → skip；目标目录不存在时 list 错误按空处理；
    /// 目标端目录树（含空目录）全部进入 dirs_to_ensure。
    #[tokio::test]
    async fn cross_copy_skips_existing_same_size_and_tolerates_missing_dirs() {
        let base = temp_base("cross-skip");
        let dst = fake_drive(&base);
        dst.ensure_dir("/备份 盘/电影A/子 目录").await.unwrap();
        // 预置目标文件（42 字节）。
        let staged = base.join("seed.bin");
        std::fs::write(&staged, vec![7u8; 42]).unwrap();
        dst.upload(
            &staged,
            "/备份 盘/电影A/子 目录",
            &UploadOptions {
                rename_to: Some("已有.txt".into()),
                chunk_size: 0,
                concurrency: 0,
                skip_guard: true,
            },
            Box::new(|_, _| true),
        )
        .await
        .unwrap();

        let scan = DirScan {
            dirs: vec!["子 目录".into(), "空目录".into(), "其它".into()],
            files: vec![
                ("子 目录/已有.txt".into(), 42),
                ("子 目录/新.txt".into(), 99),
                ("其它/未知 文件.bin".into(), 7),
            ],
        };
        let plan = plan_cross_copy(&dst, &scan, "/源 盘/电影A/", "/备份 盘")
            .await
            .expect("跨盘计划");

        assert_eq!(plan.kind, PlanKind::CrossCopy);
        assert_eq!(plan.root_name, "电影A");
        assert_eq!(plan.files.len(), 3);
        assert_eq!(plan.total_bytes, 148);
        assert_eq!(plan.skipped_bytes, 42);

        let skipped = &plan.files[0];
        assert!(skipped.skip);
        assert_eq!(skipped.skip_reason, SKIP_REASON_REMOTE);
        assert_eq!(skipped.source, "/源 盘/电影A/子 目录/已有.txt");
        assert_eq!(skipped.dst_dir, "/备份 盘/电影A/子 目录");
        assert_eq!(skipped.local, None);

        assert!(!plan.files[1].skip);
        assert!(plan.files[1].skip_reason.is_empty());
        // 目标目录尚不存在：list 报错被容忍，不跳过、留给后续传输报错。
        assert!(!plan.files[2].skip);
        assert_eq!(plan.files[2].dst_dir, "/备份 盘/电影A/其它");

        assert_eq!(
            plan.dirs_to_ensure,
            vec![
                "/备份 盘/电影A".to_string(),
                "/备份 盘/电影A/其它".to_string(),
                "/备份 盘/电影A/子 目录".to_string(),
                "/备份 盘/电影A/空目录".to_string(),
            ]
        );
        assert_eq!(
            plan.plan_summary(),
            "3 个文件 / 4 个目录 / 148 B，跳过 1 个"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 跨盘空源目录：files 空但目标端目录树非空。
    #[tokio::test]
    async fn cross_copy_empty_source_returns_target_dirs_only() {
        let base = temp_base("cross-empty");
        let dst = fake_drive(&base);
        let scan = DirScan {
            dirs: vec!["子空目录".into(), "子空目录/更深".into()],
            files: Vec::new(),
        };
        let plan = plan_cross_copy(&dst, &scan, "/源/空 目录", "/备份")
            .await
            .expect("跨盘计划");

        assert!(plan.is_empty());
        assert!(plan.files.is_empty());
        assert_eq!(plan.total_bytes, 0);
        assert_eq!(plan.skipped_bytes, 0);
        assert_eq!(plan.kind, PlanKind::CrossCopy);
        assert_eq!(plan.root_name, "空 目录");
        assert_eq!(
            plan.dirs_to_ensure,
            vec![
                "/备份/空 目录".to_string(),
                "/备份/空 目录/子空目录".to_string(),
                "/备份/空 目录/子空目录/更深".to_string(),
            ]
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
