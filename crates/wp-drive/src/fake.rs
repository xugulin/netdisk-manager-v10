//! 演示网盘：内存里的样例文件树，无网络、无账号。
//!
//! 用途：
//! * 无网盘账号时跑通 GUI 全流程（浏览 / 上传 / 下载 / 传输 / 分享）；
//! * 自动化测试与截图回归（确定性数据）。
//!
//! 数据在进程内，重启后恢复初始样例；上传/下载会真实读写
//! `数据/演示网盘/` 下的文件，进度回调按 256 KiB 粒度上报。

use crate::{Drive, DriveCaps, DriveInit, QrPoll, ShareOptions};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wp_core::model::{
    AccountInfo, AuthCaps, DirectLink, DownloadOptions, DriveKind, OpReport, ProgressFn, Quota,
    RemoteEntry, ShareInfo, UploadOptions,
};
use wp_core::util::{base_name, join_path, norm_path, parent_path, sort_entries, SortBy};
use wp_core::{Error, Result};

const 账号名: &str = "演示用户";

struct State {
    logged_in: bool,
    entries: BTreeMap<String, RemoteEntry>,
    shares: Vec<ShareInfo>,
}

pub struct FakeDrive {
    id: String,
    name: String,
    data_dir: PathBuf,
    state: Mutex<State>,
}

/// 创建演示网盘（自动登录）。
pub fn create(init: DriveInit) -> Result<Arc<dyn Drive>> {
    let data_dir = init.data_dir.join("演示网盘");
    let _ = std::fs::create_dir_all(&data_dir);
    let mut entries: BTreeMap<String, RemoteEntry> = BTreeMap::new();
    // 根 + 样例目录树
    for dir in ["/影视", "/影视/科幻", "/文档", "/音乐"] {
        entries.insert(dir.to_string(), RemoteEntry::dir(base_name(dir), dir));
    }
    let samples: &[(&str, u64)] = &[
        ("/影视/示例影片-4K.mp4", 4_500_000_000),
        ("/影视/示例影片-1080P.mp4", 1_200_000_000),
        ("/影视/科幻/星际漫游.mkv", 8_000_000_000),
        ("/影视/科幻/时间简史.mp4", 700_000_000),
        ("/文档/使用说明.txt", 1024),
        ("/文档/端口与协议清单.pdf", 2_500_000),
        ("/文档/设计稿.png", 680_000),
        ("/音乐/示例音乐.flac", 38_000_000),
    ];
    for (i, (path, size)) in samples.iter().enumerate() {
        let mut e = RemoteEntry::file(base_name(path), *path, *size);
        e.id = format!("fake-{i:04}");
        e.modified = Some(wp_core::util::now_ts() - (i as i64 + 1) * 86400);
        entries.insert(path.to_string(), e);
    }
    Ok(Arc::new(FakeDrive {
        id: init.id,
        name: if init.name.is_empty() {
            "演示网盘".into()
        } else {
            init.name
        },
        data_dir,
        state: Mutex::new(State {
            logged_in: true,
            entries,
            shares: Vec::new(),
        }),
    }))
}

impl FakeDrive {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn require_login(&self) -> Result<()> {
        if self.lock().logged_in {
            Ok(())
        } else {
            Err(Error::auth("演示账号未登录"))
        }
    }

    fn children(&self, dir: &str) -> Vec<RemoteEntry> {
        let base = norm_path(dir);
        let prefix = if base == "/" {
            "/".to_string()
        } else {
            format!("{base}/")
        };
        let st = self.lock();
        let mut out: Vec<RemoteEntry> = st
            .entries
            .iter()
            .filter(|(p, _)| p.starts_with(&prefix) && *p != &base)
            .filter(|(p, _)| !p[prefix.len()..].contains('/'))
            .map(|(_, e)| e.clone())
            .collect();
        sort_entries(&mut out, SortBy::Name, false);
        out
    }
}

/// 模拟分块进度（演示用）。
async fn simulate_progress(total: u64, mut progress: ProgressFn) -> Result<u64> {
    let chunk = 256 * 1024u64;
    let mut done = 0u64;
    while done < total {
        done = (done + chunk).min(total);
        if !progress(done, total) {
            return Err(Error::Canceled("演示任务已取消".into()));
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    Ok(done)
}

#[async_trait]
impl Drive for FakeDrive {
    fn kind(&self) -> DriveKind {
        DriveKind::Fake
    }
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn caps(&self) -> DriveCaps {
        DriveCaps {
            auth: AuthCaps {
                qr: true,
                cookie: true,
                sms: false,
                password: false,
                token: false,
            },
            mkdir: true,
            rename: true,
            move_copy: true,
            search: true,
            share: true,
            direct_link: true,
            recycle: false,
            upload: true,
            download: true,
        }
    }

    async fn account(&self) -> Result<AccountInfo> {
        self.require_login()?;
        Ok(AccountInfo {
            display_name: 账号名.into(),
            user_name: "demo@example.com".into(),
            user_id: "demo-0001".into(),
            vip: "演示会员".into(),
            quota: Some(Quota {
                total: 100 * 1024 * 1024 * 1024,
                used: 15 * 1024 * 1024 * 1024,
                free: 85 * 1024 * 1024 * 1024,
            }),
            extra: serde_json::Map::new(),
        })
    }

    async fn begin_qr(&self) -> Result<crate::QrChallenge> {
        Ok(crate::QrChallenge {
            image_png: None,
            url: Some("https://example.com/demo-qr".into()),
            code: Some("DEMO".into()),
            hint: "演示网盘：点「模拟扫码成功」即可登录".into(),
            expires_in_secs: 120,
        })
    }

    async fn poll_qr(&self) -> Result<QrPoll> {
        self.lock().logged_in = true;
        Ok(QrPoll::success("演示登录成功"))
    }

    async fn login_cookie(&self, _text: &str) -> Result<AccountInfo> {
        self.lock().logged_in = true;
        self.account().await
    }

    async fn send_sms(&self, phone: &str) -> Result<String> {
        Ok(format!("演示模式：已向 {phone} 发送验证码（固定 123456）"))
    }

    async fn verify_sms(&self, _phone: &str, code: &str) -> Result<AccountInfo> {
        if code.trim() == "123456" {
            self.lock().logged_in = true;
            self.account().await
        } else {
            Err(Error::auth("演示验证码错误（应为 123456）"))
        }
    }

    async fn login_token(&self, token: &str, _extra: &str) -> Result<AccountInfo> {
        if token.trim().is_empty() {
            return Err(Error::auth("令牌不能为空"));
        }
        self.lock().logged_in = true;
        self.account().await
    }

    async fn logout(&self) -> Result<()> {
        self.lock().logged_in = false;
        Ok(())
    }

    async fn list(&self, path: &str) -> Result<Vec<RemoteEntry>> {
        self.require_login()?;
        let p = norm_path(path);
        if p != "/"
            && !self
                .lock()
                .entries
                .get(&p)
                .map(|e| e.is_dir)
                .unwrap_or(false)
        {
            return Err(Error::not_found(format!("{p} 不是目录或不存在")));
        }
        Ok(self.children(&p))
    }

    async fn stat(&self, path: &str) -> Result<RemoteEntry> {
        self.require_login()?;
        let p = norm_path(path);
        if p == "/" {
            return Ok(RemoteEntry::dir("/", "/"));
        }
        self.lock()
            .entries
            .get(&p)
            .cloned()
            .ok_or_else(|| Error::not_found(format!("{p} 不存在")))
    }

    async fn ensure_dir(&self, path: &str) -> Result<RemoteEntry> {
        self.require_login()?;
        let p = norm_path(path);
        if p == "/" {
            return Ok(RemoteEntry::dir("/", "/"));
        }
        let mut cur = String::new();
        for seg in p.split('/').filter(|s| !s.is_empty()) {
            cur = join_path(&cur, seg);
            let mut st = self.lock();
            st.entries
                .entry(cur.clone())
                .or_insert_with(|| RemoteEntry::dir(seg, &cur));
        }
        self.stat(&p).await
    }

    async fn delete(&self, paths: &[String]) -> Result<OpReport> {
        self.require_login()?;
        let mut report = OpReport::default();
        for raw in paths {
            let p = norm_path(raw);
            if p == "/" {
                report.fail("不能删除根目录");
                continue;
            }
            let mut st = self.lock();
            if !st.entries.contains_key(&p) {
                report.fail(format!("{p} 不存在"));
                continue;
            }
            let prefix = format!("{p}/");
            st.entries.retain(|k, _| k != &p && !k.starts_with(&prefix));
            report.success();
        }
        Ok(report)
    }

    async fn rename(&self, path: &str, new_name: &str) -> Result<RemoteEntry> {
        self.require_login()?;
        let p = norm_path(path);
        let new_name = new_name.trim();
        if new_name.is_empty() || new_name.contains('/') {
            return Err(Error::other("文件名非法"));
        }
        let parent = parent_path(&p);
        let dest = join_path(&parent, new_name);
        let mut st = self.lock();
        if st.entries.contains_key(&dest) {
            return Err(Error::other(format!("同名条目已存在：{dest}")));
        }
        let mut entry = st
            .entries
            .remove(&p)
            .ok_or_else(|| Error::not_found(format!("{p} 不存在")))?;
        // 若目录，整棵子树一起改名
        let old_prefix = format!("{p}/");
        let new_prefix = format!("{dest}/");
        let children: Vec<(String, RemoteEntry)> = st
            .entries
            .iter()
            .filter(|(k, _)| k.starts_with(&old_prefix))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (k, v) in children {
            st.entries.remove(&k);
            let nk = k.replacen(&old_prefix, &new_prefix, 1);
            let mut nv = v;
            nv.path = nk.clone();
            st.entries.insert(nk, nv);
        }
        entry.name = new_name.to_string();
        entry.path = dest.clone();
        st.entries.insert(dest.clone(), entry.clone());
        drop(st);
        let _ = parent;
        Ok(entry)
    }

    async fn move_entries(&self, paths: &[String], dest_dir: &str) -> Result<OpReport> {
        self.require_login()?;
        let mut report = OpReport::default();
        for p in paths {
            match self.rename_into(p, dest_dir) {
                Ok(()) => report.success(),
                Err(e) => report.fail(format!("{p}：{e}")),
            }
        }
        Ok(report)
    }

    async fn copy_entries(&self, paths: &[String], dest_dir: &str) -> Result<OpReport> {
        self.require_login()?;
        let mut report = OpReport::default();
        let dest = norm_path(dest_dir);
        for p in paths {
            let src = norm_path(p);
            let Some(e) = self.lock().entries.get(&src).cloned() else {
                report.fail(format!("{src} 不存在"));
                continue;
            };
            let mut st = self.lock();
            let np = join_path(&dest, &e.name);
            if st.entries.contains_key(&np) {
                report.fail(format!("{np} 已存在"));
                continue;
            }
            let mut ne = e.clone();
            ne.path = np.clone();
            st.entries.insert(np.clone(), ne);
            report.success();
        }
        Ok(report)
    }

    async fn search(&self, keyword: &str, dir: &str) -> Result<Vec<RemoteEntry>> {
        self.require_login()?;
        let kw = keyword.to_lowercase();
        let base = norm_path(dir);
        let prefix = if base == "/" {
            "/".to_string()
        } else {
            format!("{base}/")
        };
        let mut out: Vec<RemoteEntry> = self
            .lock()
            .entries
            .iter()
            .filter(|(p, _)| p.starts_with(&prefix))
            .filter(|(_, e)| e.name.to_lowercase().contains(&kw))
            .map(|(_, e)| e.clone())
            .collect();
        sort_entries(&mut out, SortBy::Name, false);
        Ok(out)
    }

    async fn direct_link(&self, path: &str) -> Result<DirectLink> {
        self.require_login()?;
        let e = self.stat(path).await?;
        Ok(DirectLink {
            url: format!("https://demo.invalid{}", e.path),
            headers: Vec::new(),
            expire_at: Some(wp_core::util::now_ts() + 3600),
            size: e.size,
            file_name: e.name,
        })
    }

    async fn download(
        &self,
        remote: &str,
        local: &std::path::Path,
        _opts: &DownloadOptions,
        progress: ProgressFn,
    ) -> Result<u64> {
        self.require_login()?;
        let e = self.stat(remote).await?;
        if e.is_dir {
            return Err(Error::unsupported("演示网盘不支持整目录下载"));
        }
        if let Some(parent) = local.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let done = simulate_progress(e.size, progress).await?;
        // 写一个占位文件（大小与远端一致，内容为 0）
        tokio::task::spawn_blocking({
            let local = local.to_path_buf();
            let size = e.size;
            move || -> std::io::Result<()> {
                use std::io::Write;
                let mut f = std::fs::File::create(&local)?;
                let buf = vec![0u8; 64 * 1024];
                let mut written = 0u64;
                while written < size {
                    let n = (size - written).min(buf.len() as u64) as usize;
                    f.write_all(&buf[..n])?;
                    written += n as u64;
                }
                Ok(())
            }
        })
        .await
        .map_err(|e| Error::other(format!("写文件失败：{e}")))?
        .map_err(Error::Io)?;
        Ok(done)
    }

    async fn upload(
        &self,
        local: &std::path::Path,
        remote_dir: &str,
        opts: &UploadOptions,
        progress: ProgressFn,
    ) -> Result<RemoteEntry> {
        self.require_login()?;
        let meta = std::fs::metadata(local).map_err(Error::Io)?;
        let name = opts
            .rename_to
            .clone()
            .or_else(|| local.file_name().map(|s| s.to_string_lossy().to_string()))
            .ok_or_else(|| Error::other("无法取文件名"))?;
        let dir = norm_path(remote_dir);
        let path = join_path(&dir, &name);
        // 真实拷贝一份到数据目录，保证「上传」有实物
        let dst = self
            .data_dir
            .join(format!("{}.bin", wp_core::util::gen_task_id()));
        tokio::fs::copy(local, &dst).await.map_err(Error::Io)?;
        let _ = meta;
        let done = simulate_progress(
            std::fs::metadata(&dst).map(|m| m.len()).unwrap_or(0),
            progress,
        )
        .await?;
        let mut e = RemoteEntry::file(&name, &path, done);
        e.id = format!("fake-{}", wp_core::util::random_hex(8));
        e.modified = Some(wp_core::util::now_ts());
        self.lock().entries.insert(path.clone(), e.clone());
        Ok(e)
    }

    async fn create_share(&self, paths: &[String], opts: &ShareOptions) -> Result<ShareInfo> {
        self.require_login()?;
        let id = wp_core::util::random_hex(10);
        let info = ShareInfo {
            share_id: id.clone(),
            url: format!("https://demo.invalid/s/{id}"),
            password: opts
                .password
                .clone()
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| {
                    let h = wp_core::util::random_hex(4);
                    h[..4].to_string()
                }),
            expire_at: if opts.expire_days == 0 {
                None
            } else {
                Some(wp_core::util::now_ts() + opts.expire_days as i64 * 86400)
            },
            title: if opts.title.is_empty() {
                paths.first().map(|p| base_name(p)).unwrap_or_default()
            } else {
                opts.title.clone()
            },
        };
        self.lock().shares.push(info.clone());
        Ok(info)
    }

    async fn list_shares(&self) -> Result<Vec<ShareInfo>> {
        self.require_login()?;
        Ok(self.lock().shares.clone())
    }

    async fn cancel_shares(&self, ids: &[String]) -> Result<OpReport> {
        self.require_login()?;
        let mut st = self.lock();
        let before = st.shares.len();
        st.shares.retain(|s| !ids.contains(&s.share_id));
        let mut report = OpReport::default();
        report.ok = before - st.shares.len();
        report.failed = ids.len().saturating_sub(report.ok);
        Ok(report)
    }

    async fn write_probe(&self) -> Result<String> {
        Ok("演示网盘允许写入".into())
    }
}

impl FakeDrive {
    /// 移动到目标目录（保留名字）。
    fn rename_into(&self, path: &str, dest_dir: &str) -> Result<()> {
        let p = norm_path(path);
        let dest = norm_path(dest_dir);
        let name = base_name(&p);
        let np = join_path(&dest, &name);
        let mut st = self.lock();
        let mut e = st
            .entries
            .remove(&p)
            .ok_or_else(|| Error::not_found(format!("{p} 不存在")))?;
        e.path = np.clone();
        st.entries.insert(np, e);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive() -> Arc<dyn Drive> {
        let dir = std::env::temp_dir().join(format!("wp-fake-{}", wp_core::util::random_hex(4)));
        create(DriveInit {
            id: "demo".into(),
            name: "演示".into(),
            data_dir: dir,
            threads: 0,
            verbose: false,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn tree_and_ops() {
        let d = drive();
        let root = d.list("/").await.unwrap();
        assert!(root.iter().any(|e| e.name == "影视"));
        let sci = d.list("/影视/科幻").await.unwrap();
        assert_eq!(sci.len(), 2);
        let nd = d.ensure_dir("/新建/深层").await.unwrap();
        assert!(nd.is_dir);
        d.rename("/文档/使用说明.txt", "说明-2.txt").await.unwrap();
        let st = d.stat("/文档/说明-2.txt").await.unwrap();
        assert_eq!(st.size, 1024);
        let hits = d.search("影片", "/").await.unwrap();
        assert!(hits.len() >= 2);
        let rep = d.cancel_shares(&[]).await.unwrap();
        assert_eq!(rep.ok, 0);
    }

    #[tokio::test]
    async fn account_and_logout() {
        let d = drive();
        assert!(d.account().await.is_ok());
        d.logout().await.unwrap();
        assert!(d.account().await.unwrap_err().is_auth());
        d.login_cookie("x").await.unwrap();
        assert!(d.account().await.is_ok());
    }

    #[tokio::test]
    async fn upload_download() {
        let d = drive();
        let tmp =
            std::env::temp_dir().join(format!("wp-fake-src-{}.txt", wp_core::util::random_hex(4)));
        std::fs::write(&tmp, b"hello demo").unwrap();
        let e = d
            .upload(
                &tmp,
                "/文档",
                &UploadOptions::default(),
                Box::new(|_, _| true),
            )
            .await
            .unwrap();
        assert!(e.path.starts_with("/文档/wp-fake-src-"));
        assert_eq!(e.size, 10);
        let dst =
            std::env::temp_dir().join(format!("wp-fake-dst-{}.bin", wp_core::util::random_hex(4)));
        let dst2 = tmp.with_extension("dl");
        let n = d
            .download(
                &e.path,
                &dst2,
                &DownloadOptions::default(),
                Box::new(|_, _| true),
            )
            .await
            .unwrap();
        assert_eq!(n, 10);
        let _ = std::fs::remove_file(dst);
        assert_eq!(n, 10);
    }
}
