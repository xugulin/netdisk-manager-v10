//! wp-cli —— 网盘管理命令行入口（真机联调 / 脚本化验收 / CI 用）。
//!
//! GUI 之外的第二入口，能力与 GUI 共用同一套适配器与传输引擎：
//! * `login-qr` 会把二维码 PNG 存盘并打印路径（无显示环境也能让用户扫码）；
//! * 所有结果默认输出 JSON（`--json` 可省略，始终 JSON）；
//! * 进度输出 `PROGRESS done/total` 到 stderr；错误 `ERROR ...` 到 stderr 且退出码非 0。
//!
//! 用法示例：
//! ```text
//! wp-cli --drive baidu login-qr --qr-out /tmp/baidu.png --timeout 300
//! wp-cli --drive baidu account
//! wp-cli --drive baidu ls /
//! wp-cli --drive baidu upload /tmp/测试 1GB 文件.bin /测试目录 --name "重命名后的名字.bin"
//! wp-cli --drive baidu download /测试目录/文件.bin /tmp/dl.bin
//! wp-cli --drive baidu share /测试目录/文件.bin --days 7
//! ```
//!
//! 数据目录：默认与 GUI 相同的 `Paths::detect()`（支持 `WP_DATA_DIR` 环境变量 / `--data-dir`）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wp_core::config::AppConfig;
use wp_core::logging::{self, LogLevel};
use wp_core::model::*;
use wp_core::Paths;
use wp_drive::{Drive, DriveInit};

fn main() {
    let code = real_main();
    std::process::exit(code);
}

fn real_main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut data_dir: Option<PathBuf> = None;
    let mut drive_id = "demo".to_string();
    let mut verbose = false;
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--data-dir" => {
                i += 1;
                data_dir = args.get(i).map(PathBuf::from);
            }
            "--drive" | "-d" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    drive_id = v.clone();
                }
            }
            "--verbose" | "-v" => verbose = true,
            "--help" | "-h" => {
                print_help();
                return 0;
            }
            "--version" | "-V" => {
                println!("wp-cli {}", wp_core::APP_VERSION);
                return 0;
            }
            _ => rest.push(args[i].clone()),
        }
        i += 1;
    }
    if let Some(d) = &data_dir {
        std::env::set_var("WP_DATA_DIR", d);
    }
    let paths = Paths::detect();
    paths.ensure();
    logging::init(
        &paths.log_dir(),
        if verbose {
            LogLevel::Debug
        } else {
            LogLevel::Warn
        },
        true,
    );

    if rest.is_empty() {
        print_help();
        return 2;
    }

    let cfg = AppConfig::load(&paths.config_file());
    let Some(dcfg) = cfg.drive(&drive_id).cloned() else {
        eprintln!(
            "ERROR: 配置里没有网盘实例 `{drive_id}`（可用：{}）",
            cfg.drives
                .iter()
                .map(|d| d.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        return 2;
    };
    let init = DriveInit {
        id: dcfg.id.clone(),
        name: dcfg.display_name().to_string(),
        data_dir: paths.account_path(&dcfg.id),
        threads: 0,
        verbose,
    };
    let drive: wp_core::Result<Arc<dyn Drive>> = match dcfg.kind {
        DriveKind::Baidu => wp_drive_baidu::create(init),
        DriveKind::Quark => wp_drive_quark::create(init),
        DriveKind::Guangya => wp_drive_guangya::create(init),
        DriveKind::Local => wp_drive::local::create(init),
        DriveKind::Fake => wp_drive::fake::create(init),
    };
    let drive = match drive {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ERROR: 创建适配器失败：{e}");
            return 2;
        }
    };

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("ERROR: 创建 tokio runtime 失败：{e}");
            return 2;
        }
    };
    let cmd = rest[0].clone();
    let cmd_args = &rest[1..];
    rt.block_on(async move { dispatch(&*drive, &cmd, cmd_args).await })
}

async fn dispatch(drive: &dyn Drive, cmd: &str, args: &[String]) -> i32 {
    match cmd {
        "auth-caps" => match drive.auth_caps().await {
            Ok(caps) => print_json(&caps),
            Err(e) => fail(e),
        },
        "account" => match drive.account().await {
            Ok(info) => print_json(&info),
            Err(e) => fail(e),
        },
        "login-qr" => cmd_login_qr(drive, args).await,
        "login-cookie" => {
            let text = join_text(args);
            if text.trim().is_empty() {
                return usage("login-cookie <cookie 文本或 @文件>");
            }
            let text = if let Some(path) = text.strip_prefix('@') {
                match std::fs::read_to_string(path) {
                    Ok(s) => s,
                    Err(e) => return fail(e.to_string()),
                }
            } else {
                text
            };
            match drive.login_cookie(&text).await {
                Ok(info) => print_json(&info),
                Err(e) => fail(e),
            }
        }
        "login-sms-send" => match args.first() {
            Some(phone) => match drive.send_sms(phone).await {
                Ok(hint) => print_json(&serde_json::json!({"状态": "已发送", "提示": hint})),
                Err(e) => fail(e),
            },
            None => usage("login-sms-send <手机号>"),
        },
        "login-sms-verify" => match (args.first(), args.get(1)) {
            (Some(phone), Some(code)) => match drive.verify_sms(phone, code).await {
                Ok(info) => print_json(&info),
                Err(e) => fail(e),
            },
            _ => usage("login-sms-verify <手机号> <验证码>"),
        },
        "login-token" => match args.first() {
            Some(token) => match drive.login_token(token, "").await {
                Ok(info) => print_json(&info),
                Err(e) => fail(e),
            },
            None => usage("login-token <token>"),
        },
        "logout" => match drive.logout().await {
            Ok(()) => {
                print_json(&serde_json::json!({"状态": "已退出登录"}));
                0
            }
            Err(e) => fail(e),
        },
        "caps" => {
            let caps = drive.caps();
            print_json(&serde_json::json!({
                "kind": drive.kind(),
                "id": drive.id(),
                "name": drive.name(),
                "auth": caps.auth,
                "mkdir": caps.mkdir,
                "rename": caps.rename,
                "move_copy": caps.move_copy,
                "search": caps.search,
                "share": caps.share,
                "direct_link": caps.direct_link,
                "recycle": caps.recycle,
                "upload": caps.upload,
                "download": caps.download,
            }))
        }
        "ls" | "list" => {
            let path = args.first().cloned().unwrap_or_else(|| "/".into());
            match drive.list(&path).await {
                Ok(entries) => {
                    eprintln!("# {} 条目（{}）", entries.len(), path);
                    print_json(&entries)
                }
                Err(e) => fail(e),
            }
        }
        "stat" => match args.first() {
            Some(path) => match drive.stat(path).await {
                Ok(e) => print_json(&e),
                Err(e) => fail(e),
            },
            None => usage("stat <path>"),
        },
        "mkdir" => match args.first() {
            Some(path) => match drive.ensure_dir(path).await {
                Ok(e) => print_json(&e),
                Err(e) => fail(e),
            },
            None => usage("mkdir <path>"),
        },
        "rename" => match (args.first(), args.get(1)) {
            (Some(path), Some(new_name)) => match drive.rename(path, new_name).await {
                Ok(e) => print_json(&e),
                Err(e) => fail(e),
            },
            _ => usage("rename <path> <new_name>"),
        },
        "rm" | "delete" => {
            if args.is_empty() {
                return usage("rm <path...>");
            }
            match drive.delete(args).await {
                Ok(r) => print_json(&r),
                Err(e) => fail(e),
            }
        }
        "mv" | "move" => {
            if args.len() < 2 {
                return usage("mv <目标目录> <path...>");
            }
            match drive.move_entries(&args[1..], &args[0]).await {
                Ok(r) => print_json(&r),
                Err(e) => fail(e),
            }
        }
        "cp" | "copy" => {
            if args.len() < 2 {
                return usage("cp <目标目录> <path...>");
            }
            match drive.copy_entries(&args[1..], &args[0]).await {
                Ok(r) => print_json(&r),
                Err(e) => fail(e),
            }
        }
        "search" => match args.first() {
            Some(kw) => {
                let dir = args.get(1).cloned().unwrap_or_else(|| "/".into());
                match drive.search(kw, &dir).await {
                    Ok(entries) => print_json(&entries),
                    Err(e) => fail(e),
                }
            }
            None => usage("search <keyword> [dir]"),
        },
        "link" => match args.first() {
            Some(path) => match drive.direct_link(path).await {
                Ok(link) => print_json(&link),
                Err(e) => fail(e),
            },
            None => usage("link <remote_path>"),
        },
        "upload" => cmd_upload(drive, args).await,
        "download" => cmd_download(drive, args).await,
        "share" => cmd_share(drive, args).await,
        "recycle-list" => match drive.recycle_list().await {
            Ok(list) => {
                eprintln!("# 回收站 {} 项", list.len());
                print_json(&list)
            }
            Err(e) => fail(e),
        },
        "recycle-restore" => {
            if args.is_empty() {
                return usage("recycle-restore <id...>");
            }
            match drive.recycle_restore(args).await {
                Ok(r) => print_json(&r),
                Err(e) => fail(e),
            }
        }
        "recycle-delete" => {
            if args.is_empty() {
                return usage("recycle-delete <id...>");
            }
            match drive.recycle_delete(args).await {
                Ok(r) => print_json(&r),
                Err(e) => fail(e),
            }
        }
        "recycle-clear" => match drive.recycle_clear().await {
            Ok(r) => print_json(&r),
            Err(e) => fail(e),
        },
        "cloud-add" => match args.first() {
            Some(url) => {
                let dest = args.get(1).cloned().unwrap_or_else(|| "/".into());
                match drive.cloud_add(url, &dest).await {
                    Ok(id) => print_json(&serde_json::json!({"任务ID": id, "保存到": dest})),
                    Err(e) => fail(e),
                }
            }
            None => usage("cloud-add <直链或 magnet> [保存目录]"),
        },
        "cloud-tasks" => match drive.cloud_tasks().await {
            Ok(list) => {
                eprintln!("# 云任务 {} 项", list.len());
                print_json(&list)
            }
            Err(e) => fail(e),
        },
        "records" => {
            let limit = args
                .first()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(100);
            match drive.upload_records(limit).await {
                Ok(list) => {
                    eprintln!("# 记录 {} 项", list.len());
                    print_json(&list)
                }
                Err(e) => fail(e),
            }
        }
        "share-info" => match args.first() {
            Some(url) => {
                let pwd = args.get(1).cloned().unwrap_or_default();
                match drive.share_info(url, &pwd).await {
                    Ok(detail) => print_json(&detail),
                    Err(e) => fail(e),
                }
            }
            None => usage("share-info <分享链接> [提取码]"),
        },
        "share-save" => match (args.first(), args.get(1)) {
            (Some(url), Some(dest)) => {
                let pwd = args.get(2).cloned().unwrap_or_default();
                match drive.save_shared(url, &pwd, dest).await {
                    Ok(r) => print_json(&r),
                    Err(e) => fail(e),
                }
            }
            _ => usage("share-save <分享链接> <保存目录> [提取码]"),
        },
        "categories" => {
            let cat = args.first().cloned().unwrap_or_else(|| "all".into());
            match drive.category_list(&cat).await {
                Ok(list) => {
                    eprintln!("# 分类 {cat} 下 {} 项", list.len());
                    print_json(&list)
                }
                Err(e) => fail(e),
            }
        }
        "thumb" => match (args.first(), args.get(1)) {
            (Some(remote), Some(out)) => {
                let path = wp_core::util::norm_path(remote);
                match drive.stat(&path).await {
                    Ok(mut entry) => {
                        // 有些网盘的 stat(filemetas) 响应不含缩略图字段，列表接口才带；
                        // 这里用父目录列表做一次兜底（只多一次列表请求）。
                        if entry.thumbnail.is_none() && !entry.is_dir {
                            let parent = wp_core::util::parent_path(&path);
                            if let Ok(list) = drive.list(&parent).await {
                                if let Some(found) = list.into_iter().find(|e| e.path == path) {
                                    entry = found;
                                }
                            }
                        }
                        match drive.fetch_thumbnail(&entry).await {
                            Ok(bytes) => {
                                if let Err(e) = std::fs::write(out, &bytes) {
                                    return fail(wp_core::Error::Io(e));
                                }
                                print_json(&serde_json::json!({
                                    "文件": path,
                                    "字节": bytes.len(),
                                    "输出": out,
                                }))
                            }
                            Err(e) => fail(e),
                        }
                    }
                    Err(e) => fail(e),
                }
            }
            _ => usage("thumb <远端路径> <输出.png>"),
        },
        "shares" => match drive.list_shares().await {
            Ok(list) => print_json(&list),
            Err(e) => fail(e),
        },
        "cancel-shares" => {
            if args.is_empty() {
                return usage("cancel-shares <share_id...>");
            }
            match drive.cancel_shares(args).await {
                Ok(r) => print_json(&r),
                Err(e) => fail(e),
            }
        }
        "write-probe" => match drive.write_probe().await {
            Ok(msg) => print_json(&serde_json::json!({"可写": true, "消息": msg})),
            Err(e) => fail(e),
        },
        other => usage(&format!("未知命令：{other}（--help 看用法）")),
    }
}

async fn cmd_login_qr(drive: &dyn Drive, args: &[String]) -> i32 {
    let mut qr_out: Option<PathBuf> = None;
    let mut timeout_secs = 300u64;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--qr-out" => {
                i += 1;
                qr_out = args.get(i).map(PathBuf::from);
            }
            "--timeout" => {
                i += 1;
                timeout_secs = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(300);
            }
            _ => {}
        }
        i += 1;
    }
    let ch = match drive.begin_qr().await {
        Ok(ch) => ch,
        Err(e) => return fail(e),
    };
    if let Some(url) = &ch.url {
        println!("URL: {url}");
    }
    if let Some(code) = &ch.code {
        println!("CODE: {code}");
    }
    if !ch.hint.is_empty() {
        println!("HINT: {}", ch.hint);
    }
    if let Some(png) = &ch.image_png {
        let path = qr_out.unwrap_or_else(|| std::env::temp_dir().join("wp-cli-qr.png"));
        if let Err(e) = std::fs::write(&path, png) {
            return fail(format!("保存二维码失败：{e}"));
        }
        println!("QR_PATH: {}", path.display());
        println!("QR_BYTES: {}", png.len());
    } else {
        println!("QR_IMAGE: none");
    }
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut last = String::new();
    loop {
        if Instant::now() > deadline {
            println!("TIMEOUT");
            return 3;
        }
        match drive.poll_qr().await {
            Ok(poll) => {
                let state = format!("{:?}", poll.state);
                if state != last || !poll.message.is_empty() {
                    println!("STATE: {state} {}", poll.message);
                    last = state.clone();
                }
                match poll.state {
                    QrState::Success => {
                        println!("LOGIN: success");
                        return match drive.account().await {
                            Ok(info) => print_json(&info),
                            Err(e) => fail(e),
                        };
                    }
                    QrState::Expired => {
                        println!("EXPIRED");
                        return 2;
                    }
                    QrState::Canceled => {
                        println!("CANCELED");
                        return 2;
                    }
                    _ => {}
                }
            }
            Err(e) => {
                eprintln!("POLL_ERROR: {e}");
                if e.is_auth() {
                    return fail(e);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }
}

async fn cmd_upload(drive: &dyn Drive, args: &[String]) -> i32 {
    let mut positional: Vec<String> = Vec::new();
    let mut rename_to: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--name" => {
                i += 1;
                rename_to = args.get(i).cloned();
            }
            _ => positional.push(args[i].clone()),
        }
        i += 1;
    }
    if positional.len() < 2 {
        return usage("upload <本地文件> <远端目录> [--name <远端文件名>]");
    }
    let local = PathBuf::from(&positional[0]);
    let remote_dir = positional[1].clone();
    let meta = match std::fs::metadata(&local) {
        Ok(m) => m,
        Err(e) => return fail(format!("本地文件不可读：{}：{e}", local.display())),
    };
    eprintln!(
        "# 上传 {}（{} 字节）→ {}",
        local.display(),
        meta.len(),
        remote_dir
    );
    let progress = make_progress(meta.len());
    let opts = UploadOptions {
        rename_to,
        ..Default::default()
    };
    match drive.upload(&local, &remote_dir, &opts, progress).await {
        Ok(entry) => print_json(&entry),
        Err(e) => fail(e),
    }
}

async fn cmd_download(drive: &dyn Drive, args: &[String]) -> i32 {
    if args.len() < 2 {
        return usage("download <远端路径> <本地路径>");
    }
    let remote = args[0].clone();
    let local = PathBuf::from(&args[1]);
    let size = drive.stat(&remote).await.map(|e| e.size).unwrap_or(0);
    eprintln!("# 下载 {} → {}（{} 字节）", remote, local.display(), size);
    let progress = make_progress(size);
    let opts = DownloadOptions {
        save_as: Some(local.clone()),
        ..Default::default()
    };
    let start = Instant::now();
    match drive.download(&remote, &local, &opts, progress).await {
        Ok(bytes) => {
            let secs = start.elapsed().as_secs_f64().max(0.001);
            print_json(&serde_json::json!({
                "本地路径": local.display().to_string(),
                "字节": bytes,
                "耗时秒": (secs * 100.0).round() / 100.0,
                "平均速度MBs": ((bytes as f64 / 1048576.0) / secs * 100.0).round() / 100.0,
            }))
        }
        Err(e) => fail(e),
    }
}

async fn cmd_share(drive: &dyn Drive, args: &[String]) -> i32 {
    let mut paths: Vec<String> = Vec::new();
    let mut days = 7u32;
    let mut password: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--days" => {
                i += 1;
                days = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(7);
            }
            "--password" => {
                i += 1;
                password = args.get(i).cloned();
            }
            _ => paths.push(args[i].clone()),
        }
        i += 1;
    }
    if paths.is_empty() {
        return usage("share <path...> [--days N] [--password P]");
    }
    let opts = wp_drive::ShareOptions {
        expire_days: days,
        password,
        title: String::new(),
    };
    match drive.create_share(&paths, &opts).await {
        Ok(info) => print_json(&info),
        Err(e) => fail(e),
    }
}

fn make_progress(total_hint: u64) -> ProgressFn {
    let start = Instant::now();
    let mut last = Instant::now();
    Box::new(move |done, total| {
        if last.elapsed() >= Duration::from_millis(500) {
            last = Instant::now();
            let total = if total > 0 { total } else { total_hint };
            let secs = start.elapsed().as_secs_f64().max(0.001);
            let speed = done as f64 / 1048576.0 / secs;
            eprintln!("PROGRESS {done}/{total} {speed:.2}MB/s");
        }
        true
    })
}

fn join_text(args: &[String]) -> String {
    args.join(" ")
}

fn print_json<T: serde::Serialize>(v: &T) -> i32 {
    match serde_json::to_string_pretty(v) {
        Ok(s) => {
            println!("{s}");
            0
        }
        Err(e) => fail(format!("序列化失败：{e}")),
    }
}

fn fail(e: impl std::fmt::Display) -> i32 {
    eprintln!("ERROR: {e}");
    1
}

fn usage(msg: &str) -> i32 {
    eprintln!("用法错误：{msg}");
    2
}

fn print_help() {
    println!(
        r#"网盘管理 CLI（wp-cli）v{}

用法：
  wp-cli [--drive <实例标识>] [--data-dir <数据目录>] [-v] <命令> [参数...]

全局选项：
  --drive <id>        网盘实例标识（默认 demo；配置里的 baidu/quark/guangya/local）
  --data-dir <dir>    数据目录（等价 WP_DATA_DIR）
  -v, --verbose       DEBUG 日志

命令：
  auth-caps                       登录能力协商
  account                         账号信息（含容量）
  login-qr [--qr-out <png>] [--timeout 秒]     扫码登录（二维码存 PNG）
  login-cookie <cookie 或 @文件>                 Cookie 登录
  login-sms-send <手机号>                       发送短信验证码（光鸭）
  login-sms-verify <手机号> <验证码>             校验短信验证码
  login-token <token>                           令牌登录（光鸭）
  logout                          退出登录（只清本地凭证）
  caps                            适配器能力表
  ls [路径]                       列目录（默认 /）
  stat <路径>                     文件信息
  mkdir <路径>                    递归建目录
  rename <路径> <新名称>           重命名
  rm <路径...>                    删除
  mv <目标目录> <路径...>          移动
  cp <目标目录> <路径...>          复制
  search <关键词> [目录]           搜索
  link <远端路径>                 取下载直链
  upload <本地文件> <远端目录> [--name 名字]
  download <远端路径> <本地路径>
  share <路径...> [--days N] [--password P]
  shares                          分享列表
  cancel-shares <share_id...>     取消分享
  share-info <链接> [提取码]       校验/预览分享内容（转存前）
  share-save <链接> <目录> [提取码] 转存分享到自己的网盘
  recycle-list                    回收站列表
  recycle-restore <id...>         从回收站恢复
  recycle-delete <id...>          彻底删除回收站条目
  recycle-clear                   清空回收站
  cloud-add <链接|magnet> [目录]   云添加（离线下载/BT，光鸭）
  cloud-tasks                     云任务列表
  records [N]                     上传记录 / 操作记录
  categories [video|audio|image|doc|app|other|all]  按分类列全盘文件（百度）
  thumb <远端路径> <输出.png>      下载条目缩略图（图片/视频）
  write-probe                     写权限探测

输出：结构化结果一律 JSON（stdout）；进度/错误走 stderr。
"#,
        wp_core::APP_VERSION
    );
}
