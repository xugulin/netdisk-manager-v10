# vendor：固定版本源码与补丁（源码可控）

| 目录 | 来源 | 版本/许可 | 补丁 |
|---|---|---|---|
| `glutin-winit/` | crates.io `glutin-winit 0.5.0` | MIT（见 LICENSE） | Windows 下把默认 API 优先级 `WglThenEgl` 改为 `EglThenWgl`（优先 ANGLE），见 `src/lib.rs` 注释 |

用途：满足「源码可控 + 可审计」要求。所有第三方源码保留原始 LICENSE，修改处在文件内注明 `[网盘管理 V10 patch]`。
libmpv 及其依赖（FFmpeg/libass/libplacebo/libbluray）的 vendored 构建由 `scripts/build-media-stack-*.{sh,ps1}` 生成，
产物放 `vendor/prebuilt/<platform>/`（不进 git），补丁放 `vendor/patches/`。
