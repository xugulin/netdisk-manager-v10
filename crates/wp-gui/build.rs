//! 构建脚本：仅 Windows 下把 .ico 嵌入可执行文件（资源管理器/任务栏图标）。
//!
//! 其它平台不做任何事；即使 Windows 资源编译失败也只告警，不阻塞构建。

fn main() {
    println!("cargo:rerun-if-changed=../../assets/icons/netdisk-manager.ico");

    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("../../assets/icons/netdisk-manager.ico");
        res.set("FileDescription", "网盘管理 NetDisk Manager");
        res.set("ProductName", "网盘管理");
        if let Err(e) = res.compile() {
            println!("cargo:warning=嵌入 Windows 图标失败（不影响运行）：{e}");
        }
    }
}
