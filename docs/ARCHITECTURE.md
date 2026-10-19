# 架构设计

> 目标：**Windows 10/11 与 Linux 单可执行文件可用的 GUI 网盘管理工具**，
> 从 V2（Python/PySide6 + 三家独立 Python 适配器）重写为纯 Rust。

## 1. 总览

```
┌───────────────────────────────────────────────────────────────┐
│ wp-gui（eframe / egui）                                        │
│  顶栏：网盘导航 + 账号/容量    左栏：功能页    中央：页面内容      │
│  页面：网盘 / 传输 / 敏感词 / 日志 / 设置 / 关于                    │
│  对话框：登录(扫码/Cookie/短信/令牌)、新建/重命名、删除确认、        │
│          移动/复制目录选择、分享、详情、跨盘传输、本地文件选择器      │
└──────────────┬────────────────────────────────────────────────┘
               │ 事件（mpsc）          命令（tokio::spawn）
┌──────────────▼────────────────────────────────────────────────┐
│ wp-transfer 传输引擎                                            │
│  任务库(JSON) · 小/大文件分级并发 · 重试退避 · 暂停/继续/取消       │
│  跨盘：下载到缓存 → 上传 · 上传前敏感词守卫钩子                     │
└──────────────┬────────────────────────────────────────────────┘
               │ Arc<dyn Drive>
┌──────────────▼────────────────────────────────────────────────┐
│ wp-drive：统一 Drive trait（文件/传输/分享/回收站/云添加/记录/   │
│           分享转存/分类浏览，能力集 DriveCaps 逐家裁剪）         │
│  ├─ wp-drive-baidu   (百度 PCS 协议)                            │
│  ├─ wp-drive-quark   (夸克 clouddrive 协议)                     │
│  ├─ wp-drive-guangya (光鸭 + 阿里云 OSS V1 签名)                 │
│  ├─ local            (本机文件系统)                              │
│  └─ fake             (内存演示网盘)                              │
└──────────────┬────────────────────────────────────────────────┘
               │
┌──────────────▼────────────────────────────────────────────────┐
│ wp-net：HTTP 客户端 · Cookie · MD5/SHA1/HMAC/Base64 ·            │
│          下载器（断点续传 + 多段并发） · 重试策略                   │
└──────────────┬────────────────────────────────────────────────┘
               │
┌──────────────▼────────────────────────────────────────────────┐
│ wp-core：数据模型 · 配置 · 路径 · 日志 · 敏感词守卫 · 工具        │
└───────────────────────────────────────────────────────────────┘
```

依赖方向严格单向：`wp-gui → wp-transfer → wp-drive → wp-net → wp-core`，
适配器 crate 只依赖 `wp-core/wp-net/wp-drive`，互不依赖（可在各自 crate 内独立测试）。

## 2. 关键设计决策

### 2.1 进程内 trait 对象取代「子进程 + JSON 行协议」

V2 的真实约束：三家 Python 适配器的顶层包都叫 `核心`/`界面`，同一解释器 import 会互相覆盖，
所以每个适配器跑独立子进程，主程序用行式 JSON 桥调用（`网盘管理_V2/v8_3/桥/`）。

Rust 没有顶层包冲突问题，直接：

```rust
pub trait Drive: Send + Sync { /* 登录 / 浏览 / 传输 / 分享 */ }
let drive: Arc<dyn Drive> = wp_drive_baidu::create(init)?;
```

收益：删掉桥进程管理、JSON 序列化往返、Python 自举/venv、跨语言错误翻译；
调用变成类型安全的 `Result<RemoteEntry>`，编译期就能发现协议字段错配。

### 2.2 全异步网络 + 单线程 GUI

* 一个 tokio 多线程 runtime（4 worker）在 `main` 里创建，生命周期覆盖整个进程；
* GUI 线程（eframe）从不 `await`，也不做阻塞 IO：所有操作 `spawn` 到 runtime，
  结果通过 `std::sync::mpsc::Sender<Event>` 回到 GUI，并调用 `egui::Context::request_repaint()` 唤醒渲染；
* 传输引擎维护 `Arc<Mutex<任务表>>`，GUI 每帧取一次快照（300ms 节流），进度条/速度自然刷新；
* 重活（下载分片合并）走 `spawn_blocking`，不会卡住 render。

### 2.3 事件驱动的 UI 状态机

页面渲染遵循「本帧只读快照 → 收集动作 → 统一应用」：

```rust
let snap = snap(app, drive_id);          // Arc 共享条目，克隆代价低
let mut actions = Vec::new();
// ... render，只往 actions 里 push（Navigate/Download/Delete/...）
for a in actions { apply(app, a); }      // 这里才 &mut App
```

这样避免了 UI 闭包内 `&mut App` 与只读借用冲突，也让「用户动作」可枚举、可测试。
异步动作（列目录/登录/上传）统一包装成 `Event`，在 `App::poll_events()` 里更新状态；
列表请求带自增 `token`，旧请求返回时自动丢弃，避免快速切目录时的新旧覆盖。

### 2.4 传输引擎

任务状态机（`wp-core::model::TaskState`）：

```
Queued ──调度──> Running ──> Done
   │                │
   │                ├─暂停──> Paused ──继续──> (重新调度，下载断点续传)
   │                ├─取消──> Canceled
   │                └─重试耗尽/不可重试──> Failed ──重试──> Queued
```

* **并发**：小文件（< `small_file_threshold`）走 `small_file_concurrency` 信号量，
  大文件走 `large_file_concurrency` 信号量（默认 16 / 3，与 V2 对齐）；
* **暂停**：通过进度回调返回 `false` 实现「安全暂停」——适配器在下一个分片/块边界退出，
  返回 `Error::Canceled`，引擎按控制位把状态置为 `Paused`（保留断点）或 `Canceled`；
* **跨盘**：源盘 `download` 到 `缓存/<task_id>.tmp` → 目标盘 `upload`，成功后删临时文件，
  进度按下载 50% + 上传 50% 加权；失败按配置决定是否保留临时文件；
* **目录操作的任务展开（GUI 层，对齐 V8）**：勾选目录下载/跨盘/上传文件夹时，**先异步预扫描
  源目录**（`wp_transfer::scan_remote_dir` / `scan_local_dir`），再把目录展开成「一个文件一个任务」；
  空目录（0 文件）**不创建任何任务**，只给出明确提示（避免"假完成 0%"）；空子目录在目标端补建。
  引擎仍保留目录递归能力（`prepare_total`/`execute_once` 的目录分支），供 CLI / 验证工具使用；
* **目录（多重嵌套，引擎层）**：`prepare_total` 递归扫描（本地 `walk_local_tree` / 远端
  `walk_remote_tree`，含空目录）求出总字节；执行时先建目录树再逐文件传输，
  目标端保持相对路径结构；单文件进度用 `base + 当前文件内进度` 映射成整任务进度，
  跨盘目录按「文件下载 0→50% + 上传 50%→100%」加权；
* **空目录与零字节**：源目录 0 文件时任务仍算 Done 但进度显示 100%（`TransferTask::progress`
  对 Done 恒返回 1.0），备注写明「空目录：0 个文件…（已创建目标目录）」；目录上传/跨盘
  重试前先 `stat` 目标文件——同名同大小跳过、不同则先删后传，保证重试幂等（避免适配器
  冲突自动改名产生 `a(1).txt` 副本）；
* **百度只读会话**：写请求依赖 `gettemplatevariable` 换取的 bdstoken；若只有
  `/api/loginStatus` 兜底成功，则标记 `session.write_ok=false`，账号状态显示
  「已登录（只读）」，写操作返回可执行的 errno:-6 指引（重新扫码/导入最新 Cookie）。
* **持久化**：入队/状态变化时标记脏，`save()` 节流原子写 `任务.json`；
  重启 `load()` 时把 `Running` 降级为 `Paused/Queued`，已结束任务保留展示。

### 2.5 凭证与多账号

`WP_DATA_DIR`/数据目录下 `账号/<实例标识>/凭证.json`（每家格式不同，各自 serde 结构）。
同一家网盘第二个账号只需新增一个实例（`baidu_2`），凭证目录彼此隔离，
不需要 V2 那样复制一整个适配器项目目录。

**新增入口（2026-10-19 二次补齐）**：

* GUI 统一走「添加网盘 / 本机」**选择弹窗**（`Dialog::AddDrive`）：顶栏「＋ 添加」常驻
  （无实例时也存在）、网盘页空状态按钮、设置页按钮三处入口；弹窗按类型列出
  百度/夸克/光鸭/本机/演示与已有实例数，本机/演示为单实例（已有→「切换」，不重复创建）；
* `App::add_drive_from_dialog`：单实例类型复用/启用已有实例，云盘走
  `add_account_and_login`；有历史凭证时自动恢复登录而不误弹二维码；
* 修复「删除某类全部实例后无法再添加」逻辑漏洞：原先顶栏添加入口是
  `if let Some(当前实例)` 条件渲染，实例全删后入口消失，只剩设置页深入口；
  现在弹窗入口与实例状态解耦，删光也能重建。

**新增账号入口（2026-10-19 补齐，对齐 V8_3 的「同一网盘多账号」）**：

* 顶栏网盘标签右侧 **「＋ 账号」**：按当前网盘类型新增实例（`baidu` → `baidu_2`），
  立即切换到新实例并**自动弹出登录对话框**（`App::add_account_and_login`）；
* 右键任意顶栏网盘标签：「登录/更换账号、退出登录、刷新、再加一个同类型账号、移除实例」；
* 设置页「类型下拉 + ＋ 新增网盘实例」走同一 helper，行为一致；
* 顶栏按钮悬停显示「实例 id / 账号（用户名）/ 登录状态」，多账号可快速区分；
* 启动时按**实例 id** 在已加载槽位中定位（修复了此前「禁用某个实例后按 cfg 下标定位
  会选错网盘/账号」的 bug），并记住上次选中的实例（`ui.last_drive`）。

会话过期自愈：适配器把「401/登录失效/凭证缺失」翻译成 `Error::Auth`；
GUI 把该实例状态标为「未登录」，用户点「登录」重新扫码即可，其它网盘不受影响。

### 2.6 敏感词守卫

* 词库 JSON（全局 + 按实例），`aho-corasick` 构建匹配器，`LeftmostLongest` 语义取最长命中
  （与 V2 的 AC 自动机语义一致）；
* 命中策略三档：警告 / 拦截 / 自动改名；
* 自动改名：命中词 → 拼音（`pinyin` crate），再按目标网盘字符集规避
  （百度/本机：非法字符全角化、控制字符折叠、星平面 emoji 移除、清空则 `未命名_<md5前4位>`）；
* 守卫作为 `UploadGuardHook` 注入传输引擎，不侵入网盘协议实现。

### 2.7 跨平台细节

| 问题 | 方案 |
|---|---|
| GUI 依赖 | eframe/egui（OpenGL/glow），Windows 用系统 OpenGL/D3D 转译，Linux 用 GLX/EGL；不依赖 Qt/GTK/WebView，无运行时 DLL 地狱 |
| 中文字体 | 内置探测表：Windows `msyh.ttc/Deng.ttf/simhei.ttf`，Linux `NotoSansCJK*.ttc`（index=2 简中）/ Dream Han / 文泉驿，macOS PingFang；也可在设置里指定字体文件 |
| 控制台窗口 | 发布版 `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`，Windows 双击不弹黑框 |
| 数据目录 | 绿色模式（exe 旁 `portable`/`数据/`）优先，否则 `%APPDATA%` / `~/.local/share`；`WP_DATA_DIR` 可覆盖 |
| 打开目录 | Windows `cmd /C start`，Linux `xdg-open`（`wp-core::paths::open_path`） |
| 路径分隔符 | 统一逻辑路径 `/` 开头 POSIX 风格；本机驱动内部自行 `strip_prefix` 映射 |
| 原子写 | 同目录临时文件 + rename；Windows 目标已存在时先删再 rename |

## 3. GUI 结构（wp-gui）

```
main.rs        命令行参数 / 数据目录 / tokio runtime / 字体 + 主题 / run_native
app.rs         全局状态 App、Event 通道、页面布局、快捷键、截图验收模式
theme.rs       10 套主题色板 + egui Visuals 映射
fonts.rs       中文字体探测与注入、QR PNG → 纹理
widgets.rs     卡片/徽标/按钮/进度条/空态/文件图标等通用小部件
pages/         browser / transfers / tools / guard_page / logs / settings / about
thumbs.rs      缩略图缓存（异步拉取 → 解码 → egui 纹理，列表/详情共用）
../../assets/  应用图标（SVG/多尺寸 PNG/ICO）、desktop 模板、品牌横幅
dialogs.rs     全部模态对话框（Modal）
```

UI 信息架构沿用 V2：**顶栏网盘导航 + 左侧功能栏 + 中央内容 + 底部状态栏**。
相比 V2 的增强：前进/后退历史、键盘快捷键、跨平台文件选择器（不依赖系统对话框）、
内嵌截图验收（`--screenshot`）使 GUI 可在无显示服务器环境自动回归。

## 4. 可验证性

| 层次 | 手段 |
|---|---|
| 纯逻辑 | `cargo test`：路径/格式化/配置/敏感词/签名/下载器（含本地 HTTP 测试服务器）/传输引擎（用假网盘驱动跑真实任务） |
| GUI | Xvfb + `--screenshot` 截 7 个页面；CI 里自动断言 PNG 产出 |
| 跨平台 | GitHub Actions：ubuntu-latest + windows-latest 编译/测试/打包 |
| 真实网盘 | 见 [待验证清单.md](待验证清单.md)（需账号手工回归） |
