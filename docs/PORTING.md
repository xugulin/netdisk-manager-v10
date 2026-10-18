# Python → Rust 移植对照表

> 重写原则：**只读学习**四个 Python 项目 → 抽取协议事实 → Rust 逐层实现。
> 源项目未做任何修改；协议细节可回溯到 `docs/research/*.md` 里的 `文件:行号` 引用。

## 1. 学习来源与产出

| 源项目 | 规模 | 研究产出 | 用途 |
|---|---|---|---|
| `百度网盘适配器/` | ~7.5k 行 PySide6 | [research/百度_协议.md](research/百度_协议.md)（622 行） | 百度 PCS 协议、扫码登录、上传/下载/分享/回收站 |
| `夸克网盘适配器/` | ~7k 行 PySide6 | [research/夸克_协议.md](research/夸克_协议.md)（891 行） | 夸克 clouddrive 协议、fid 路径解析、分片上传 |
| `光鸭云盘适配器/` | ~7k 行 PySide6 | [research/光鸭_协议.md](research/光鸭_协议.md)（995 行） | 光鸭协议、设备身份/埋点签名、OSS V1 签名分片上传 |
| `网盘管理_V2/` | v8_3 功能层 + wangpan 播放内核 | [research/网盘管理_V2_应用规格.md](research/网盘管理_V2_应用规格.md)（1192 行） | 应用层功能、桥协议、传输引擎、敏感词、主题 |

## 2. 模块级对照

| Python（V2 / 适配器） | Rust（V10） | 说明 |
|---|---|---|
| `v8_3/核心/模型.py` | `wp-core/src/model.rs` | 数据模型；dict+中文键 → serde 强类型 |
| `v8_3/配置.py` | `wp-core/src/config.rs` | 配置读写；中文键 → 英文键，保留默认值语义 |
| `v8_3/日志.py` | `wp-core/src/logging.rs` | 环形缓冲 + 文件落盘 + GUI 增量拉取 |
| `v8_3/敏感词/敏感词数据库.py` | `wp-core/src/guard.rs` | **简化**：SQLite 四表 → JSON 词库；AC 匹配语义保留 |
| `v8_3/敏感词/命名规避.py` | `wp-core/src/guard.rs::avoid_illegal_chars` | 百度非法字符全角化/emoji 移除/空名兜底，逐条移植 |
| `v8_3/敏感词/上传守卫.py` | `wp-transfer` 的 `UploadGuardHook` | V2 的探测绕过（P1–P4）不移植；保留预检 + 三档策略 |
| `v8_3/核心/适配器.py`（适配器规格） | `wp-core::config::DriveConfig` + `wp-drive::DriveInit` | 多实例模型保留，但不再需要「适配器项目目录/Python 解释器」 |
| `v8_3/核心/子进程客户端.py` + `v8_3/桥/*` | **删除** | 进程内 trait 对象替代 |
| `百度网盘适配器/核心/网络/网络客户端.py` | `wp-drive-baidu/src/session.rs` + `wp-net` | HTTP 封装/UA/Cookie/bdstoken |
| `百度网盘适配器/核心/认证/*` | `wp-drive-baidu/src/auth.rs` | 二维码登录/会话校验/登出 |
| `百度网盘适配器/核心/接口/文件接口.py` | `wp-drive-baidu/src/files.rs` | list/stat/mkdir/rename/move/copy/delete |
| `百度网盘适配器/核心/上传/*` | `wp-drive-baidu/src/upload.rs` | 秒传 + 分片上传（superfile2） |
| `百度网盘适配器/核心/下载/下载服务.py` | `wp-drive-baidu/src/download.rs` + `wp-net::download` | dlink + 必须 UA + 断点续传 |
| `夸克网盘适配器/核心/接口/路径解析.py` | `wp-drive-quark/src/files.rs` | fid ↔ 路径解析缓存（10s TTL 语义） |
| `夸克网盘适配器/核心/认证/*` | `wp-drive-quark/src/auth.rs` | 本地生成二维码 + Cookie 导入 |
| `夸克网盘适配器/核心/上传/*` | `wp-drive-quark/src/upload.rs` | MD5 秒传 + 分片 PUT |
| `光鸭云盘适配器/核心/网络/设备身份.py` | `wp-drive-guangya/src/device.rs` | 设备身份生成与持久化 |
| `光鸭云盘适配器/核心/接口/埋点签名.py` | `wp-drive-guangya/src/sign.rs` | SHA1(data+t+appid+密钥) 签名 |
| `光鸭云盘适配器/核心/上传/*`（oss2） | `wp-drive-guangya/src/upload.rs` | **oss2 SDK → 手写 OSS V1 签名 + HTTP 分片** |
| `v8_3/核心/传输引擎.py`（1483 行） | `wp-transfer/` | 队列/并发/重试/暂停/跨盘/持久化 |
| `v8_3/核心/任务数据库.py`（SQLite） | `wp-transfer` 任务 JSON | 简化持久化；重启恢复语义保留 |
| `v8_3/界面/主窗口.py` + 各页面 | `wp-gui/app.rs` + `pages/*` | 顶栏网盘 + 左栏功能页信息架构保留 |
| `v8_3/界面/主题管理器.py`（10 套） | `wp-gui/theme.rs` | 10 套配色原样映射到 egui |
| `v8_3/界面/登录对话框` / `桥/后端_*.py` 登录逻辑 | `wp-gui/dialogs.rs` + 各适配器 `auth.rs` | 扫码轮询、Cookie、短信、令牌 |
| `wangpan/player/*`（自研播放内核） | ❌ 不移植 | 需 libav* 绑定 + 解码/音频输出/渲染，独立大工程 |
| `v8_3/AI/*`、`wangpan/scrape/*`、`wangpan/danmaku/*` | ❌ 不移植 | 与「网盘管理」无关；V2 里也是可选功能 |
| `v8_3/更新.py` / `启动自检.py` / `进程.py` | ❌ 不移植 | 子进程桥/更新器不再需要；自检以 `--demo`/CI 冒烟替代 |

## 3. 有意差异（改进而非照搬）

| 主题 | V2 | V10 | 理由 |
|---|---|---|---|
| 适配器隔离 | 每适配器一个 Python 子进程 + 行式 JSON 桥 | 进程内 `Arc<dyn Drive>` | Rust 无包名冲突；少一层进程/序列化，错误类型安全 |
| 任务数据库 | SQLite + 批次表 | JSON 原子写 + 内存快照 | 任务量级小（千百条），无 SQL 依赖，绿色版更简单 |
| 敏感词学习 | AC + 探测历史 + 敏感词树 + 置信度 | 词库 + 命中记录 | 学习/探测绕过是「锦上添花」，先保证可靠上传 |
| 下载 | 适配器各自实现 + 分段/兜底 | 统一 `wp-net` 下载器（Range/续传/多段/限速） | 三家共用一份经过单元测试的下载实现 |
| 播放/AI | 内置 | 不移植 | 范围控制；架构上以后可加独立 crate 而不影响管理核心 |
| 断点续传 | 仅下载 | 下载（上传侧同 V2：协议需分片会话，暂不做） | 与 V2 行为对齐 |
| 界面 | PySide6 + 10 主题 | egui + 10 主题 + 快捷键 + 前后退 | 纯 Rust 跨平台；截图可自动化回归 |

## 4. 协议移植纪律

1. **先事实后实现**：每个适配器先有研究规格（URL/参数/header/响应字段/错误码/签名算法/实测坑），
   实现时逐条对照，规格与源码冲突以源码为准；
2. **签名/编码必须有单测**：MD5/SHA1/HMAC/Base64/OSS 签名/埋点签名/URL 编码用已知向量验证，
   不依赖真实账号；
3. **错误分类**：适配器把各家 errno 翻译成 `wp_core::Error::{Auth,Network,Server,NotFound,Unsupported,Canceled}`，
   上层只按分类处理（重试/重新登录/提示），不再像 V2 那样匹配中文错误文案；
4. **可插拔**：新增网盘只需实现 `Drive` trait + 在 `wp-gui` 注册 `create()`，其余层不动。

## 5. 没把握的地方（诚实清单）

* 三家网盘线上行为可能已变化（风控、接口升级）；适配器按 2026-09 源码协议实现，
  **真实账号回归前不能宣称 100% 可用**；
* 光鸭的 OSS 上传按 `oss2` 调用行为逆向为 HTTP 规格，签名算法有单测，但未对真实 bucket 验证；
* 百度的部分接口在 V2 源码里已标注失效（如 `/api/gettemplatevariable`），
  本项目使用 V2 实际可用的接口路径。
* 详见 [待验证清单.md](待验证清单.md)。
