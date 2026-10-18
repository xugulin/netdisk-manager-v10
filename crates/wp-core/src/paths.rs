//! 跨平台路径与数据目录。
//!
//! 绿色版优先：可执行文件旁边的 `数据/` 若可写就用它（U 盘 / 解压即用）；
//! 否则落到系统用户数据目录（Windows: `%APPDATA%/网盘管理V10`，
//! Linux: `~/.local/share/网盘管理V10`）。
//!
//! 可用环境变量 `WP_DATA_DIR` 强制指定数据目录（开发/测试用）；
//! 也可在可执行文件旁创建空文件 `portable` 强制绿色模式。

use std::path::{Path, PathBuf};

/// 数据目录集合。
#[derive(Debug, Clone)]
pub struct Paths {
    /// 数据根（配置 / 日志 / 账号凭证 / 任务库都在这下面）
    pub root: PathBuf,
    /// 可执行文件所在目录
    pub exe_dir: PathBuf,
    /// 是否绿色模式（数据在 exe 旁边）
    pub portable: bool,
}

impl Paths {
    /// 探测并创建数据目录。
    pub fn detect() -> Self {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));

        // 1) 环境变量强制指定
        if let Ok(dir) = std::env::var("WP_DATA_DIR") {
            let dir = PathBuf::from(dir);
            let _ = std::fs::create_dir_all(&dir);
            return Self {
                root: dir,
                exe_dir,
                portable: false,
            };
        }

        // 2) 绿色模式：exe 旁边有 portable 文件，或已经存在「数据」目录
        let candidate = exe_dir.join("数据");
        let force_portable = exe_dir.join("portable").is_file();
        if force_portable || candidate.is_dir() {
            if std::fs::create_dir_all(&candidate).is_ok() && is_writable(&exe_dir) {
                return Self {
                    root: candidate,
                    exe_dir,
                    portable: true,
                };
            }
        }

        // 3) 系统用户数据目录
        let base = dirs::data_dir()
            .unwrap_or_else(|| exe_dir.clone())
            .join("网盘管理V10");
        let _ = std::fs::create_dir_all(&base);
        Self {
            root: base,
            exe_dir,
            portable: false,
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("配置.json")
    }
    pub fn log_dir(&self) -> PathBuf {
        self.root.join("日志")
    }
    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("缓存")
    }
    pub fn account_dir(&self) -> PathBuf {
        self.root.join("账号")
    }
    pub fn account_path(&self, drive_id: &str) -> PathBuf {
        self.account_dir().join(drive_id)
    }
    pub fn tasks_file(&self) -> PathBuf {
        self.root.join("任务.json")
    }
    pub fn sensitive_file(&self) -> PathBuf {
        self.root.join("敏感词.json")
    }
    pub fn download_dir(&self) -> PathBuf {
        let d = self.root.join("下载");
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// 确保标准子目录存在。
    pub fn ensure(&self) {
        for d in [
            self.log_dir(),
            self.cache_dir(),
            self.account_dir(),
            self.download_dir(),
        ] {
            let _ = std::fs::create_dir_all(d);
        }
    }

    /// 取（或生成）一个唯一的账号数据目录名，例如 `baidu`、`baidu_2`。
    pub fn unique_account_id(&self, base: &str, used: &[String]) -> String {
        if !used.iter().any(|x| x == base) {
            return base.to_string();
        }
        let mut i = 2;
        loop {
            let cand = format!("{base}_{i}");
            if !used.iter().any(|x| x == &cand) {
                return cand;
            }
            i += 1;
        }
    }
}

fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(".wp_write_test");
    match std::fs::write(&probe, b"1") {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 原子写文件：先写同目录临时文件，再 rename 覆盖。
/// Windows 上 rename 目标已存在会失败，因此先删目标（配置/任务库都是单写者，可接受）。
pub fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("dat")
    ));
    std::fs::write(&tmp, data)?;
    #[cfg(windows)]
    {
        let _ = std::fs::remove_file(path);
    }
    std::fs::rename(&tmp, path).or_else(|e| {
        // 跨设备 / 权限等极端情况：退化为直接写
        let _ = std::fs::remove_file(&tmp);
        std::fs::write(path, data).map_err(|_| e)
    })
}

/// 用系统默认程序打开目录/文件（跨平台，失败返回 err 文本）。
pub fn open_path(path: &Path) -> Result<(), String> {
    let p = path.to_string_lossy().to_string();
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", &p])
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&p)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(&p)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}
