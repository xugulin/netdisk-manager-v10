# 播放器页面 — 任务计划（提案，暂不动工）

> 状态：**仅计划，未写任何实现代码**。经用户确认后再按里程碑执行。
> 依据：[PLAYER_RESEARCH.md](PLAYER_RESEARCH.md)（只读调研报告与方案评估）。
> 估算口径：1 名熟悉本项目的 Rust 工程师，含自测与文档；不含等待用户/网络环境的时间。

---

## 0. 前置条件（开工前必须先完成）

| # | 前置 | 说明 | 责任 |
|---|---|---|---|
| P1 | 用户拍板 4 个开放问题 | 运行时策略（依赖 VLC vs 捆绑）、字幕优先级、v1 功能范围、明确不做投屏/HLS/DRM | 用户 |
| P2 | 三盘直链 Range 实测结论（M0-E1） | 决定「任意 seek」是否可行 → 直接影响 T4/T6 的验收口径 | 开发 |
| P3 | 测试机补运行时 | Linux 安装 `vlc` 运行时插件（本机当前只有开发头，插件目录为空）；GStreamer 路线则补 `gst-libav` | 开发 |
| P4 | 样例媒体集 | 1080p H.264 mp4、4K HEVC mkv、带内嵌字幕、带多音轨、纯音频 mp3/flac、损坏文件各 1 个 | 开发/用户 |
| P5 | Windows 测试环境 | 若要做 Windows 播放验收，需要 Windows 10/11 测试机（可远程） | 用户 |

---

## 1. 里程碑总览

| 里程碑 | 目标 | 产出 | 估算 | 验收门槛 |
|---|---|---|---|---|
| **M0 可行性实验** | 消除两个高风险未知项 | Range 探针报告、vmem 最小收帧实验记录、流代理 PoC | **0.5–1 人日** | E1/E2/E3/E4 全有明确结论（通过/不通过+数据） |
| **M1 MVP 内嵌播放** | 本地视频 + 网盘视频能播、暂停/进度/音量 | `wp-player`/`wp-player-vlc` crate + Player 页 + 流代理；CLI 探针命令 | **3–5 人日** | 本地 mp4、三盘各 1 个视频真机播放；拖动进度条生效（视 E1 结论） |
| **M2 体验完善** | 音轨/倍速/全屏/播放列表/断点续播/入口完善 | UI 完整版 + 持久化 + 浏览器右键/双击入口 + 本地文件选择/拖放 | **3–4 人日** | 播放矩阵全过；重启后进度恢复；无崩溃 |
| **M3 稳定性与发行** | 错误恢复、性能基线、打包、文档 | 换链重试、性能报告、Linux/Windows 打包脚本、README/验收报告 | **2–3 人日** | 链接过期自动恢复；1080p CPU 可接受；绿色版/安装包可用 |
| **M4（可选）硬件加速评估** | 决定是否上 VLC4 GPU / libmpv 后端 | 方案对比 + 最小 PoC 结论（不一定实现） | **2–5 人日** | 给出明确「上/不上 + 原因」；若上，另立任务 |
| **合计（不含 M4）** | | | **9–13 人日** | |

---

## 2. M0 可行性实验（先做，避免返工）

### E1：三盘直链 Range 探针（最高优先级）

- **目的**：确定 `DirectLink.url` 是否支持 `Range`/`Accept-Ranges`，以及过期行为。
- **方法**：对每盘各取 1 个 >64MB 视频的直链（百度/夸克/光鸭），用 `curl`（带该盘 headers）依次请求：
  1. `HEAD` → 看 `Content-Length` / `Accept-Ranges` / `Content-Type`；
  2. `GET Range: bytes=0-1023` → 期望 `206` + `Content-Range: bytes 0-1023/总长`；
  3. `GET Range: bytes=<中间>-<中间+1023>` → 验证任意偏移；
  4. 等待/伪造过期后请求 → 看 401/403/410 的具体形态（为 T4 换链策略提供依据）。
- **产出**：`docs/待验证清单.md` 追加「播放器-Range 矩阵」表（每盘：HEAD/首段/中段/过期码/结论）。
- **判定门槛**：
  - 三盘都 206 → seek 按「任意拖动」验收；
  - 仅部分 206 → 对该盘降级为「顺序播放；前进需重开链接从 0 开始或近似跳转」；
  - 全部不支持 → v1 改为「先下载到临时文件再播」（体验降级，需用户重新确认）。
- **建议实现**：先用 `curl`/临时脚本，不写产品代码。

### E2：libvlc 动态加载 + vmem 最小收帧实验

- **目的**：验证「libloading 加载 → 创建 instance/player → 设 vmem 回调 → 播放本地 mp4 → 收到帧」全链路。
- **方法**：装好 VLC 运行时（Linux `vlc` 包）后，写一个临时 bin（或 `examples/`）打印：帧计数、首帧延迟、分辨率/chroma、10 秒内平均帧率与 CPU。
- **产出**：实验记录（命令、日志、数据）写入报告附录；确认 soname 加载策略（`libvlc.so.5` vs `.so.12`）。
- **判定门槛**：能稳定收到 RV32 帧且不崩溃；1080p30 播放 10s CPU 单核占用 < 60%（超出则 M4 提前到 M2 评估）。

### E3：流代理 PoC

- **目的**：验证「本地 HTTP 代理 + Range 透传 + 注入 Cookie/UA/Referer + 过期换链」可行。
- **方法**：临时 bin 起 `127.0.0.1:0`，用 `curl` 对三盘直链分别请求全量/Range/中断重连；再让 VLC 直接播代理 URL。
- **判定门槛**：代理下 VLC 能正常播放 + 拖动；换链后 Range 续传不崩。

### E4：性能基线

- 1080p H.264、4K HEVC 各 30s：RV32 vs I420（若 E2 支持 setup 协商）的 CPU/内存/帧率对比。
- 产出：性能表格，作为 M2/M4 决策依据。

---

## 3. 任务分解

> 写作用域为建议值，实际开工时以「一个任务一个 crate/文件集」为原则，避免并行写冲突。

### M1（MVP）

| ID | 任务 | 交付物 | 依赖 | 估算 | 写作用域 | 验收标准 |
|---|---|---|---|---|---|---|
| T1 | `wp-player` crate 骨架 + `PlayerEngine` trait | trait：`open(url/path, headers)`、`play/pause/seek/stop`、`set_volume/set_rate`、`tracks()`、`state()`、事件 channel；`PlayerSession` 管理 | — | 0.5 人日 | `crates/wp-player/**`、workspace Cargo | `cargo test` 有 mock engine 单测；trait 文档 |
| T2 | `wp-player-vlc`：libloading 动态加载 + 薄 FFI | `libvlc` 符号表（instance/media/player/event/video callbacks/audio volume/track）、加载失败可读错误、版本探测 | T1 | 1 人日 | `crates/wp-player-vlc/**` | 无 VLC 的机器返回可读错误而不 panic；有 VLC 时可创建 instance/player |
| T3 | vmem 帧通道（RV32 起步） | lock/unlock 回调 → 环形缓冲（2–3 帧）→ UI 线程 `TextureHandle::set` + `request_repaint` 跟随帧率；首帧/尺寸变化处理 | T2 | 1 人日 | `wp-player-vlc` + `wp-gui/src/pages/player.rs` | 本地 mp4 画面连续；窗口缩放不崩；暂停后不再忙刷新 |
| T4 | 流代理 `StreamProxy` | 本地 HTTP 服务：GET/HEAD、Range/If-Range 透传、headers 注入、token 校验、403/410/过期 → `direct_link` 换链并续 Range（≤3 次） | — | 1 人日 | `crates/wp-net/src/stream_proxy.rs`（或 `wp-player/src/proxy/`） | 单测：Range 解析/透传/换链重试（本地 mock server）；真机三盘各播 1 个视频 |
| T5 | Player 页 UI（最小版） | 视频区（等比适配/黑边/旋转元数据暂不做）、播放/暂停、进度条+时间、音量、全屏切换、加载/错误态 | T3 | 1 人日 | `wp-gui/src/pages/player.rs`、`app.rs`（Page 枚举/事件）、`pages/mod.rs` | 截图回归新增 2 项（空/播放中）；键鼠操作顺畅 |
| T6 | 入口接线 | 浏览器双击视频文件 → 播放页（`Action::Open` 分流）；右键菜单「▶ 播放」；`DirectLink → StreamProxy URL`；播放器事件回传 UI | T4,T5 | 0.5 人日 | `wp-gui/src/pages/browser.rs`、`app.rs` | 双击网盘视频直接开播；非媒体文件仍进详情 |
| T7 | CLI 探针命令（便于真机/CI 验收） | `wp-cli probe-range <路径>`（HEAD/首段/中段/过期信息）、`wp-cli media-info <路径>`（容器/时长/轨道，可先用 VLC 解析） | T4 | 0.5 人日 | `crates/wp-cli/src/main.rs` | 三盘真机输出 JSON；无播放器环境也可跑 Range 探针 |

### M2（体验完善）

| ID | 任务 | 交付物 | 依赖 | 估算 | 验收标准 |
|---|---|---|---|---|---|
| T8 | 音轨/字幕轨/倍速 | track 列表与切换；倍速 0.5×–2×；字幕轨选择 UI（vmem 限制下先保留接口+提示，等 M4） | T5 | 1 人日 | 多音轨 mkv 可切；倍速变化音画同步 |
| T9 | 播放列表 / 目录顺序播放 | 当前目录视频列表、上一首/下一首、循环模式；列表项用已有缩略图缓存 | T5 | 1 人日 | 目录内连续播放；缩略图复用正确 |
| T10 | 断点续播 + 设置持久化 | 播放进度（按 drive+path 记忆）、最近播放、音量/倍速写入数据目录 JSON | T5 | 0.5 人日 | 重启后恢复进度；导出/迁移不污染现有配置 |
| T11 | 本地文件入口 | 播放页「打开文件」（rfd）；拖放文件到窗口播放；最近本地文件 | T6 | 0.5 人日 | 本地 mp4/mkv/mp3 直接播；拖放不误触其它页面 |
| T12 | 错误恢复打磨 | 网络中断（代理重连）、链接过期（换链+seek 回原位）、解码失败（提示外部播放器） | T4,T5 | 0.5 人日 | 人为断网/过期后恢复；错误文案可读 |

### M3（稳定性与发行）

| ID | 任务 | 交付物 | 依赖 | 估算 | 验收标准 |
|---|---|---|---|---|---|
| T13 | 播放矩阵回归 | 自动化/半自动矩阵：容器(mp4/mkv/flv/ts/mp3/flac) × 编码(H.264/HEVC/AV1?) × 源(本地/三盘) × OS(Linux/Windows) | M2 | 1 人日 | 矩阵结果入验收报告；失败项有明确原因 |
| T14 | 性能与内存基线 | 1080p/4K 指标、长时间播放（1h）内存稳定性；写报告 | T13 | 0.5 人日 | 无内存增长泄漏；4K 结论明确 |
| T15 | 打包与发行 | Linux：绿色版带 `libvlc` 依赖说明/PKGBUILD 片段；Windows：`libvlc/` 子目录方案或「提示安装」，许可文件与声明；`build-*.sh/ps1` 更新；desktop `MimeType=video/*;audio/*` | T13 | 1 人日 | 绿色版在「有/无 VLC」两种机器行为正确；许可声明齐全 |
| T16 | 文档与验收 | README「播放器」章节、`docs/播放器说明.md`（限制：VLC3 字幕/4K/依赖）、《验收报告》更新、`待验证清单` 勾选 | M2 | 0.5 人日 | 文档可复现；限制写清楚 |
| T17 | 回归与提交 | `cargo fmt/check/test --workspace` 全绿、smoke 截图全过、真机播放证据（截图/日志）入库 | 全部 | 0.5 人日 | 无警告；CI 绿；提交信息完整 |

### M4（可选，先只做评估）

| ID | 任务 | 产出 | 估算 |
|---|---|---|---|
| T18 | VLC4 GPU 路径预研 | 等 VLC 4 正式版；评估 output callbacks（OpenGL engine）与本项目 glow 上下文集成、字幕能力、发行要求；产出对比结论 | 1–2 人日 |
| T19 | libmpv(LGPL) 后端预研 | 基于 `libmpv2 render` + `egui-sharkplayer` 先例做最小 PoC；评估 LGPL 构建/发行、Windows 打包、与 VLC 后端共存策略 | 1–3 人日 |
| T20 | 决策 | 二选一或维持 VLC3；若上，另立实现计划 | 0.5 人日 |

---

## 4. 测试与验收策略

- **单元测试**：trait mock、FFI 封装的安全边界（无 VLC 环境不崩）、代理 Range 解析/换链状态机、进度持久化格式。
- **集成测试**：本地媒体矩阵；三盘真机播放（每盘 ≥1 视频 + 1 音频）；`wp-cli probe-range` 输出作为证据。
- **GUI 回归**：`scripts/smoke-headless.sh` 增加播放页空态/播放中/全屏 3 张截图（帧纹理需可截图验证）。
- **手工验收**：长时间播放、窗口缩放/最小化、全屏切换、暂停恢复、拖动、断网/过期恢复。
- **性能门槛**：1080p30 CPU 单核 ≤60%；4K30 若 >100% 单核则记录并明确「推荐外部播放器」提示。

---

## 5. 风险登记（执行层）

| ID | 风险 | 触发 | 缓解 | 兜底 |
|---|---|---|---|---|
| R1 | 部分盘不支持 Range | E1 实测 | 该盘降级顺序播放/近似 seek | 临时文件下载后播 |
| R2 | VLC3 vmem 无字幕 | 既定 | UI 提示「内嵌模式无字幕」；提供外部播放器按钮 | M4 上 VLC4/libmpv |
| R3 | Windows 无 VLC 环境 | 用户机器 | 首选外部播放器 + 安装引导弹窗 | 捆绑（法务确认后） |
| R4 | 4K 性能不足 | E4/T14 | 默认提示用外部播放器；M4 加速路径 | 窗口嵌入（X11/Win） |
| R5 | 代理与并发/限速冲突 | 集成期 | 代理独立、播放不限速；端口/令牌隔离 | 禁用代理直连（仅光鸭） |
| R6 | FFI 崩溃（VLC 内部错误导致进程退出） | 联调 | 回调最小化、跨 FFI 不 panic、错误隔离；文档说明 | 外部播放器模式 |
| R7 | 许可证/合规问题 | 打包期 | 只动态链接；附 LGPL 声明；插件按 GPL 义务处理 | 不捆绑，仅依赖系统安装 |

---

## 6. 提交与分支策略（建议）

- 每个任务一个原子提交（如 `feat(player): ...`），构建期保持 `cargo check --workspace` 零警告。
- M0 实验代码放 `examples/` 或临时分支，不合入主分支（除非作为测试工具保留）。
- 新增 crate 后同步更新：workspace `members`、`README` 工程结构、`docs/ARCHITECTURE.md`、`.github/workflows/build.yml`（Linux/Windows 均需构建）。
- 里程碑结束跑一次 `cargo test --workspace` + `scripts/smoke-headless.sh`，并把真机证据写入 `docs/验收报告.md`。

---

## 7. 待用户确认清单（开工前回复即可）

1. **运行时策略**：Windows 走「提示安装 VLC/外部播放器兜底」，还是「捆绑 libvlc（约 +40–60MB，需 GPL 插件合规处理）」？
2. **字幕**：v1 内嵌无字幕可接受，还是要求「字幕优先」而改用外部播放器/等 M4？
3. **v1 范围**：最小可用（播放/暂停/进度/音量/全屏）即可，还是要一次带上「播放列表/断点续播/倍速/音轨」？
4. **平台优先级**：Linux 优先、Windows 跟随；还是两端同步验收？
5. 确认 M0 的 4 个实验（半天到一天）先跑，结果出来再决定是否执行 M1。

---

> 本文档与 [PLAYER_RESEARCH.md](PLAYER_RESEARCH.md) 是**只读调研产物**：项目源码、构建脚本、依赖均未改动；
> 所有「新增 crate/页面/命令」均为计划内容，等待确认后实施。
