# PLAYER_STREAM_M0 —— wp-stream 流代理实现与三盘 Range 真机验证

> 对应任务：**M0-E1**（Range 探测）+ **T12**（本地 HTTP + Range 透传 + 头注入）+ **T13**（磁盘预读缓存）+ **T14**（链接过期热换）
> 代码：`crates/wp-stream/**`、`crates/wp-cli/src/main.rs`（仅新增 `probe-range`）
> 日期：2026-10-19 · 数据目录：`出/真机联调数据`（百度 / 夸克 / 光鸭 真机会话）

---

## 0. 结论先行

| 项 | 结论 |
|---|---|
| 三盘单段 Range | **均支持**：首段 `bytes=0-1023`、中段 `bytes=size/2..+1023` 均返回 **206**，`Content-Range` 正确 |
| 本地流代理 | **可用**：`127.0.0.1:0` + `/s/<token>`，GET/HEAD、单段 Range、keep-alive、416/400 语义齐全 |
| 真机代理冒烟 | **三盘通过**：首段/中段 206 + **字节级与直链一致** + 重复请求命中磁盘缓存（不再请求上游）+ 0 错误 |
| 百度注意点 | 直链是 **302 → `*.baidupcs.com`** 两段式，必须跟随重定向；`direct_link` 给的 URL 本身不带 Range 签名，跟随后的 CDN URL 才真正服务 Range |
| 夸克注意点 | **HEAD 返回 403**（预签名直链只签 GET），但 **GET Range 正常 206** → 代理/探测以 Range 为准，不以 HEAD 为准；`>~100MB` 文件 `direct_link` 返回 `23018 download file size limit`，大视频改走 **HLS**（`DirectLink.is_hls=true`，本代理直连不代理） |
| 光鸭注意点 | 直链带 `expire_at`（约 6h），无 headers（签名全在 URL）；代理已实现 **到期前 45s 主动换链** |
| 百度不可下载文件 | 根目录 `/video_20260322_154432.mp4`（2.4GB）对该账号所有请求返回 `403 {"error_code":31326,"error_msg":"user is not authorized"}` —— 属**文件权限/归属**问题（非 Range 问题），探测改用同盘可下载的 >10MB 视频 |
| 测试 | `cargo test -p wp-stream`：**44 个测试全绿**（22 单元 + 21 端到端 mock + 1 doc-test），不联网 |

---

## 1. 复现命令

```bash
# 构建 CLI
cargo build -p wp-cli

# 三盘 Range 探测（只读：HEAD + 两次 ≤1KB 的 Range GET）
cargo run -p wp-cli -- --data-dir 出/真机联调数据 --drive baidu   probe-range "/来自：V2408A/DCIM/1074058976_1734538851561.mp4"
cargo run -p wp-cli -- --data-dir 出/真机联调数据 --drive quark   probe-range "/来自：分享/IDM.zip"
cargo run -p wp-cli -- --data-dir 出/真机联调数据 --drive guangya probe-range "/来自：分享/东京复仇者/东京复仇者.S01E25.2021.1080p.mkv"

# 真机代理冒烟：先用 CLI 取直链，再让 wp-stream 起本地代理读首段/中段/重复读
cargo run -p wp-cli -- --data-dir 出/真机联调数据 --drive baidu link "/来自：V2408A/DCIM/1074058976_1734538851561.mp4" > /tmp/link-baidu.json
cargo run -p wp-stream --example proxy_smoke -- /tmp/link-baidu.json

# 单测（mock 上游，不联网）
cargo test -p wp-stream
```

---

## 2. 三盘 Range 探测结果（M0-E1）

### 2.1 汇总表

| 网盘 | 探测文件 | 大小(bytes) | HEAD | 首段 `0-1023` | 中段 `size/2` | Accept-Ranges | 结论 |
|---|---|---|---|---|---|---|---|
| 百度 | `/来自：V2408A/DCIM/1074058976_1734538851561.mp4` | 16,306,047 | **200**（CL=16306047） | **206** `bytes 0-1023/16306047` | **206** `bytes 8153023-8154046/16306047` | `bytes` | ✅ 支持 Range（302 后） |
| 夸克 | `/来自：分享/IDM.zip` | 12,555,024 | 403（预期，仅签 GET） | **206** `bytes 0-1023/12555024` | **206** `bytes 6277512-6278535/12555024` | `bytes` | ✅ 支持 Range（GET） |
| 光鸭 | `/来自：分享/东京复仇者/东京复仇者.S01E25.2021.1080p.mkv` | 727,730,541 | **200**（CL=727730541） | **206** `bytes 0-1023/727730541` | **206** `bytes 363865270-363866293/727730541` | `bytes` | ✅ 支持 Range |

### 2.2 探测输出摘要（原始 JSON 节选）

百度（`is_hls=false`；headers 注入 Cookie/UA/Referer/Accept/Accept-Language；`expire_at=null`）：

```json
{
  "drive": "baidu:baidu", "size": 16306047, "url_host": "d.pcs.baidu.com",
  "header_names": ["User-Agent", "Referer", "Accept", "Accept-Language", "Cookie"],
  "head":  {"status": 200, "content_length": 16306047, "accept_ranges": "bytes", "ok": true},
  "first": {"status": 206, "content_range": "bytes 0-1023/16306047", "ok": true},
  "mid":   {"status": 206, "content_range": "bytes 8153023-8154046/16306047", "ok": true},
  "range_supported": true, "upstream_ignores_range": false, "error": null
}
```

夸克（HEAD 403 属预签名 URL 正常行为；GET Range 正常）：

```json
{
  "drive": "quark:quark", "size": 12555024, "url_host": "dl-pc-zb.pds.quark.cn",
  "header_names": ["user-agent", "referer", "accept", "cookie"],
  "head":  {"status": 403, "content_type": "application/xml", "error": "HTTP 403"},
  "first": {"status": 206, "content_range": "bytes 0-1023/12555024", "accept_ranges": "bytes", "ok": true},
  "mid":   {"status": 206, "content_range": "bytes 6277512-6278535/12555024", "ok": true},
  "range_supported": true, "upstream_ignores_range": false, "error": null
}
```

光鸭（headers=[]，签名在 URL；`expire_at` 有值）：

```json
{
  "drive": "guangya:guangya", "size": 727730541, "url_host": "hwsh02-httpdown.guangyacdn.com",
  "expire_at": 1792367451, "header_names": [],
  "head":  {"status": 200, "content_length": 727730541, "accept_ranges": "bytes", "ok": true},
  "first": {"status": 206, "content_range": "bytes 0-1023/727730541", "ok": true},
  "mid":   {"status": 206, "content_range": "bytes 363865270-363866293/727730541", "ok": true},
  "range_supported": true, "upstream_ignores_range": false, "error": null
}
```

### 2.3 探测目标选择说明（与任务原计划的差异）

1. **百度**：任务给定的 `/video_20260322_154432.mp4`（2.4GB）在真机实测中，无论 HEAD 还是 GET Range（curl 同步复测亦然）都返回
   `403 {"error_code":31326,"error_msg":"user is not authorized, hitcode:119"}`。这是**该账号对文件无下载权限/文件归属问题**，
   不是 Range 能力问题（同盘其他文件正常）。故改用同盘已可下载且 >10MB 的视频
   `/来自：V2408A/DCIM/1074058976_1734538851561.mp4`（16.3MB，真实拍摄 mp4）。
2. **夸克**：根目录两个 `>170MB` 视频 `direct_link` 均返回 `23018 download file size limit`（实测 50.6MB 文件可、108MB 文件不可），
   无法用于 Range 探测；按 Lead 批复改用同盘 `>10MB` 的 `/来自：分享/IDM.zip`（12.5MB）探测 Range 能力。
   大文件播放链路已由「夸克流媒体专家」改走 HLS（`DirectLink.is_hls=true`），wp-stream 已实现对应直连契约（见 §4.6）。

---

## 3. 真机本地代理冒烟（T12/T13/T14 联合验证）

工具：`cargo run -p wp-stream --example proxy_smoke -- <wp-cli link 输出.json>`
流程：真实 `DirectLink` → `StreamProxy::open_with_provider` → 本地代理 GET `bytes=0-1023` → GET 中段 `size/2..+1023` →
再 GET 首段（验证磁盘缓存命中）→ 与**直连上游同一段**做字节级对照。

| 网盘 | 文件 | 大小 | 首段 | 中段 | 中段与直连字节一致 | 重复请求缓存命中 | errors | 端到端耗时 |
|---|---|---|---|---|---|---|---|---|
| 百度 | `1074058976_1734538851561.mp4` | 16.3 MB | 206 ✅ | 206 ✅ | ✅ true | ✅ true | 0 | ~6.5s |
| 夸克 | `IDM.zip` | 12.5 MB | 206 ✅ | 206 ✅ | ✅ true | ✅ true | 0 | ~2.3s |
| 光鸭 | `东京复仇者.S01E25...mkv` | 727.7 MB | 206 ✅ | 206 ✅ | ✅ true | ✅ true | 0 | ~1.3s |

样例输出（光鸭，节选）：

```json
{
  "total_bytes": 727730541,
  "first": {"status": 206, "content_range": "bytes 0-1023/727730541", "len": 1024},
  "mid":   {"status": 206, "content_range": "bytes 363865270-363866293/727730541", "len": 1024},
  "first_again": {"status": 206, "content_range": "bytes 0-1023/727730541", "len": 1024},
  "mid_body_matches_upstream": true,
  "cache_hit_on_repeat": true,
  "stats": {"downloaded_bytes": 524288, "upstream_restarts": 0, "errors": 0}
}
```

> `downloaded_bytes = 524288`（2×256KB 分块，首块+中块各一次）：证明重复请求**没有**再下载，缓存命中成立。

---

## 4. 实现清单

### 4.1 公共 API（`crates/wp-stream/src/lib.rs`）

```text
StreamProxy::new(StreamConfig) -> StreamProxy
StreamProxy::start() -> Result<u16>                          # 127.0.0.1:0，幂等
StreamProxy::open(Arc<dyn Drive>, remote_path) -> Result<StreamHandle>
StreamProxy::open_with_provider(Arc<dyn LinkProvider>, ...)  # 单测/自定义取链
StreamProxy::stats(token) -> Option<StreamStats>             # 缓冲/下载/码率/换链/错误/完整
StreamProxy::close(token); StreamProxy::shutdown()
probe_range(Arc<dyn Drive>, remote_path) -> Result<RangeProbe> # M0-E1 / CLI
probe_with_provider(...)                                      # 离线探测（测试用）
```

`StreamHandle { url, token, display, total_bytes, direct, headers }`：
* 普通文件：`url = http://127.0.0.1:<port>/s/<token>`（播放器/curl/VLC/mpv 直接打开）；
* **HLS**：`direct = true`，`url` 为原始 m3u8 URL，`headers` 为需注入的请求头（Cookie 等），`token` 为空，`stats(None)`。

### 4.2 本地 HTTP 语义（T12）

| 项 | 行为 |
|---|---|
| 监听 / 鉴权 | 仅 `127.0.0.1:0`（随机端口）；路径 `/s/<随机 token>`；未知 token/路径 → 404；非 `/s/` → 404 |
| 方法 | `GET` / `HEAD`；其它 → 405（`Allow` 语义） |
| Range | 单段三种形式：`bytes=a-b`、`bytes=a-`、`bytes=-N`；越界 → **416** + `Content-Range: bytes */total` |
| 多段 Range | **明确不支持 → 400**（响应体说明；mpv/ffmpeg/VLC 只用单段） |
| 成功响应 | 全量 `200`；Range `206` + `Content-Range`；恒带 `Accept-Ranges: bytes`、`Content-Length`、`Content-Type`（按扩展名） |
| 连接 | 支持 HTTP/1.1 keep-alive（同连接多个 Range 请求，seek 不重复建连） |
| 首块预检 | GET 在发响应头**之前**先确保首块可用：上游失败时干净返回 `502`（而不是发完头后断流） |
| 健康检查 | `GET /healthz` 返回 `{"ok":true,"sessions":N,"version":"..."}`（验收脚本用） |

### 4.3 上游访问与头注入

* `LinkProvider` 抽象直链来源；`DriveLinkProvider` 适配真实网盘（`Drive::direct_link`）。
* `DirectLink.headers` **原样注入**上游请求（百度需要 Cookie/UA/Referer；夸克需要 UA/Referer/Cookie；光鸭空）。
  自动跳过 `range/content-length/accept-encoding/host`（由代理控制），强制 `Accept-Encoding: identity`。
* 302 重定向由 reqwest 默认跟随（百度 `d.pcs.baidu.com → xaky-cm01.baidupcs.com` 实测通过）。
* 上游 206 正常；若忽略 Range 返回 200 → **整文件回退**（跳过 `start` 字节、截取所需段）并记录告警/
  `upstream_ignores_range`，保证播放器仍能读。
* 非 2xx 状态原样带回代理层分类，不在传输层乱抛错。

### 4.4 磁盘预读缓存（T13）

* 缓存 key：`hash(drive:path:size)` + 文件名提示；数据文件 `<key>.data`（稀疏文件）+ 元数据 `<key>.meta.json`。
* 分块默认 **4MB**（配置 `chunk_bytes`；超大文件自动放大分块保证块数 ≤65536）。
* 预读窗口默认 **64MB**（`readahead_bytes`，0=关闭），多连接并行预取（`parallel_fetches`，默认 2）。
* 未命中范围按需下载**并阻塞等待**；同块并发去重（`Mutex<ChunkState>` + `Notify`，只下载一次，其余等待）。
* 磁盘 LRU：默认上限 **20GB**（`max_cache_bytes`），只淘汰无使用者的旧缓存；仍被播放会话占用的不抢占。
* 已完成块写入 sidecar 元数据（原子 rename），**进程/会话重启后可复用**；数据文件大小不符会自动重建。
* 缓存目录可配置；未配置时落系统临时目录。真机冒烟/单测全部使用临时目录，不污染 `出/`。

### 4.5 链接过期热换（T14）

* 触发：上游 `401/403/410`、超时、网络错误。
* 动作：重新 `direct_link` 后**从当前块起始偏移续传**；每块最多换链 `max_link_restarts=3` 次；
  并发换链用「代次」去重（多个块同时 403 只换一条链）。
* 主动换链：`DirectLink.expire_at` 距今 **≤45 秒**时在下次取链快照时提前换（避免播放中途 403）。
* 统计：`StreamStats.upstream_restarts`；换链次数耗尽向播放器返回明确 `502` + 错误文案，`errors` 计数。

### 4.6 HLS 直连契约（夸克大文件）

`DirectLink.is_hls == true` 时**不走 Range 代理/缓存**，`StreamProxy::open` 直接返回：
`StreamHandle { direct: true, url: <原始 m3u8>, headers: <Cookie 等>, token: "", stats: None }`
—— 上层把 URL+headers 交给 mpv（`--http-header-fields`）直连即可（HLS 是播放列表+分片，重写不值当）。
`probe_range` 对 HLS 直接返回错误：`HLS 流不支持 Range 探测（is_hls=true，应把 m3u8 URL+headers 直接交给播放器）`。

### 4.7 探测（M0-E1）

`probe_range` 输出 JSON（CLI `probe-range <路径>`）：

* `drive/path/url_host/size/direct_link_ms/expire_at/header_names/is_hls`
* `head` / `first` / `mid` 三步：`status/content_range/accept_ranges/content_length/content_type/elapsed_ms/ok/error`
* `range_supported`（首段 206 即 true）、`upstream_ignores_range`、整体 `error`
* 只读：HEAD + 两次 ≤1KB 的 Range GET（**不读响应体**），不会下载大文件。

### 4.8 CLI 变更（最小侵入）

* `wp-cli probe-range <远端路径>`：调用 `wp_stream::probe_range`，输出 JSON（失败走 stderr + 非 0 退出）。
* `--help` 增加一行 `probe-range <远端路径> 探测直链 HEAD/Range 能力（M0-E1，输出 JSON）`。
* `crates/wp-cli/Cargo.toml` 仅新增 `wp-stream.workspace = true` 一行依赖；**未改动其它命令语义**。

---

## 5. 测试矩阵（`cargo test -p wp-stream` 全绿，共 44 个）

单元测试（22）：
* Range 解析/归一化 9：`a-b`/`a-`/`-N`、大小写/空格、多段拒绝、非法形式、越界收敛、suffix 收敛、空文件、Content-Range 格式；
* 缓存 6：key 稳定性、块区间、并发同块去重（4 并发只下载 1 次）、落盘+重开恢复（meta 持久化）、失败后可重试、LRU 淘汰无使用者旧缓存；
* 上游 3：Content-Range 解析、状态分类（403/503/206）、Range 归一化引用；
* 代理 3：Content-Type 映射、配置收敛（0→1）、keep-alive 判定。

端到端（21，本地 mock 上游 `tokio TcpListener` 实现 Range，**不联网**）：
`full_get_200` / `range_206_slice` / `open_ended+suffix` / `HEAD元数据` / `多段400` / `416` / `未知token+未知路径404` /
`缓存命中不再打上游` / `跨块Range拼接` / `头注入` / `后台预读填窗口` /
`403→换链续传` / `换链耗尽→502` / `expire_at过期主动换链` / `上游忽略Range→200回退`
`keep-alive同连接两个Range` / `probe报告206能力` / `probe识别忽略Range` / `probe直链失败带原始错误` /
`HLS返回direct句柄且无代理会话` / `probe HLS返回明确错误`。

```text
$ cargo test -p wp-stream
running 22 tests ... test result: ok. 22 passed
running 21 tests ... test result: ok. 21 passed
running 1 test  ... test result: ok. 1 passed
```

---

## 6. 已知限制 / 注意

1. **多段 Range 不支持**（明确 400）：当前播放器内核（libmpv/ffmpeg、VLC）只用单段 Range，故按此实现并在文档标注。
2. **上游忽略 Range 时**走整文件回退（200 → 本地裁剪）：可播放但**越靠后 seek 代价越大**；真机三盘均支持 206，未触发该路径。
3. **HLS 不代理**：m3u8 播放列表与分片由播放器直连；本代理不做播放列表重写/分片缓存。
4. **夸克 HEAD 403 是正常现象**（预签名直链仅签 GET）：`StreamProxy::open` 不依赖 HEAD（`DirectLink.size>0`），
   `probe_range` 的 HEAD 步骤会记录 403，但以 GET Range 结果判定 Range 能力。
5. **百度不可下载文件**（`error_code:31326`）属账号权限/文件归属，代理会按「换链至上限后 502」处理，不会死循环。
6. 缓存跨进程复用依赖 sidecar 元数据；数据文件被外部改动（大小不符）会自动重建，不会读到脏块。
7. `StreamHandle.direct/headers` 为本次新增字段：播放器侧 `is_hls=true` 时须用 `direct` 字段走直连分支。

## 7. M0-E1 判定

**PASS**：三盘（百度/夸克/光鸭）单段 Range 均返回 206 且 `Content-Range` 正确；本地代理在三盘真机直链上完成
「首段/中段 206 + 字节与上游一致 + 重复请求磁盘缓存命中 + 0 错误」全链路冒烟。夸克大文件 HLS 直连契约已就位，
待夸克侧把 `direct_link` 的 23018 分支接到 HLS 后即可做一条真实 HLS 冒烟。
