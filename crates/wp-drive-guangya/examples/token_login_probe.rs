//! 用网页里抓到的凭证 JSON 验证光鸭令牌登录（access_token/refresh_token）。
//! 用法：cargo run -p wp-drive-guangya --example token_login_probe -- /tmp/gy_creds.json
use std::path::PathBuf;
use wp_drive::{Drive, DriveInit};

#[tokio::main]
async fn main() {
    let p = std::env::args().nth(1).expect("需要凭证 JSON 路径");
    let token = std::fs::read_to_string(&p).expect("读凭证失败");
    let dir = PathBuf::from("/tmp/wp-gy-token-probe");
    std::fs::create_dir_all(&dir).ok();
    let drive = wp_drive_guangya::create(DriveInit {
        id: "probe".into(),
        name: "probe".into(),
        data_dir: dir,
        threads: 2,
        verbose: true,
    })
    .expect("create");
    match drive.login_token(&token, "").await {
        Ok(a) => println!("TOKEN_LOGIN_OK name={}", a.display_name),
        Err(e) => {
            eprintln!("TOKEN_LOGIN_ERR {e}");
            std::process::exit(1);
        }
    }
}
