# 三盘真机播放矩阵（百度 / 夸克 / 光鸭）—— M1/M3 验收证据

> 任务：task-12「三盘真机播放矩阵」。目标：用真机会话验证三盘「取链 → mpv 播放 → seek」全链路，
> 固化可复现脚本与证据，为 M1（播放内核）/ M3（源接入）验收提供依据。
>
> 运行时间：2026-10-19 02:36–02:44（Asia/Shanghai）
> 数据目录：`出/真机联调数据`（三盘真机会话；只读，未对用户文件做任何写操作）
> 证据目录：**`出/真机联调数据/日志/播放矩阵/20261019-023624/`**
> 复现脚本：`scripts/real-device-playback-matrix.sh`（默认 dry-run，`--run` 执行）
> 环境：Arch Linux、无 DISPLAY（`xvfb-run`）、mpv **v0.41.0**、ffmpeg 自带 HLS demuxer、wp-cli（本仓）

---

## 0. 结论（TL;DR）

| 盘 | 用例 | 链路 | 播放 | seek | 判定 |
|---|---|---|---|---|---|
| 百度 | 16.3MB mp4（h264） | 普通直链（Range） | ✅ h264 1080x1920 + aac | ✅ 2s | **PASS** |
| 百度 | 50.6MB mp4（hevc） | 普通直链（Range） | ✅ hevc 1920x1080 + aac | ✅ 15s | **PASS** |
| 百度 | 118MB mp4 | 普通直链（Range） | ❌ CDN 403 | ❌ | **WARN（预期内，见 §3.1）** |
| 夸克 | 11.5MB mp4 | 普通直链（Range，需 Cookie） | ✅ h264 960x540 + aac | ✅ 2s | **PASS** |
| 夸克 | **173.7MB mp4** | **云端转码 HLS（is_hls=true，需 Cookie）** | ✅ h264 810x1440 + aac | ✅ 60s | **PASS** |
| 光鸭 | **727.7MB mkv** | 普通直链（签名 URL，无 headers） | ✅ hevc 1920x1080 + FLAC | ✅ 120s | **PASS** |

**5 个严格用例全 PASS**；百度 >50MB 用例按预期 403 记 WARN（平台侧限制，已记录证据）。
三盘「直链 + Range」「夸克 HLS 直连」两类链路在 mpv 真机全部可播、可 seek。
**未发现需要修改 `crates/wp-drive-quark` 的链路 bug。**

---

## 1. 环境与判定口径

| 项 | 值 |
|---|---|
| mpv | `mpv v0.41.0`（系统 `/usr/bin/mpv`），HLS 用内置 ffmpeg hls demuxer |
| 播放口径 | `xvfb-run -a mpv --no-config --vo=null --ao=null --frames=5 <url>`，带 `DirectLink.headers` |
| seek 口径 | 同等参数 + `--start=<N>`（各用例按视频时长选 2/15/30/60/120s） |
| PASS | mpv 退出码 0 + 输出含 `● Video` + `VO: [null]` + `Exiting... (End of file)`；seek 同样条件 |
| Range 证据 | `wp-cli probe-range <path>` 的 head/first/mid 三段状态；HLS 用例改用 m3u8+分片直测 |
| 帧数 | 默认 5 帧（`WP_MATRIX_FRAMES` 可覆盖）——只验证「能打开+解码出画」，不跑长片性能 |

> 说明：`wp-cli probe-range` 会对同一文件重新取链并做 HEAD/Range 探测；它与 mpv 使用**不同的 URL 签名**
> （每次 direct_link 都重新签发），因此「probe-range 206 但 mpv 失败」这类现象需要单独解释（见 §3.3）。

---

## 2. 矩阵明细

### 2.1 汇总表

| # | case | 盘 | 文件（远端路径） | 大小 | 期望链路 | 实际 `is_hls` | mpv 视频/音频 | seek | Range 证据 | 结果 |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | `baidu-small` | 百度 | `/来自：V2408A/DCIM/1074058976_1734538851561.mp4` | 16,306,047 | plain | false | h264 1080x1920 31.05fps / aac 2ch | 2s ✅ | head 200 / first 206 / mid 206 | ✅ PASS |
| 2 | `baidu-large` | 百度 | `/来自：V2408A/DCIM/Camera/video_20250722_184451.mp4` | 50,603,985 | plain | false | hevc 1920x1080 30.01fps / aac 256kbps | 15s ✅ | head 200 / first 206 / mid 206 | ✅ PASS |
| 3 | `baidu-over50-403` | 百度 | `/来自：V2408A/DCIM/Camera/video_20250714_182340.mp4` | 118,000,134 | plain | false | ❌ CDN 403（31326 hitcode:119） | ❌ | head/first/mid 均 403 | ⚠️ WARN（tolerate） |
| 4 | `quark-small` | 夸克 | `/模拟器游戏/pc端全能模拟器/29元完美版免安装，自带34T游戏库/29米一键安装整合包版本介绍.mp4` | 11,452,287 | plain | false | h264 960x540 30fps / aac 2ch | 2s ✅ | **head 403**（预签名只签 GET）/ first 206 / mid 206 | ✅ PASS |
| 5 | `quark-large-hls` | 夸克 | `/video_20260903_171534.mp4` | 173,709,677 | **hls** | **true** | h264 810x1440 25fps / aac 2ch | 60s ✅ | m3u8 200 / 22 分片 / 首分片 **206** `bytes 0-1023/970268` | ✅ PASS |
| 6 | `guangya-mkv` | 光鸭 | `/来自：分享/东京复仇者/东京复仇者.S01E25.2021.1080p.mkv` | 727,730,541 | plain | false | hevc 1920x1080 23.976fps / FLAC 2ch | 120s ✅ | head 200 / first 206 / mid 206 | ✅ PASS |

### 2.2 每用例证据（mpv 关键输出）

**① baidu-small（普通直链）**

```text
link: is_hls=False size=16306047 expire_at=None url_host=d.pcs.baidu.com n_headers=5 has_cookie=True
range: head=200 first=206 mid=206
play : rc=0 ● Video --vid=1 (h264 1080x1920 31.0542 fps) [default]
            ● Audio --aid=1 (aac 2ch 44100 Hz 134 kbps) [default]   VO: [null] 1080x1920
seek2: rc=0 ● Video --vid=1 (h264 1080x1920 31.0542 fps)
```

**② baidu-large（普通直链；大文件里本账号唯一可下载的档位）**

```text
link: is_hls=False size=50603985 expire_at=None url_host=d.pcs.baidu.com n_headers=5 has_cookie=True
range: head=200 first=206 mid=206
play : rc=0 ● Video --vid=1 --vlang=eng (hevc 1920x1080 30.0056 fps) [default]
            ● Audio --aid=1 --alang=eng (aac 2ch 48000 Hz 256 kbps) [default]   VO: [null] 1080x1920
seek15: rc=0 ● Video --vid=1 (hevc 1920x1080 30.0056 fps)
```

**③ baidu-over50-403（>50MB：平台侧 403，链接签发成功但 CDN 拒绝）**

```text
link: is_hls=False size=118000134 expire_at=None url_host=d.pcs.baidu.com n_headers=5 has_cookie=True
range: head=403 first=403 mid=403
first.error: HTTP 403：{"error_code":31326,"error_msg":"user is not authorized, hitcode:119", ...}
play : rc=2 [ffmpeg] https: HTTP error 403 Forbidden
seek30: rc=2
```

**④ quark-small（普通直链，需 Cookie；HEAD 403 属预签名 URL 正常行为）**

```text
link: is_hls=False size=11452287 expire_at=None url_host=dl-pc-zb.pds.quark.cn n_headers=4 has_cookie=True
range: head=403 first=206 mid=206     # HEAD 403 / GET Range 206
play : rc=0 ● Video --vid=1 (h264 960x540 30 fps) [default]
            ● Audio --aid=1 (aac 2ch 44100 Hz 48 kbps) [default]   VO: [null] 960x540
seek2: rc=0 ● Video --vid=1 (h264 960x540 30 fps)
```

**⑤ quark-large-hls（>100MB → 云端转码 HLS，is_hls=true）**

```text
link: is_hls=True size=173709677 expire_at=1792359769 url_host=video-play-h-sz.drive.quark.cn n_headers=4 has_cookie=True
range: probe-range 拒绝：ERROR: 不支持：HLS 流不支持 Range 探测（is_hls=true，应把 m3u8 URL+headers 直接交给播放器）
       —— 这是按契约的预期行为（wp-stream 已实现 is_hls 分流）
hls  : m3u8=200 bytes=11917 segs=22 seg=206 cr=bytes 0-1023/970268
play : rc=0 ● Video --vid=1 (h264 810x1440 25 fps)
            ● Audio --aid=1 --alang=eng (aac 2ch 44100 Hz)   VO: [null] 810x1440
seek60: rc=0 ● Video --vid=1 (h264 810x1440 25 fps)
```

> 云端转码 HLS 的画质/会员/过期限制详见 [PLAYER_QUARK_STREAM.md](PLAYER_QUARK_STREAM.md)
> （本用例源为 1080p HEVC 竖拍，转码产物 h264 810x1440；`auth_key` 3h 过期）。

**⑥ guangya-mkv（727MB 普通直链，无 headers）**

```text
link: is_hls=False size=727730541 expire_at=1792370630 url_host=hwsh01-httpdown.guangyacdn.com n_headers=0 has_cookie=False
range: head=200 first=206 mid=206
play : rc=0 ● Video --vid=1 '55308_lolice-EC' (hevc 1920x1080 23.976 fps) [default]
            ● Audio --aid=1 --alang=ja 'FLAC 2.0' (flac 2ch 48000 Hz) [default]
            VO: [null] 1920x1080 yuv420p10
seek120: rc=0 ● Video --vid=1 (hevc 1920x1080 23.976 fps)
```

### 2.3 证据文件索引

证据目录：`出/真机联调数据/日志/播放矩阵/20261019-023624/`（目录权限 700，仅本用户可读）

| 文件 | 内容 |
|---|---|
| `matrix.tsv` | 6 用例汇总（case/drive/file/size/expect/is_hls/play/seek/range_or_hls/result/note） |
| `<case>.result.txt` | 单用例：远端路径、复现命令、Range/HLS 摘要、mpv Video/Audio/VO 行、seek 行、PASS/WARN |
| `<case>.probe-range.json` | `wp-cli probe-range` 原始结果（含 url_host/size/header **名单**/head/first/mid）；**不含 Cookie 值** |
| `<case>.link.sanitized.json` | 脱敏 DirectLink（URL 去掉签名 query，只留 host+路径；header 值只记录长度） |
| `<case>.mpv-play.log` | 播放（前 5 帧）mpv 完整 stdout/stderr |
| `<case>.mpv-seek.log` | seek 后播放 mpv 完整输出 |
| `<case>.hls-check.json` | HLS 专有：m3u8 状态/字节数/分片数/首分片 Range 状态 |

> 隐私纪律：原始 DirectLink（含签名与 Cookie）只在脚本的 `mktemp` 私有临时目录中存在，退出时删除；
> 证据目录全部为脱敏数据。

---

## 3. 失败与限制分析

### 3.1 百度 >约 50MB 文件 → CDN 403（31326），非本仓链路 bug

真机尺寸阶梯（同一账号、同一 `wp-cli` 与 header 组合）：

| 文件大小 | probe-range head/first/mid | 结论 |
|---|---|---|
| 16,306,047（16.3MB） | 200 / 206 / 206 | ✅ 可播 |
| 37,752,737（36MB） | 200 / 206 / 206 | ✅ |
| 50,603,985（48.3MiB） | 200 / 206 / 206 | ✅（本矩阵 `baidu-large`） |
| 50,603,985±以上、50,603,985 < s 的下一档：59,338,798（56.6MiB） | 403 / 403 / 403 | ❌ `31326 user is not authorized, hitcode:119` |
| 82,834,093（79MiB） | 403 | ❌ 同 31326 |
| 96,952,003（92.5MiB） | 403 | ❌ 同 31326 |
| 105,146,829（100.3MiB） | 403 | ❌ 同 31326 |
| 118,000,134（112.5MiB） | 403 | ❌ 同 31326（本矩阵 tolerate 用例） |
| 172,594,093 / 2,395,521,159（历史实测） | 403 | ❌ 同 31326 |

判读：
* `direct_link` **本身成功**（拿到了带签名的 URL，`probe-range` 的 link 阶段 OK）；
  失败发生在 `d.pcs.baidu.com` 的 GET 阶段，返回 `{"error_code":31326,"error_msg":"user is not authorized","hitcode":119}`。
* 阈值落在 50.6MB（可）与 59.3MB（不可）之间：**约 50MB**，与「Range/HEAD 语义」「是否带 Cookie」无关
  （小文件带同一组头 206；大文件带同一组头 403）。
* 与 [PLAYER_STREAM_M0.md](PLAYER_STREAM_M0.md) 里 `video_20260322_154432.mp4`（2.4GB）的 31326 同源：
  属百度**账号权限/大文件下载策略**（该账号非会员大文件被拒；或文件归属/风控），不是播放器或适配器 bug。
* 产品影响：百度 >50MB 视频**当前账号**无法通过 API 直链播放 → 播放器应把 403/31326 映射成可读提示
  （「百度限制：该账号大文件直链不可用，请用百度网盘客户端或开通会员」）。后续若要覆盖百度大文件，
  需换会员账号重测或走「客户端下载」侧链路（属 M3 后续决策，非本矩阵范围）。
* 矩阵里把它固化成 `baidu-over50-403`（`policy=tolerate`）：每次 `--run` 都自动记录一次 403 证据，
  但不算失败。

### 3.2 夸克 HEAD 403 属正常（预签名 URL 只签 GET）

`quark-small` 的 probe-range 显示 `head=403 first=206 mid=206`：
夸克直链的预签名只对 GET 生效，HEAD 会被 CDN 拒绝——**不是链接失效**。
探测/代理不得以 HEAD 失败判定不可播，应以 GET（Range）为准。HLS 用例同样不受影响。

### 3.3 「probe-range OK 但 mpv 失败」与 mpv 的 header 逗号坑（本次修掉的集成问题）

初次跑矩阵时 `baidu-small` mpv 返回 **HTTP 400 Bad Request**，而同一文件 `probe-range` 是 206。定位过程：

1. 复现：分别用 curl / ffprobe / mpv 请求同一 `DirectLink`；
2. curl 与 ffprobe 带头（UA/Referer/Cookie）均 200/206，只有 mpv 400；
3. 差异在 header 集合——mpv 的 `--http-header-fields` 是 **String list（逗号分隔）**，
   而百度 `Accept-Language: zh-CN,zh;q=0.9` 的值里含逗号 →
   被拆成 `Accept-Language: zh-CN` + 非法项 `zh;q=0.9` → 请求头畸形 → 上游 400；
4. 本地 echo server 实测确认：`--http-header-fields="X-A: a,b"` 到达服务器时值只剩 `a`，
   后续 `-add` 的头甚至会被丢弃；`\,` 转义无效；
5. 修复：矩阵脚本在给 mpv 注入头时**跳过含逗号的头**（记录到结果；本次仅 Accept-Language 被跳过，
   关键头 Cookie/Referer/Authorization 三盘实测均不含逗号），播放即 PASS。

**给播放器实现（MpvEngine）的结论**：
* mpv **CLI** 的 `--http-header-fields` 无法可靠承载含逗号的值；
* mpv **client API** 用 `MPV_FORMAT_NODE_ARRAY` 逐项设置 `http-header-fields`（每项一个 "Name: value"）
  不经过逗号解析，可以原样传递——media-core 已按此实现并在 M0 交叉验证通过（本地 HLS server 实测
  m3u8 + 4 个 .ts 全部带 Cookie/Referer/UA）。
* 因此：**播放器侧不丢头；本仓库的 CLI 证据脚本按「跳过+记录」处理即可。**

### 3.4 夸克 HLS 不走 Range 探测（按契约）

`quark-large-hls` 的 `probe-range` 主动报错「HLS 流不支持 Range 探测（is_hls=true，应把 m3u8 URL+headers
直接交给播放器）」——这是 wp-stream 按 `DirectLink.is_hls` 实现的正确分流。HLS 的字节校验改为：
m3u8 200（22 分片）+ 首分片 Range 0-1023 → 206（`bytes 0-1023/970268`）。

### 3.5 其它限制（本轮不覆盖，记录备查）

| 项 | 说明 |
|---|---|
| 断流/换链 | 本轮不验证；属 wp-stream 换链 + 播放器错误恢复（media-core/stream 侧后续联合验证） |
| 长片性能/丢帧 | 本轮只跑 `--frames=5`（打开+解码验证）；播放性能矩阵由 M0/性能专项覆盖 |
| 百度大文件 | 见 §3.1；需会员账号或客户端下载链路 |
| 夸克 HLS 画质 | 云端转码非原画、档位受会员限制、m3u8 3h 过期 → 见 [PLAYER_QUARK_STREAM.md](PLAYER_QUARK_STREAM.md) §6 |
| 光鸭 headers | 直链为签名 URL、`DirectLink.headers` 为空；到期换链由 stream 侧负责（本轮验证可播） |

---

## 4. 复现方式

### 4.1 一条命令复跑全部矩阵

```bash
cd 网盘管理_V10

# ① dry-run（默认，只打印命令，不联网）
./scripts/real-device-playback-matrix.sh

# ② 真跑全部 6 个用例（约 6–8 分钟；生成新证据目录）
./scripts/real-device-playback-matrix.sh --run

# ③ 只跑某用例 / 指定证据目录
./scripts/real-device-playback-matrix.sh --run --only quark-large-hls
./scripts/real-device-playback-matrix.sh --run --out /tmp/matrix-20261019

# ④ 列出用例
./scripts/real-device-playback-matrix.sh --list
```

脚本行为：
* 默认 **dry-run**（不访问网络）；`--run` 才执行；`--only/--out/--data-dir/--frames` 可调；
* 默认证据目录 `出/真机联调数据/日志/播放矩阵/<时间戳>/`；
* **不写真实 Cookie**：原始 DirectLink 只在 `mktemp` 私有目录（trap 清理），证据全部脱敏；
* wp-cli 定位支持 `CARGO_TARGET_DIR` 重定向（本机 target 在 `共享基座/重活/构建/cargo-target`），
  可用环境变量 `WP_CLI_BIN` 覆盖；
* 依赖：`bash python3 wp-cli mpv xvfb-run curl ffprobe`；
* 退出码：全部 strict 用例 PASS → 0；有 strict 失败 → 1；tolerate 失败只记 WARN；
* 音频/视频/seek 判定与证据格式见脚本头部注释。

### 4.2 单用例手工复现（以夸克 HLS 为例）

```bash
# 1) 取链（本仓适配器，大视频自动 HLS）
cargo run -q -p wp-cli -- --data-dir 出/真机联调数据 --drive quark link "/video_20260903_171534.mp4" > /tmp/link.json
# 2) 从 JSON 取 url 与 headers（Cookie 必须透传；HLS 缺 Cookie 会 412）
# 3) 播放 / seek（注意：--http-header-fields 是 list，第二个头起必须用 -add；含逗号的头需跳过或走 client API）
CK=$(python3 -c "import json;d=json.load(open('/tmp/link.json'));print(dict((k.lower(),v) for k,v in d['headers'])['cookie'])")
URL=$(python3 -c "import json;print(json.load(open('/tmp/link.json'))['url'])")
xvfb-run -a mpv --no-config --vo=null --ao=null --frames=5 \
  --user-agent='Mozilla/5.0 … QQBrowser/12.2.5544.400' \
  --http-header-fields="Cookie: $CK" \
  --http-header-fields-add="Referer: https://pan.quark.cn/" "$URL"
# seek：再加 --start=60
```

---

## 5. 对 M1/M3 验收的输入

1. **三盘取链 + Range 直连**：百度/夸克/光鸭普通文件在真机全部 206 可播；夸克 HEAD 403、光鸭无 headers
   属各自 URL 语义特征，播放器/代理需按 GET+Range 判定。
2. **夸克 HLS 大视频链路**：`is_hls=true` 的 m3u8 直连 mpv 可播可 seek（seek 60s 实测）；播放器按
   `DirectLink.headers` 透传即可；不要交给 wp-stream Range 代理。
3. **百度大文件限制**：>约 50MB 该账号 403/31326，非链路 bug；UI 需可读提示（本仓适配器已对夸克 23018
   给可读错误，百度侧同类映射建议在 M3/UI 层跟进）。
4. **mpv 集成注意**：CLI list 选项逗号坑（§3.3）；播放器实现请用 client API node array 或等价方式。
5. 回归：`scripts/real-device-playback-matrix.sh --run` 可作为 M1/M3 的真机冒烟门禁（strict 用例须全 PASS）。

---

## 6. 附：本次运行环境与命令时间线

```text
02:36:24  matrix 开始（baidu-small）
02:38:35  baidu-large PASS
02:42:29  baidu-over50-403 WARN（403 证据）
02:42:41  quark-small PASS
02:43:13  quark-large-hls PASS（m3u8 + seek60）
02:44:30  guangya-mkv PASS（seek120）
02:44:39  全矩阵结束：5 strict PASS + 1 tolerate WARN，exit=0
```

原始运行日志：`/tmp/matrix_run2.log`（临时，复跑会重新生成）；证据目录见 §2.3。
