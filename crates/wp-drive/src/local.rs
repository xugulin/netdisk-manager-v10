//! 本机文件系统驱动：把用户主目录当作一个「网盘」来浏览。
//!
//! 用途：
//! * 没有网盘账号时也能演示/验证文件浏览、传输、拖拽等 GUI 行为；
//! * 「跨盘传输」的本地一侧；
//! * 上传/下载在本地驱动里就是文件复制（仍走同一套进度/取消语义）。
//!
//! 逻辑路径 `/` ↔ 根目录（默认用户主目录，可用环境变量 `WP_LOCAL_ROOT` 覆盖）。
//! 所有路径都做越界校验，`..` 不能跳出根目录。

use crate::{Drive, DriveCaps, DriveInit};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wp_core::model::{
    AccountInfo, AuthCaps, DirectLink, DownloadOptions, DriveKind, OpReport, ProgressFn,
    RemoteEntry, UploadOptions,
};
use wp_core::util::{join_path, norm_path, parent_path};
use wp_core::{Error, Result};

pub struct LocalDrive {
    id: String,
    name: String,
    root: PathBuf,
}

/// 创建本机驱动（根目录：环境变量 WP_LOCAL_ROOT → 用户主目录）。
pub fn create(init: DriveInit) -> Result<Arc<dyn Drive>> {
    let root = std::env::var("WP_LOCAL_ROOT")
        .ok()
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"));
    create_with_root(init, root)
}

/// 指定根目录创建本机驱动（测试/多根目录场景用，不读环境变量）。
pub fn create_with_root(init: DriveInit, root: PathBuf) -> Result<Arc<dyn Drive>> {
    let root = root.canonicalize().unwrap_or(root);
    Ok(Arc::new(LocalDrive {
        id: init.id,
        name: if init.name.is_empty() {
            "本机".into()
        } else {
            init.name
        },
        root,
    }))
}

impl LocalDrive {
    fn to_local(&self, path: &str) -> Result<PathBuf> {
        let p = norm_path(path);
        let rel = p.trim_start_matches('/');
        let joined = if rel.is_empty() {
            self.root.clone()
        } else {
            self.root.join(rel)
        };
        // 越界校验：norm_path 已消掉 `..`，这里再确认结果仍在 root 之下（根目录本身允许）
        if joined != self.root && !joined.starts_with(&self.root) {
            return Err(Error::other("路径越界：不允许访问本机根目录之外"));
        }
        Ok(joined)
    }

    fn entry_from(&self, dir: &str, name: &str, meta: &std::fs::Metadata) -> RemoteEntry {
        let path = join_path(dir, name);
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        RemoteEntry {
            id: path.clone(),
            name: name.to_string(),
            path,
            is_dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified,
            created: None,
            raw: None,
        }
    }

    fn stat_sync(&self, path: &str) -> Result<RemoteEntry> {
        let local = self.to_local(path)?;
        let meta = std::fs::metadata(&local).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::not_found(format!("{path} 不存在"))
            } else {
                Error::Io(e)
            }
        })?;
        let name = local
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "/".into());
        Ok(self.entry_from(&parent_path(path), &name, &meta))
    }
}

#[async_trait]
impl Drive for LocalDrive {
    fn kind(&self) -> DriveKind {
        DriveKind::Local
    }
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn caps(&self) -> DriveCaps {
        DriveCaps {
            auth: AuthCaps::default(),
            mkdir: true,
            rename: true,
            move_copy: true,
            search: true,
            share: false,
            direct_link: false,
            recycle: false,
            upload: true,
            download: true,
        }
    }

    async fn account(&self) -> Result<AccountInfo> {
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "local".into());
        Ok(AccountInfo {
            display_name: format!("本机：{user}"),
            user_name: user,
            user_id: self.root.to_string_lossy().to_string(),
            vip: String::new(),
            quota: None,
            extra: serde_json::Map::new(),
        })
    }

    async fn begin_qr(&self) -> Result<crate::QrChallenge> {
        Err(Error::unsupported("本机驱动无需登录"))
    }

    async fn logout(&self) -> Result<()> {
        Ok(())
    }

    async fn list(&self, path: &str) -> Result<Vec<RemoteEntry>> {
        let dir = norm_path(path);
        let local = self.to_local(&dir)?;
        let mut out = Vec::new();
        let rd = std::fs::read_dir(&local).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::not_found(format!("{dir} 不存在"))
            } else {
                Error::Io(e)
            }
        })?;
        for item in rd.flatten() {
            let name = item.file_name().to_string_lossy().to_string();
            // 跳过隐藏文件（GUI 里也可以加开关，这里默认不显示，避免 /.config 干扰演示）
            if name.starts_with('.') {
                continue;
            }
            let Ok(meta) = item.metadata() else { continue };
            out.push(self.entry_from(&dir, &name, &meta));
        }
        wp_core::util::sort_entries(&mut out, wp_core::util::SortBy::Name, false);
        Ok(out)
    }

    async fn stat(&self, path: &str) -> Result<RemoteEntry> {
        self.stat_sync(path)
    }

    async fn ensure_dir(&self, path: &str) -> Result<RemoteEntry> {
        let local = self.to_local(path)?;
        std::fs::create_dir_all(&local).map_err(Error::Io)?;
        self.stat_sync(path)
    }

    async fn delete(&self, paths: &[String]) -> Result<OpReport> {
        let mut report = OpReport::default();
        for p in paths {
            let r = match self.to_local(p) {
                Ok(local) => {
                    if local.is_dir() {
                        std::fs::remove_dir_all(&local).map_err(Error::Io)
                    } else {
                        std::fs::remove_file(&local).map_err(Error::Io)
                    }
                }
                Err(e) => Err(e),
            };
            match r {
                Ok(()) => report.success(),
                Err(e) => report.fail(format!("{p}：{e}")),
            }
        }
        Ok(report)
    }

    async fn rename(&self, path: &str, new_name: &str) -> Result<RemoteEntry> {
        if new_name.contains('/') || new_name.contains('\\') || new_name.trim().is_empty() {
            return Err(Error::other("文件名非法"));
        }
        let local = self.to_local(path)?;
        let parent = local
            .parent()
            .ok_or_else(|| Error::other("无法重命名根目录"))?;
        let dest = parent.join(new_name);
        std::fs::rename(&local, &dest).map_err(Error::Io)?;
        let new_path = join_path(&parent_path(path), new_name);
        self.stat_sync(&new_path)
    }

    async fn move_entries(&self, paths: &[String], dest_dir: &str) -> Result<OpReport> {
        let mut report = OpReport::default();
        let dst_dir = match self.to_local(dest_dir) {
            Ok(d) => d,
            Err(e) => {
                report.fail(format!("目标目录无效：{e}"));
                return Ok(report);
            }
        };
        for p in paths {
            let local = match self.to_local(p) {
                Ok(l) => l,
                Err(e) => {
                    report.fail(format!("{p}：{e}"));
                    continue;
                }
            };
            let Some(name) = local.file_name() else {
                report.fail(format!("{p}：无法取文件名"));
                continue;
            };
            match std::fs::rename(&local, dst_dir.join(name)) {
                Ok(()) => report.success(),
                Err(e) => report.fail(format!("{p}：{e}")),
            }
        }
        Ok(report)
    }

    async fn copy_entries(&self, paths: &[String], dest_dir: &str) -> Result<OpReport> {
        let mut report = OpReport::default();
        let dst_dir = match self.to_local(dest_dir) {
            Ok(d) => d,
            Err(e) => {
                report.fail(format!("目标目录无效：{e}"));
                return Ok(report);
            }
        };
        for p in paths {
            let local = match self.to_local(p) {
                Ok(l) => l,
                Err(e) => {
                    report.fail(format!("{p}：{e}"));
                    continue;
                }
            };
            let Some(name) = local.file_name() else {
                report.fail(format!("{p}：无法取文件名"));
                continue;
            };
            let dest = dst_dir.join(name);
            let r = if local.is_dir() {
                copy_dir_recursive(&local, &dest)
            } else {
                std::fs::copy(&local, &dest).map(|_| ())
            };
            match r {
                Ok(()) => report.success(),
                Err(e) => report.fail(format!("{p}：{e}")),
            }
        }
        Ok(report)
    }

    async fn search(&self, keyword: &str, dir: &str) -> Result<Vec<RemoteEntry>> {
        let root = self.to_local(dir)?;
        let kw = keyword.to_lowercase();
        let mut out = Vec::new();
        let mut stack = vec![(root, norm_path(dir), 0usize)];
        while let Some((path, virt, depth)) = stack.pop() {
            if depth > 8 || out.len() >= 500 {
                break;
            }
            let Ok(rd) = std::fs::read_dir(&path) else {
                continue;
            };
            for item in rd.flatten() {
                let name = item.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                let Ok(meta) = item.metadata() else { continue };
                let vpath = join_path(&virt, &name);
                if name.to_lowercase().contains(&kw) {
                    out.push(self.entry_from(&virt, &name, &meta));
                }
                if meta.is_dir() {
                    stack.push((item.path(), vpath, depth + 1));
                }
            }
        }
        wp_core::util::sort_entries(&mut out, wp_core::util::SortBy::Name, false);
        Ok(out)
    }

    async fn direct_link(&self, path: &str) -> Result<DirectLink> {
        let local = self.to_local(path)?;
        Ok(DirectLink {
            url: local.to_string_lossy().to_string(),
            headers: Vec::new(),
            expire_at: None,
            size: std::fs::metadata(&local).map(|m| m.len()).unwrap_or(0),
            file_name: local
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
        })
    }

    async fn download(
        &self,
        remote: &str,
        local: &Path,
        _opts: &DownloadOptions,
        mut progress: ProgressFn,
    ) -> Result<u64> {
        let src = self.to_local(remote)?;
        copy_with_progress(&src, local, 256 * 1024, &mut progress).await
    }

    async fn upload(
        &self,
        local: &Path,
        remote_dir: &str,
        opts: &UploadOptions,
        mut progress: ProgressFn,
    ) -> Result<RemoteEntry> {
        let dir = norm_path(remote_dir);
        let dir_local = self.to_local(&dir)?;
        std::fs::create_dir_all(&dir_local).map_err(Error::Io)?;
        let name = opts
            .rename_to
            .clone()
            .or_else(|| local.file_name().map(|s| s.to_string_lossy().to_string()))
            .ok_or_else(|| Error::other("无法取文件名"))?;
        let dest = dir_local.join(&name);
        copy_with_progress(local, &dest, 256 * 1024, &mut progress).await?;
        let vpath = join_path(&dir, &name);
        self.stat_sync(&vpath)
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for item in std::fs::read_dir(src)?.flatten() {
        let p = item.path();
        let d = dst.join(item.file_name());
        if p.is_dir() {
            copy_dir_recursive(&p, &d)?;
        } else {
            std::fs::copy(&p, &d)?;
        }
    }
    Ok(())
}

async fn copy_with_progress(
    src: &Path,
    dst: &Path,
    chunk: usize,
    progress: &mut ProgressFn,
) -> Result<u64> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let total = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
    if let Some(parent) = dst.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut r = tokio::fs::File::open(src).await.map_err(Error::Io)?;
    let mut w = tokio::fs::File::create(dst).await.map_err(Error::Io)?;
    let mut buf = vec![0u8; chunk];
    let mut done = 0u64;
    loop {
        let n = r.read(&mut buf).await.map_err(Error::Io)?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n]).await.map_err(Error::Io)?;
        done += n as u64;
        if !progress(done, total) {
            return Err(Error::Canceled("用户取消/暂停".into()));
        }
    }
    w.flush().await.map_err(Error::Io)?;
    let _ = progress(done, total.max(done));
    Ok(done)
}

/// 本机驱动的演示用单元测试。
#[cfg(test)]
mod tests {
    use super::*;

    fn init(root: &Path) -> Arc<dyn Drive> {
        create_with_root(
            DriveInit {
                id: "local".into(),
                name: "本机".into(),
                data_dir: root.join("data"),
                threads: 0,
                verbose: false,
            },
            root.to_path_buf(),
        )
        .unwrap()
    }

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "wp-local-{}-{}",
            std::process::id(),
            wp_core::util::random_hex(4)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn browse_and_ops() {
        let root = tmpdir();
        std::fs::create_dir_all(root.join("电影")).unwrap();
        std::fs::write(root.join("a.txt"), b"hello").unwrap();
        let d = init(&root);
        let list = d.list("/").await.unwrap();
        assert_eq!(list.len(), 2);
        assert!(list[0].is_dir, "目录应排在前面");
        let e = d.ensure_dir("/新目录").await.unwrap();
        assert!(e.is_dir);
        let e = d.rename("/a.txt", "b.txt").await.unwrap();
        assert_eq!(e.name, "b.txt");
        let report = d
            .move_entries(&["/b.txt".to_string()], "/新目录")
            .await
            .unwrap();
        assert!(report.is_ok());
        let st = d.stat("/新目录/b.txt").await.unwrap();
        assert_eq!(st.size, 5);
        let hits = d.search("b.txt", "/").await.unwrap();
        assert_eq!(hits.len(), 1);
        let report = d.delete(&["/新目录".to_string()]).await.unwrap();
        assert!(report.is_ok());
    }

    #[tokio::test]
    async fn escape_prevented() {
        let root = tmpdir();
        let d = LocalDrive {
            id: "local".into(),
            name: "本机".into(),
            root: root.clone(),
        };
        // norm_path 会消掉 ..，因此实际落到 root 内层，不会越界
        let p = d.to_local("/../../etc/passwd");
        assert!(p.is_err() || p.unwrap().starts_with(&root));
    }

    #[tokio::test]
    async fn upload_download_roundtrip() {
        let root = tmpdir();
        let d = init(&root);
        // 源文件放在 root 之外，避免「本机→本机同路径复制」把源文件截断
        let src =
            std::env::temp_dir().join(format!("wp-local-src-{}.bin", wp_core::util::random_hex(4)));
        std::fs::write(&src, vec![7u8; 100_000]).unwrap();
        let prog = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let p2 = prog.clone();
        let entry = d
            .upload(
                &src,
                "/上传目录",
                &UploadOptions::default(),
                Box::new(move |done, _| {
                    p2.store(done, std::sync::atomic::Ordering::Relaxed);
                    true
                }),
            )
            .await
            .unwrap();
        assert_eq!(entry.size, 100_000);
        assert_eq!(entry.path, format!("/上传目录/{}", entry.name));
        let dst = root.join("dst.bin");
        let remote_path = entry.path.clone();
        let n = d
            .download(
                &remote_path,
                &dst,
                &DownloadOptions::default(),
                Box::new(|_, _| true),
            )
            .await
            .unwrap();
        assert_eq!(n, 100_000);
        assert_eq!(std::fs::read(&dst).unwrap().len(), 100_000);
    }
}
