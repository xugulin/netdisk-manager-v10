# API 覆盖矩阵（地毯式扫描结果）

> 扫描日期：2026-10-18。目的：把三家网盘**能用的接口全部摊开**，
> 对照本项目的 Rust 统一接口（`wp-drive::Drive`）标出「已实现 / 本次新增 / 未实现及原因」。
>
> 扫描方式：
> 1. 逐文件读三家 Python 适配器的 `核心/接口`、`核心/认证`、`核心/上传`、`核心/下载`、`核心/网络`；
> 2. 提取所有 URL 端点 + 公开函数签名；
> 3. 解析 `百度网盘适配器/_har/百度网盘_完整操作.har`：**95 个百度域名请求路径**（含网页版后台接口）；
> 4. 对照 V2 `v8_3/桥/后端_*.py` 的补丁与真机注释；
> 5. 逐项对照 Rust 适配器已实现的 trait 方法，得出差距。
>
> 图例：✅ 已实现并真机验证 / 🟡 已实现未真机验证 / 🔷 本次新增 / ❌ 不实现（附原因）

## 0. 统一能力矩阵（Rust `Drive` trait）

| 能力 | 百度 | 夸克 | 光鸭 | 本机/演示 |
|---|---|---|---|---|
| 扫码登录 / Cookie / 短信 / 令牌 | ✅扫码 ✅Cookie / — / — | ✅扫码 ✅Cookie / — / — | ✅扫码 ✅短信 ✅令牌 | — |
| 账号信息 / 容量 | ✅（Cookie 会话） | ✅ | ✅ | ✅ / ✅ |
| 列目录 / stat / 搜索 | ✅ / ✅ / 🟡 | ✅ / ✅ / ✅ | ✅ / ✅ / ❌无搜索接口 | ✅ |
| 新建 / 重命名 / 删除 / 移动 / 复制 | ✅ / ✅ / ✅ / ✅ / ✅ | ✅ / ✅ / ✅ / ✅ / ❌无复制接口 | ✅ / ❌无接口 / ✅ / ❌无接口 / ❌无接口 | ✅ |
| 上传（秒传/分片） | ✅（4MiB 串行） | ✅（4MiB 并发） | ✅（单 PUT / OSS multipart） | ✅ / ✅ |
| 下载（直链/续传/分段） | ✅ | ✅ | ✅ | ✅ |
| 分享：创建 / 列表 / 取消 | ✅ / ✅ / ✅ | ✅ / ✅ / ✅ | ✅ / ✅ / ✅ | ✅ / ✅ / ✅ |
| **回收站：列表 / 恢复 / 彻底删除 / 清空** | 🔷 / 🔷 / ❌ / ❌ | 🔷 / 🔷 / 🔷 / 🔷 | 🔷 / ❌ / ❌ / 🔷 | — |
| **云添加（离线下载/BT）** | 🔷 只读任务列表（创建参数未抓到，不猜） | ❌（无此功能） | 🔷 直链+magnet / 任务列表 | — |
| **上传记录 / 操作记录** | ❌（Python 亦未实现） | ❌（无接口） | 🔷 | — |
| **分享校验 / 转存** | 🔷 / 🔷 | 🔷 / 🔷 | ❌（无转存接口） | — |
| **分类浏览（全盘视频/图片/文档…）** | 🔷 | ❌ | ❌ | — |
| 直链（播放/外链） | ✅ | ✅ | ✅ | ✅ |
| 写权限探测 | ✅ | ❌ | ❌ | — |

## 1. 百度网盘

### 1.1 认证与账号

| 能力 | 端点 | 来源 | 状态 |
|---|---|---|---|
| 二维码申请/轮询 | `GET passport /v2/api/getqrcode`、`/channel/unicast` | 登录服务.py | ✅ |
| 扫码换 BDUSS | `GET passport /v3/login/main/qrbdusslogin` | 登录服务.py | ✅ |
| pan 域会话/子域种植 | `GET passport /v3/login/api/auth[/]` | 登录服务.py | ✅（只读会话限制见 docs/待验证清单.md） |
| 会话自检/引导 | `GET /api/loginStatus`、`/api/gettemplatevariable` | 认证服务.py | ✅ 双通道回退 |
| 用户信息 | `GET /api/user/getinfo` | 认证服务.py | ✅ |
| 容量 | `GET /api/quota` | 认证服务.py | ✅（free 可能是 0，用 total-used 兜底） |
| 会员信息（HAR 发现，未接） | `GET /rest/2.0/membership/user[/info]` | HAR | ❌（`getinfo` 已含 vip 字段，够用） |
| 设备开关/风控问卷（HAR） | `/api/account/getdeviceswitch`、`/api/certuser/get` | HAR | ❌ 非核心 |

### 1.2 文件与目录

| 能力 | 端点 | 来源 | 状态 |
|---|---|---|---|
| 列目录 | `GET /api/list` | 文件接口.py | ✅ |
| 元信息 | `GET/POST /api/filemetas` | 文件接口.py | ✅（真机 errno:12=不存在，已映射） |
| 新建目录 | `POST /api/create?isdir=1` | 管理接口.py | ✅ |
| 删除/重命名/移动/复制 | `POST /api/filemanager?opera=delete/rename/move/copy` | 管理接口.py | ✅ |
| 分类列表（全盘） | `GET /api/categorylist` | 文件接口.py | ✅ 真机 5000 条（深翻页 31034 已优雅降级） |
| 缩略图 | 条目 `thumbs` 字段（list/categorylist） | 文件接口.py | ✅ 真机 60x44 JPEG；filemetas 无该字段，父目录列表兜底 |
| 上传记录/用户操作 | （Python 为 NotImplemented 占位） | 文件接口.py | ❌ 源码无实现 |
| 目录树/批量下载（HAR 有，未接） | `/api/getsyscfg` 等为网页后台 | HAR | ❌ 非必需 |

### 1.3 分享

| 能力 | 端点 | 来源 | 状态 |
|---|---|---|---|
| 创建分享 | `POST /share/pset` | 分享接口.py | ✅ |
| 分享列表/取消 | `GET /share/record`、`/share/cancel` | 分享接口.py | ✅ |
| 校验分享链接/提取码 | `GET /share/verify`（+ `/share/surlinfoinrecord` 提取码查询） | 分享接口.py | ✅ 真机校验通过 |
| 转存 | `POST /share/transfer` | 分享接口.py | 🟡 协议已实现；同账号转自己分享被服务端拒（errno:2），需他人分享正向验证 |
| 分享任务查询 | `/share/taskquery` | 分享接口.py | ✅（内部等待用） |

### 1.4 回收站

| 能力 | 端点 | 来源 | 状态 |
|---|---|---|---|
| 列表 | `GET /api/recycle/list/` | 回收站接口.py | ✅ 真机 130 项 |
| 恢复 | `POST /api/recycle/restore` | 回收站接口.py | ✅ 自建目录闭环 |
| 彻底删除 | —（源码只有说明：会触发 132 短信风控，无公开接口） | 回收站接口.py | ❌ |
| 清空 | — | — | ❌ |

### 1.5 云添加 / 离线下载

| 能力 | 端点 | 来源 | 状态 |
|---|---|---|---|
| 任务列表（只读） | `GET /rest/2.0/services/cloud_dl?method=list_task` | HAR（响应样本完整） | ✅ 真机只读列表（任务状态 0=完成已单独映射） |
| 任务查询（只读） | `GET .../?method=query_task` | HAR | 🔷 状态语义与通用表相反（百度 0=完成），已单独映射 |
| 创建离线任务 | `POST .../?method=add_task`（form: source_url+save_path；magnet 加 od_type=4） | 真机探测 | 🔷 参数形态已通过服务端校验（账号超容 36009）；错误码 36005~36032 全中文映射；error_code 可能裹在 HTTP 403 JSON 里 |

### 1.6 下载（直链 / 续传 / 分段）

| 能力 | 端点 | 来源 | 状态 |
|---|---|---|---|
| 取下载直链 | `GET /api/filemetas?target=[{"path":...}]&dlink=1` | 文件接口.py | ✅ 服务端签好 dlink（含 fid/sign/expires，8h 有效），302 → `*.baidupcs.com` |
| 直链下载 | `GET https://d.pcs.baidu.com/file/...`（带 Cookie；支持 Range） | 真机 | ✅ 但**大文件有三条 CDN 限制**（2026-10-19 真机，非会员账号）：① 请求 UA 必须是客户端标识（`netdisk;...`），浏览器 UA → 403 `31326`；② 必须带**有界 Range**（无 Range / `bytes=0-` 开区间均 403 `31326 hitcode:104`）；③ 单次 Range 跨度 **>约 5MB 即 403**。实现：`download_headers()` 用 `DOWNLOAD_UA`；`direct_link()` 手动解析 302（reqwest 跨域跟随会丢 Cookie）；下载器 `max_range_span=4MiB` 按块请求（单连接/多段内均生效） |
| 目录打包下载 | — | — | ❌（`/api/getsyscfg` 等网页后台，未接；跨盘整目录复制也未实现） |

> **真机结果（2026-10-19 晚）**：修复后完整下载两个大文件并上传光鸭云盘 `/IDA pro9.5 加补丁/`：
> * `/IDA95/linux/ida-pro_95_x64linux.run` 722,672,855 B（5,986 s，平均 0.12 MB/s）→
>   `file` 为 ELF 64-bit x86-64；sha256 `3c4337c9…` / md5 `154690688b87753fd768184791458129`；
> * `/IDA95/win/ida-pro_95_x64win.exe` 752,039,466 B（6,224 s，平均 0.12 MB/s）→
>   `file` 为 PE32+ executable (GUI) x86-64；sha256 `5996d7b6…` / md5 `6365ad4216399781cc5c66d8315d193e`；
>
> 两个文件在光鸭云盘的大小/md5 均与本地一致（上传器上报 md5 相同），证明上传完整。

> 注：`31326` 不是「链接失效/无权限」的永久错误——根因是 CDN 按 UA 做授权判定；
> 排查时先用 `curl -r 0-255 -H "User-Agent: netdisk;P2SP;2.2.2.0" -H "Cookie: <登录Cookie>" <dlink>` 复测。

## 2. 夸克网盘

### 2.1 认证与账号

| 能力 | 端点 | 来源 | 状态 |
|---|---|---|---|
| 扫码（本地生成二维码） | ticket 流程：`/1/clouddrive/...` + `account/info` | 登录服务.py | ✅ |
| Cookie 登录/校验 | `GET /1/clouddrive/member` | 认证服务.py | ✅ |
| 账号/容量/会员 | `GET /1/clouddrive/member` | 认证服务.py | ✅ |

### 2.2 文件与目录

| 能力 | 端点 | 来源 | 状态 |
|---|---|---|---|
| 列目录 | `GET file/sort`、`file/list` | 文件接口.py | ✅（10s TTL 缓存） |
| 搜索 | `GET file/search` | 文件接口.py | ✅ |
| 文件信息 | `POST file/info` | 文件接口.py | ✅ |
| 新建目录 | `POST file/create`（+ 确保目录） | 文件接口.py | ✅ |
| 删除 | `POST file/delete`（task 等待） | 文件接口.py | ✅ |
| 重命名 / 移动 | `POST file/rename` / `file/move` | 文件接口.py | ✅ |
| 复制 | —（夸克无复制接口；分享转存可替代） | — | ❌ |
| 文件夹树 | `GET file/tree` | 文件接口.py | ❌（目录选择器用逐层列表已够） |
| 缩略图 | 条目 `thumbnail` 字段 | 文件接口.py | ✅ 真机 WebP 133x100（image 需开 webp feature） |
| 批量下载地址 | `POST file/download`（列表形态） | 文件接口.py | 🟡（单文件直链已够传输引擎用） |

### 2.3 分享

| 能力 | 端点 | 来源 | 状态 |
|---|---|---|---|
| 创建分享（+提取码） | `POST share` + `share/password` + 任务等待 | 分享接口.py | ✅ |
| 分享列表 / 取消 | `GET share/mypage/detail`、`POST share/delete` | 分享接口.py | ✅ |
| 校验分享/取内容 | `POST share/sharepage/token`、`GET share/sharepage/detail` | 分享接口.py | ✅ 真机校验通过（标题/文件/大小） |
| 转存分享 | `POST share/sharepage/save` | 分享接口.py | 🟡 协议已实现；自己分享自己返回 41017（已给中文提示） |

### 2.4 回收站

| 能力 | 端点 | 来源 | 状态 |
|---|---|---|---|
| 列表 | `GET file/recycle/list` | 文件接口.py | ✅ 真机 |
| 恢复 | `POST file/recycle/recover` | 文件接口.py | ✅ 自建目录闭环 |
| 彻底删除 | `POST file/recycle/remove` | 文件接口.py | ✅ 真机清理 3 项 |
| 清空 | 由「列表全部 + 彻底删除」组合 | 组合实现 | ✅ 组合已真机验证 |

### 2.5 其它

| 能力 | 端点 | 来源 | 状态 |
|---|---|---|---|
| 任务查询 | `GET task`（taskId 轮询） | 任务接口.py | ✅（内部等待用） |
| 云添加 / 离线下载 | — | — | ❌（夸克无此接口） |
| 上传记录 | — | — | ❌ |

## 3. 光鸭云盘

### 3.1 认证与账号

| 能力 | 端点 | 来源 | 状态 |
|---|---|---|---|
| 设备码扫码 | `POST /v1/auth/device/code`、`/v1/auth/token` | 认证服务.py | ✅ |
| 短信登录 | `/v1/shield/captcha/init` → `/v1/auth/verification[/verify]` → `/v1/auth/signin` | 登录服务.py | ✅（真机通过） |
| 令牌登录/刷新 | `POST /v1/auth/token` | 认证服务.py | ✅ |
| 用户信息 | `GET /v1/user/me` | 认证服务.py | ✅ |
| 资产/容量/流量 | `POST /assets/v1/get_assets` | 资产接口.py | ✅（容量已接；流量统计未展示） |
| 权益/流量统计 | `POST /assets/v1/get_user_rights`、`/assets/v1/get_flow_stat`(?) | 资产接口.py | ❌ 非核心（HAR 未覆盖字段） |

### 3.2 文件与目录

| 能力 | 端点 | 来源 | 状态 |
|---|---|---|---|
| 列目录 | `POST /userres/v1/file/get_file_list` | 文件接口.py | ✅ |
| 新建目录 | `POST /userres/v1/file/create_dir` | 文件接口.py | ✅ |
| 删除（进回收站） | `POST /userres/v1/file/delete_file` | 文件接口.py | ✅ |
| 重命名 / 移动 / 复制 | —（源码与 HAR 均无） | — | ❌ |
| 搜索 | —（无接口） | — | ❌ |
| 缩略图 | 条目 `thumbnail` 字段 | 文件接口.py | ✅ 真机 JPEG 128x96（鉴权用 Bearer） |
| 上传记录 | `POST /userres/v1/get_user_action` | 文件接口.py | ✅ 真机 |
| 上传任务统计 | `POST /userres/v1/query_uploading_tasks_stat` | 文件接口.py | ❌ 非核心 |

### 3.3 分享

| 能力 | 端点 | 来源 | 状态 |
|---|---|---|---|
| 创建分享 | `POST /userres/v1/share_file`（+ 模板 `/misc/v1/get_share_template`） | 分享接口.py | ✅ |
| 分享列表 / 取消 | `POST /userres/v1/get_share_list`、`/delete_share` | 分享接口.py | ✅ |
| 校验/转存 | —（无接口） | — | ❌ |

### 3.4 回收站

| 能力 | 端点 | 来源 | 状态 |
|---|---|---|---|
| 列表 | `POST /userres/v1/file/get_file_list`（`dirType=4`） | 文件接口.py | ✅ 真机 |
| 清空 | `POST /userres/v1/file/clear_recycle_bin` | 文件接口.py | 🟡 未执行（会连用户历史一起清空） |
| 恢复 / 彻底删除 | —（无接口） | — | ❌ |

### 3.5 云添加（离线下载 / BT）

| 能力 | 端点 | 来源 | 状态 |
|---|---|---|---|
| 解析直链 | `POST /cloudcollection/v1/resolve_res` | 云添加接口.py | ✅（内部） |
| 解析种子 | `POST /cloudcollection/v1/resolve_torrent` | 云添加接口.py | 🟡（内部，GUI 未接种子文件选择） |
| 创建直链任务 | `POST /cloudcollection/v1/create_task`（`url`+`parentId`） | 云添加接口.py | ✅ 真机端到端（建任务→完成→落地） |
| 创建 BT 任务 | 同上（`url=magnet:?..`，`newName` 可选） | 云添加接口.py | 🟡 与直链同一入口，未提交真实 BT 任务 |
| 任务列表 | `POST /cloudcollection/v1/list_task` | 云添加接口.py | ✅ 真机 |
| 任务详情 / 等待完成 | `POST /cloudcollection/v1/list_task`（`taskIds`） | 云添加接口.py | ✅（内部） |
| 取消任务 | —（源码无取消接口） | — | ❌ |

## 4. 未实现项总表（含原因与后续）

| 未实现 | 归属 | 原因 | 后续 |
|---|---|---|---|
| 百度云添加「任务成功落地」 | 百度 | 创建参数已通过服务端校验，但该账号已超容（used 9.94TB > total 2.21TB），服务端返回 36009 拒绝创建 | 账号清理空间/续费后复测一次即可 |
| 百度回收到彻底删除/清空 | 百度 | 源码无公开接口；彻底删除会触发 132 短信风控 | 需要短信验证交互，价值低 |
| 百度上传记录 | 百度 | Python 源码亦为 `NotImplementedError` | 需抓包 |
| 夸克云添加/上传记录/复制 | 夸克 | 无对应接口 | — |
| 光鸭重命名/移动/复制/搜索/转存/回收站恢复 | 光鸭 | 源码与 HAR 均无接口 | 若官方新增接口再补 |
| 缩略图（已完成） | 三家 | 列表小图 + 详情大图 + 懒加载缓存；夸克 WebP 已解码 | — |
| 多文件批量直链下载（夸克） | 夸克 | 传输引擎按单文件调度已够用 | 可选优化 |

## 5. 扫描方法论（可复用）

1. **以「公开函数 → 端点」建表**：`grep -n "^    def [^_]" 核心/接口/*.py`，逐函数读实现拿 URL/参数/响应字段；
2. **HAR 补齐网页后台接口**：解析 HAR 统计 `(host, path)`，过滤沙箱/埋点/静态资源，剩下的就是 API 面；
3. **V2 注释当"真机事实"**：`v8_3/桥/后端_*.py` 里有大量 2026-09 的真机实测结论（errno 语义、可见性延迟、风控），优先级高于文档；
4. **对照 Rust 现状**：`grep -n "async fn" crates/wp-drive/src/lib.rs` × 各适配器实现，逐项勾选；
5. **不臆造**：源码/HAR 都没有的接口，宁可标 ❌ 也不猜参数——宁可少功能，不可假功能。
