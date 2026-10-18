# 夸克网盘大文件播放链路攻关（23018 → 云端转码 HLS）

> 状态：**已打通并落地**（`crates/wp-drive-quark`）。
> 范围：夸克 >100MB 文件「无法取下载直链（服务端 23018）」这一播放阻塞项。
> 结论：**视频走官方 Web 播放器的云端转码 HLS 链路可播，非视频大文件无解**（降级为可读提示）。
> 真机证据：2026-10-19 于本机使用 `出/真机联调数据` 中已登录夸克账号（会员等级 **SUPER_VIP**）只读探测，
> 未对用户文件做任何删除/上传/移动操作。

---

## 0. TL;DR

| 链路 | 端 | 结果 |
|---|---|---|
| 下载直链 | `POST file/download {"fids":[fid]}` | **>约 100MB 一律失败**：HTTP 400 / `code=23018` `download file size limit`。官方 Web 端同样在此拦截（JS 里把 23018 映射成「下载文件过大」并引导客户端），换 UA/参数/域名均无法绕过 |
| 云端转码播放 | `POST file/v2/play {"fid","resolutions","supports"}` | **成功**：返回 `data.video_list[].video_info.url`（m3u8，HLS）。首次请求若未转码返回 `code=21002 video is transcoding`，轮询约 8–13s 后就绪 |
| HLS 内容 | `https://video-play-h-sz.drive.quark.cn/.../media.m3u8` | m3u8 + MPEG-TS 分片（h264+aac）；**必须带完整 Cookie**（只带 UA/Referer → HTTP 412）；`auth_key` 有效期 **10800s = 3 小时** |
| mpv 播放 | `mpv --no-config --http-header-fields="Cookie: …" <m3u8>` | **可播**（解码 h264 810x1440）；`--start=60` **seek 可播**；分片支持 `Range` 206 |
| 非视频大文件 | — | 无转码链路；`file/v2/play` 返回 `code=21005 not video`。降级：映射成可读错误「文件过大…请用夸克客户端」 |

---

## 1. 问题复现与证据（23018）

测试文件（均为用户网盘真实文件，只读）：

| 路径 | 大小 | `file/download` 结果 |
|---|---|---|
| `/video_20260903_171534.mp4` | 173,709,677 B (165.7 MiB) | ❌ HTTP 400 `code=23018` `download file size limit[573f4029…]` |
| `/video_20260903_174348.mp4` | 172,594,093 B (164.6 MiB) | ❌ 同上 `[ae89456f…]` |
| `/电脑软件/tabby_1.0.219_portable.7z` | 108,954,019 B (103.9 MiB) | ❌ 同上 `[8ca9ae6d…]` |
| `/电脑软件/pdfgear_2.1.12_setup.zip` | 135,658,154 B (129.4 MiB) | ❌ 同上 `[4828f81f…]` |
| `/电脑软件/搜狗输入法_14.8.0.9884_Setup.zip` | 50,625,888 B (48.3 MiB) | ✅ HTTP 200，`data[0].download_url` 非空 |
| `/类似的网站_v7.1.3.crx` | 1,315,261 B | ✅ HTTP 200 |

原始响应（173MB 视频，真机）：

```json
{"status":400,"code":23018,
 "message":"download file size limit[573f40294e8e46ea92d3644560a7874c]",
 "req_id":"9805e9-2b5783814aca97","timestamp":1792345259}
```

> 阈值实测落在 50.6MB（成功）与 108MB（失败）之间；实现里用 100MiB 作为「优先走转码」的启发式阈值，
> **真正的判定始终以服务端返回为准**（23018 触发兜底）。

### 1.1 官方 Web 端也是这个限制（说明不是第三方参数问题）

拉取官方网盘 Web 构建 `https://pan.quark.cn/`（`window.__LOCAL_VER='4.6.7'`）的
`https://g.alicdn.com/uc-cloud-drive-web-system/cloud-drive-web/4.6.7/index.js` 分析：

* `23018` 在 JS 中出现 10 次，全部是**下载任务失败分支**，文案「文件过大 / 下载文件过大」，
  并弹出 `QUANTITYLIMIT` 引导（引导用户用客户端）；
* 播放器走的是另一个接口（见 §1.2），与下载完全解耦。

### 1.2 从官方 JS 逆出的播放接口（关键发现）

官方 Web 播放器（4.6.7）取播放信息的实现（反编译片段，参数已对照）：

```js
c = `${cloudDriveHost}/1/clouddrive/file/v2/play`
POST {fid, resolutions: (resolutions||["low"]).join(","), supports: "fmp4,m3u8", ...rest}
→ 返回 res.data.data
```

错误码表（官方前端原样）：

| code | 官方文案 |
|---|---|
| 21001 | 视频文件不存在，无法播放 |
| 21002 | 视频正在转码中，请稍后刷新重试（常量名 `WAITING`） |
| 21003 | 该视频无法播放，换一个试试吧 |
| 22001 | 该视频无法播放（风险文件） |
| 21112 | 该视频暂不支持在线播放，请下载到本地播放（常量名 `FAIL`） |

---

## 2. 探测矩阵

所有请求统一携带 `pr=ucpro&fr=pc&uc_param_str=&__dt=1000&__t=<ms>` 与完整 Cookie，
`POST https://drive-pc.quark.cn/1/clouddrive/<path>`，除非另注。每项只发 1 次请求，人工控制频率。

### 2.1 `file/download` 变体（目标：绕过 23018）→ 全部不可行

| # | 变体 | HTTP | code | 结果 |
|---|---|---|---|---|
| V1 | `{"fids":[fid]}` 基线（PC 基址） | 400 | 23018 | ❌ 文件过大 |
| V2 | `{"fids": fid}`（字符串） | 500 | 15000 | ❌ inner error |
| V3 | `{"fid_list":[fid]}` | 400 | 14001 | ❌ Bad Parameter: fids is empty |
| V4 | `fids` + `fid_list` 同时 | 400 | 23018 | ❌ 仍限 |
| V5 | `GET file/download?fids=…` | 405 | — | ❌ 不支持 GET |
| V6 | 移动基址 `drive-m.quark.cn` + `fr=android` + 移动 UA | 401 | 401 | ❌ 需要移动端令牌（Cookie 不认） |
| V7 | 分享基址 `drive.quark.cn`（Python 参考里的共享域） | 400 | 23018 | ❌ 仍限 |
| V8 | 浏览器 Chrome UA + `x-requested-with: XMLHttpRequest` | 400 | 23018 | ❌ 仍限 |
| V9 | body 追加 `sys=Windows&app=qkpc&client=pc` 等字段 | 400 | 23018 | ❌ 仍限 |
| V10 | 同基址 `fr=android` + 移动 UA | 401 | 401 | ❌ 同 V6 |
| V11 | 换 UA/headers 组合 | 400 | 23018 | ❌ 仍限 |

**结论：23018 是服务端按文件体积的硬限制，与请求参数/UA/域名无关。** 另参考 AList 现行实现
（`drivers/quark_uc/util.go:getDownloadLink`）也没有任何绕过大文件限制的逻辑。

### 2.2 播放类接口探测

| 端点 | 方法/参数 | 结果 |
|---|---|---|
| `file/v2/play/project`（AList 用的旧/客户端变体）<br>body `{fid,resolutions:"low,normal,high,super,2k,4k",supports:"fmp4_av,m3u8,dolby_vision"}` | POST | ❌ 400 `code=14018` `data invalid: [plf_invalid]`（AList 也是把它当「转码不可用」回退） |
| **`file/v2/play`**（官方 Web 用，本文采用）<br>body `{fid,resolutions:"low,normal,high,super,2k,4k",supports:"fmp4,m3u8"}` | POST | ✅ 首次 400 `code=21002` 转码中 → 约 13s 后 200；已转码文件直接 200 |
| `file/v2/play`（非视频 7z/zip） | POST | ❌ 400 `code=21005` `not video` |
| `share/sharepage/video_preview`（分享页预览） | GET | 未采用（仅分享场景，需分享参数） |

### 2.3 `file/v2/play` 参数变体（173MB 视频，已转码后）

| 变体 | 结果 |
|---|---|
| `resolutions=low,normal,high,super,2k,4k` | ✅ 200，返回 `super`(默认)+`high` 两档（源 1080p，2k/4k 无产出） |
| `resolutions=super` | ✅ 200，只返回 `super` |
| `resolutions=""`（不传） | ✅ 200，返回全部可用档位（super+high） |
| `resolutions=low` / `normal` / `2k,4k` | ❌ 400 `code=21111` `video query resolution invalid`（这些档位对当前文件/账号不存在） |
| `supports=fmp4` / `fmp4_av,m3u8,dolby_vision` | ✅ 与 `fmp4,m3u8` 返回一致（实际仍给 m3u8） |
| `resolutions=origin/source/original` | ❌ 400 `code=21111`（**没有原画直出档位**） |

> 因此实现策略：一次请求列出全部档位，从返回的 `video_list` 里挑**可用且最高**的档位；
> 若遇 21111 再退化到「不指定档位」重试一次（让服务端给默认档位）。

### 2.4 播放接口原始响应（173MB 视频，完整字段删减）

```json
{"status":200,"code":0,"message":"ok","data":{
  "default_resolution":"super","origin_default_resolution":"super",
  "size":173709677,"file_name":"video_20260903_171534.mp4",
  "thumbnail":"https://drive-pc.quark.cn/1/clouddrive/file/video/thumbnail?fid=573f…",
  "video_list":[
    {"resolution":"super","trans_status":"success","accessable":true,
     "right":"svip","member_right":"svip","supports_format":"p_qv",
     "video_info":{"format":"m3u8","file_format":"m3u8",
        "width":810,"height":1440,"bitrate":2500.0,"codec":"h264","fps":0.0,
        "duration":99,"size":32521500,"finish":true,"success":true,"hls_type":"none",
        "audio":{"duration":99,"bitrate":128.0,"codec":"aac","channels":2},
        "url":"https://video-play-h-sz.drive.quark.cn/qv/<hash>/<dir>/media.m3u8?auth_key=1792356237-31759-10800-<sig>&sp=312&token=4-2-2-…"}},
    {"resolution":"high","trans_status":"success","accessable":true,
     "right":"svip","video_info":{"width":540,"height":960,"bitrate":1200.0,
        "url":"https://video-play-h-sz.drive.quark.cn/qv/<hash>/<dir>/media.m3u8?auth_key=1792356237-16048-10800-<sig>&…"}}
  ],
  "meta":{"duration":99,"size":173709677,"format":"mov,mp4,m4a,3gp,3g2,mj2",
          "width":1920,"height":1080,"bitrate":14065.0,"codec":"hevc","fps":30.0,
          "rotate":90,"pix_fmt":"yuv420p"},
  "audio_list":[],"preload_level":3
}}
```

要点：
* `data.size` 是**原文件**大小；`video_info.size` 是转码产物大小；
* `resolution` 档位：本次账号（SUPER_VIP）拿到 `super`/`high`；档位由账号会员等级与源分辨率共同决定；
* 源是 1080p HEVC（rotate=90 竖拍），转码产物是 **h264 810x1440 / 540x960 + aac**（有画质损失、HDR/HEVC 信息丢失）；
* `auth_key=<过期 Unix 秒>-<随机>-<ttl 秒>-<签名>`，ttl=10800 → **3 小时**后需重新取链；
* m3u8 为 VOD（`#EXT-X-PLAYLIST-TYPE:VOD`，22 个分片，MPEG-TS 2s/片，总时长 98.764s）。

### 2.5 HLS CDN 防盗链（必须 Cookie）

`GET <m3u8 地址>`：

| 请求头 | HTTP | 说明 |
|---|---|---|
| 无 | 412 | Precondition Failed |
| UA only（QQBrowser / Chrome） | 412 | 防盗链拒绝 |
| UA + `Referer: https://pan.quark.cn/` | 412 | 仍拒绝 |
| **UA + Referer + 完整 `Cookie`** | **200** | ✅ 12KB m3u8 |
| 分片（m3u8 里的 `.ts` 相对路径）带同样头 | 200 | `video/MP2T`，约 970KB/片 |
| 分片带 `Range: bytes=0-1023` | **206** | `Content-Range: bytes 0-1023/970268` |

结论：**播放器必须拿到完整 Cookie 头**；`DirectLink.headers` 原样透传即可（见 §3）。

---

## 3. 可行链路：云端转码 HLS

```
                 ┌─ 普通直链可用（≤约100MB）→ file/download → Range 直链（is_hls=false，原画）
打开网盘视频 ────┤
                 └─ 大视频 / 23018 ─────────→ file/v2/play（官方 Web 播放接口）
                                                  │
                                      code=21002 转码中 → 轮询 1/2/3/5…s（上限约26s）
                                                  │
                                      code=0 → data.video_list 选最高可用档位
                                                  │
                                      m3u8 + Cookie/UA/Referer → 播放器直连（is_hls=true）
```

### 3.1 各端点职责

1. `POST /1/clouddrive/file/download`（带 `pr/fr/...` 公共 query + Cookie）
   → `data[].download_url`。超过约 100MB 时 23018。
2. `POST /1/clouddrive/file/v2/play` body `{"fid","resolutions":"low,normal,high,super,2k,4k","supports":"fmp4,m3u8"}`
   → `data.video_list[]`；首次请求会触发/等待云端转码，转码中为 `400/21002`。
3. `GET <video_info.url>`（m3u8，带 Cookie）→ 播放器（mpv/ffmpeg）自行拉 m3u8 与分片。

### 3.2 转码等待

* 实测 173MB / 99s 视频：第一次 `v2/play` 返回 `400/code=21002`，**约 13 秒后**再请求 200 就绪；
* 是否由 `v2/play` 首次调用触发转码无法 100% 隔离（该视频此前可能已被后台预转码），
  但实现按「21002 就重试，直到就绪或超时」处理，两种情况都正确；
* 实现内轮询间隔：立即 → 1s → 2s → 3s → 5s ×4，累计约 **26 秒**；仍不就绪返回可读错误
  （官方 Web 端的做法是直接提示「转码中，请稍后刷新重试」，我们多等一会儿以改善首次体验）。

---

## 4. 实现（`crates/wp-drive-quark`）

新增 `src/play.rs`，并改 `src/download.rs` 的直链逻辑：

### 4.1 `DirectLink` 契约（`wp-core` 新增字段）

```rust
pub struct DirectLink {
    pub url: String,                    // 普通直链 URL，或 HLS 的 m3u8 URL
    pub headers: Vec<(String, String)>, // HLS 时必须含完整 Cookie / UA / Referer
    pub expire_at: Option<i64>,         // HLS：auth_key 解析出的过期秒（约 now+3h）
    pub size: u64,                      // 原文件大小（不是转码产物）
    pub file_name: String,
    pub is_hls: bool,                   // ★ 新增：HLS 需播放器直连，不要走 Range 代理/缓存
}
```

### 4.2 取链策略（`direct_link_for` → `direct_link_for_entry`）

1. 解析条目；
2. **是视频且 size > 100MiB** → 直接走 `fetch_play_link`（HLS，`is_hls=true`）；
   - 转码不可用（21001/21003/21005/21112/21111/Unsupported）→ 回退直链，由直链给最终可读错误；
   - 转码等待超时（21002）→ 直接返回可读「转码中」错误（不做无限重试）；
3. 否则先走 `file/download`：
   - 成功 → 普通直链（`is_hls=false`，原画质量，Range）；
   - 失败且是 **23018**：
     * 视频 → 兜底走 HLS；
     * 非视频 → 返回可读业务错误（`ErrorKind::Unsupported`，区别于网络错误，供 UI 提示）；
4. `download_to_file`（下载到本地）遇到 23018 同样返回可读错误（大文件请用夸克客户端）。

### 4.3 错误码映射

| 服务端 | 实现处理 | 用户可见文案（节选） |
|---|---|---|
| `23018` 非视频 | `Error::unsupported`（不可重试、非网络错误） | 「夸克限制：文件超过 100MB：夸克直链下载被服务端拒绝（23018 download file size limit），请用夸克网盘客户端下载（xxx，103.9MB）」 |
| `23018` 视频（HLS 也不可用） | `Error::unsupported` | 「该视频超过 100MB：直链下载被服务端拒绝（23018），且在线转码播放不可用；请用夸克网盘客户端播放/下载」 |
| `21002` 转码中 | 轮询约 26s；超时 `Error::server(21002, …)` | 「夸克视频正在云端转码（首次播放需转码，通常十几秒，长视频更久），已等待约 26 秒仍未就绪，请稍后重试」 |
| `21005` 非视频 / `21001/21003/22001/21112` | `Error::unsupported` | 「夸克：该视频暂不支持在线播放（服务端 21112：…）；文件超过 100MB 时直链下载也不可用，请用客户端…」 |
| `21111` 画质档位非法 | 不指定档位重试一次 | — |

### 4.4 转码档位选择

`video_list` 中跳过 `accessable=false` 与 `trans_status != success`（transcoding 计入等待），
按 `4k > 2k > super > high > normal > low` 取最高可用档位；`expire_at` 从 `auth_key` 解析。

### 4.5 播放器侧约定（给 wp-stream/GUI 的集成说明）

* `is_hls == false`：维持现状——URL 交给 wp-stream 做 Range 代理/预读/缓存/换链；
* `is_hls == true`：**m3u8 直连播放器**（mpv/ffmpeg 原生 HLS），不要走 Range 代理
  （代理需重写 m3u8 内相对分片地址，性价比低；mpv 自带 HLS 缓存与分片预读）；
  `headers` 必须透传给 mpv（建议 GUI 用 `MediaSource::Network(url, headers)` 或等价机制）；
* HLS 断链/过期（约 3h）时重新调 `Drive::direct_link` 换新 m3u8 即可；
* mpv 侧等价参数：`--http-header-fields="Cookie: …" --http-header-fields-add="Referer: https://pan.quark.cn/" --user-agent=<UA>`。

---

## 5. 验证证据（真机）

### 5.1 接口链路

* `file/download` 173MB → 400/23018（§1）；
* `file/v2/play` 首次 → 400/21002；13s 后 → 200 + `video_list` 两档（§2.4）；
* 172MB 第二个视频 → 200 直接两档（同接口）；
* 非视频 108MB 7z / 135MB zip → play 400/21005 `not video`。

### 5.2 HLS 可拉取

```console
# 不带 Cookie（UA+Referer）
HTTP 412 Precondition Failed
# 带完整 Cookie
HTTP 200, 12005 字节 m3u8（22 个分片，VOD）
# 分片
HTTP 200, 970268 字节, content-type: video/MP2T
# 分片 Range
HTTP 206, Content-Range: bytes 0-1023/970268, 1024 字节
```

ffprobe（带 Cookie/Referer 头）：

```console
$ ffprobe -headers "Cookie: …\r\nReferer: https://pan.quark.cn/\r\n" -of json <m3u8>
codec_name=h264  810x1440  avg_frame_rate=25/1
codec_name=aac
format_name=hls   duration=98.764000
```

### 5.3 mpv 真机播放（无 DISPLAY，xvfb-run + 导出帧）

```console
$ CK=$(jq -r .cookie_string 出/真机联调数据/账号/quark/凭证.json)
$ xvfb-run -a mpv --no-config --vo=image --vo-image-format=png --vo-image-outdir=/tmp/mpvshots \
    --ao=null --frames=5 \
    --user-agent='Mozilla/5.0 … QQBrowser/12.2.5544.400' \
    --http-header-fields="Cookie: $CK" \
    --http-header-fields-add="Referer: https://pan.quark.cn/" \
    '<m3u8 地址>'

● Video  --vid=1   (h264 810x1440 25 fps)
VO: [image] 810x1440 yuv420p
[vo/image] Saving /tmp/mpvshots/00000001.png … 00000005.png
Exiting... (End of file)        # exit=0
```

seek（从 60s 开始，同样解出画面）：

```console
$ xvfb-run -a mpv --no-config --vo=image --ao=null --frames=5 --start=60 … <m3u8>
● Video  --vid=1   (h264 810x1440 25 fps)
VO: [image] 810x1440 yuv420p
Exiting... (End of file)        # exit=0
```

> 注：mpv 命令行里 `--http-header-fields` 是 **list 选项**，第二个头必须用
> `--http-header-fields-add=` 追加（直接写两次后者会覆盖前者 → 缺 Cookie → CDN 412）。

### 5.4 CLI（本项目真实实现）端到端证据

用改造后的 `wp-drive-quark` 经 `wp-cli` 取链，再把返回的 `headers` 原样交给 mpv：

```console
$ cargo run -q -p wp-cli -- --data-dir 出/真机联调数据 --drive quark link "/video_20260903_171534.mp4"
{
  "url": "https://video-play-h-sz.drive.quark.cn/qv/<hash>/<dir>/media.m3u8?auth_key=…",
  "headers": [["user-agent","…QQBrowser/12.2.5544.400"],["referer","https://pan.quark.cn/"],
              ["accept","*/*"],["cookie","Video-Auth=…; __kp=…; __kps=…; __pus=…; __puus=…; …"]],
  "expire_at": 1792357046,          # now+10800s
  "size": 173709677,                # 原文件大小
  "file_name": "video_20260903_171534.mp4",
  "is_hls": true                    # ★ HLS 分支生效
}
```

把 `url`/`headers` 传给 mpv（本项目实现产出的真实 m3u8）：

```console
$ xvfb-run -a mpv --no-config --vo=image --vo-image-outdir=/tmp/mpv_e2e --ao=null --frames=5     --user-agent="$H_USER_AGENT"     --http-header-fields="Cookie: $H_COOKIE"     --http-header-fields-add="Referer: https://pan.quark.cn/" "$HLS_URL"
● Video  --vid=1   (h264 810x1440 25 fps)
VO: [image] 810x1440 yuv420p
[vo/image] Saving /tmp/mpv_e2e/00000001.png … 00000005.png
Exiting... (End of file)        # exit=0
```

导出帧为有效画面（沙滩/人物场景，右下角水印时间 2026.09.03 17:15 与文件名 `video_20260903_171534` 吻合），
说明不是空帧/占位。其余三条路径同 CLI 验证：

| 路径 | CLI 结果 |
|---|---|
| 173MB mp4（大视频） | `is_hls: true`，m3u8 可播（上） |
| 50.6MB zip（普通文件） | `is_hls: false`，`expire_at: null`，原直链不变 |
| 108MB 7z（大非视频） | `ERROR: 不支持：夸克限制：文件超过 100MB：夸克直链下载被服务端拒绝（23018 download file size limit），请用夸克网盘客户端下载（tabby_1.0.219_portable.7z，103.9MB）` |

### 5.5 单元测试（不联网）

`cargo test -p wp-drive-quark` 覆盖：

* `23018` → 可读 Unsupported 错误的映射；
* 播放响应解析：多档位取最高、跳过 `accessable=false`、`transcoding`/结构异常分类；
* `auth_key` 过期时间解析；
* `is_video_entry`（`obj_category` 优先、扩展名兜底）；
* HLS `DirectLink` 构造：`is_hls=true`、headers 原样、`size/file_name` 用原文件；
* 轮询：mock `21002`×2 → 200 成功（调用 3 次）；一直 21002 → 超时可读错误；网络错误不重试。

---

## 6. 限制与风险（必须如实告知 UI/用户）

| # | 限制 | 说明 / 缓解 |
|---|---|---|
| L1 | **不是原画**：HLS 是云端转码产物（h264+aac） | 源 1080p HEVC → super 档 810x1440@2500kbps；HDR/HEVC/多音轨/内嵌字幕/章节等原盘特性丢失。播放器字幕可另走外挂/AI 字幕；如需原画只能下载（>100MB 又被 23018 拦）→ **大文件在 Web/第三方 API 下拿不到原画是夸克侧的产品限制** |
| L2 | 画质档位受会员等级限制 | `video_list[].right="svip"`；本机账号为 SUPER_VIP 才能拿 `super/high`。**免费账号未验证**（无法在不换登录态的情况下测试）；从官方 Web 行为推断免费账号可拿更低档或提示开通会员 |
| L3 | 首播需等云端转码 | 实测 8–13s（99s/173MB）；4K 长片可能更久。实现最多等约 26s，超时给「转码中，稍后重试」 |
| L4 | m3u8 3 小时过期 | `auth_key` ttl=10800s；重调 `direct_link` 换链即可 |
| L5 | HLS 必须带 Cookie | 缺 Cookie → m3u8/分片 412；播放器必须透传 `DirectLink.headers` |
| L6 | 非视频大文件无解 | >100MB 的 zip/7z 等：`file/download` 23018、无转码链路 → 只能夸克客户端/浏览器下载 |
| L7 | 非视频 / 风险文件 / 不支持的编码 | 21001/21003/21005/22001/21112 → 可读错误提示用客户端 |
| L8 | 移动端接口未打通 | `drive-m`/`fr=android` 需要移动端令牌（401），未走通，也未采用 |
| L9 | 云转码涉隐私 | 播放会把文件交给夸克云端转码（与官方 Web 播放行为一致），文档/隐私说明需提及 |

### 降级方案（UI 文案建议）

* 非视频大文件（23018）：**「该文件超过 100MB，夸克直链下载受限；请用夸克网盘客户端下载」**
* 大视频转码等待超时：**「视频正在云端转码，请稍后重试（首次播放需要转码）」**
* 视频不支持在线播放（21112）：**「该视频暂不支持在线播放，请用夸克网盘客户端播放」**
* 免费账号画质低：按接口返回档位播放即可，不做强提示（档位以 `video_list` 为准）

---

## 7. 复现命令（只读，人工控制频率）

```console
BASE=https://drive-pc.quark.cn/1/clouddrive
Q='pr=ucpro&fr=pc&uc_param_str=&__dt=1000&__t=0'
CK=$(jq -r .cookie_string 出/真机联调数据/账号/quark/凭证.json)

# ① 23018 复现
curl -sS -X POST "$BASE/file/download?$Q" -H "cookie: $CK" \
  -H 'content-type: application/json' -H 'origin: https://pan.quark.cn' \
  -H 'referer: https://pan.quark.cn/' \
  -d '{"fids":["573f40294e8e46ea92d3644560a7874c"]}'
# → {"status":400,"code":23018,"message":"download file size limit[...]"}

# ② 取云端转码 HLS（首次可能 21002，重试即可）
curl -sS -X POST "$BASE/file/v2/play?$Q" -H "cookie: $CK" \
  -H 'content-type: application/json' -H 'origin: https://pan.quark.cn' \
  -H 'referer: https://pan.quark.cn/' \
  -d '{"fid":"573f40294e8e46ea92d3644560a7874c",
       "resolutions":"low,normal,high,super,2k,4k","supports":"fmp4,m3u8"}'
# → 200 data.video_list[].video_info.url = https://video-play-h-sz.drive.quark.cn/.../media.m3u8?auth_key=...

# ③ 播放（需给 mpv 传 Cookie/Referer/UA，见 §5.3）
```

> 探测脚本（本机保留，未入库）：`/tmp/qprobe.py`；请求日志 `/tmp/qprobe.log.jsonl`。

---

## 8. 后续（不在本任务范围）

* GUI/播放器侧按 `is_hls` 分流：HLS 直连 mpv，普通直链走 wp-stream（由 Lead 协调）；
* 播放器「画质选择」若要做：把 `resolutions` 抽成参数并缓存 `video_list` 档位信息；
* 字幕/音轨：转码 HLS 通常只有单音轨、无内嵌字幕；字幕走独立字幕轨（项目已有 `wp-subtitle`）；
* 免费账号档位、4K 源（2k/4k 档）待有对应账号/素材时补测。
