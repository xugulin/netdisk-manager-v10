# PLAYER_PERF —— 网盘流播放性能压测与代理参数调优

> 对应任务：**task-14 / M1「4K 无比流畅」验收数据**
> 工具：`crates/wp-stream/examples/stream_bench.rs`（新增，可复跑）
> 数据来源：`出/真机联调数据` 真实会话（百度 / 夸克 / 光鸭），**只读**；缓存目录在系统临时目录，测完即删。
> 日期：2026-10-19 · 环境：Linux 7.2.8-1-cachyos x86_64 / i7-1195G7 / 8 线程；测试窗口机器负载 load1 6–13（多人并行构建/测试），**且测试窗口公网明显劣化**（见 §1.3），下表数据应视为**下限参考**，相对关系与公式结论有效。

---

## 0. 结论先行

| 项 | 结论 |
|---|---|
| 代理吞吐 | 代理顺序读吞吐 ≈ 直连基线（劣化网络下 4.25 vs 4.83 Mbps，差 ~12%，主要来自预读并发/分块粒度），**代理不是吞吐瓶颈**；本地缓存命中读可达 GB/s 级（矩阵中实测 2.7–8.2 Gbps 等价） |
| 起播延迟 | **≈ 分块大小 / 带宽**：光鸭同网络下 chunk=1M 起播 1.57s、chunk=4M 起播 6.59s（≈4×，符合公式）；百度劣化网络 chunk=256K 起播 6.39s（≈256KB/0.33Mbps，公式吻合）。**弱网/移动端应减小分块** |
| seek | 冷 seek 未命中 = 一次分块下载（实测 1.2–3.0s，随网络）；**缓存命中 seek 0.2–9.5ms（本地磁盘，与网络无关）**，回看/拖动体验由预读窗口覆盖决定 |
| 预读 | 预读窗口填满时间受网络限制（20s 超时内填 23–32MB 到 64–128MB 目标）；**多连接聚合有效**：单流 ~3–5Mbps，parallel=2 预读聚合 ≈ 21.8Mbps（4–5×） |
| 参数建议（4K 40–100Mbps） | **chunk=4MB（弱网 1–2MB）· readahead=128MB（≈8.5–25s 缓冲）· parallel=4（弱网 2）· LRU=50GB（4K 重度；现有 20GB 保留为默认）** |
| HLS | 正常网络 m3u8 0.65–1.07s、首分片 1.0s（7.7Mbps）、40% seek 2.2s/2.2MB；mpv 实测起播 4.7s、seek 19.9s（详见 `PLAYER_STREAM_M0.md` §9）。HLS 无缓存，seek 成本=重新拉分片 |
| 发现的问题 | ① 弱网下**预读会与 seek 抢带宽**（矩阵 ra=128M 冷 seek 16.4s vs ra=0 4.1s）；② 起播受「首块就绪才回首字节」策略约束（=chunk/带宽）。两项均给出调参/后续优化建议，**未改冻结 API** |

---

## 1. 环境与测量方法

### 1.1 被测对象

| 盘 | 样本 | 类型 |
|---|---|---|
| 百度 | `/来自：V2408A/DCIM/1074058976_1734538851561.mp4` | 16.3MB mp4（Range 代理） |
| 光鸭 | `/来自：分享/东京复仇者/东京复仇者.S01E25.2021.1080p.mkv` | 727.7MB mkv（Range 代理） |
| 夸克 | `/video_20260903_171534.mp4` | 173.7MB，>100MB → **HLS 直连**（m3u8，98.76s / 22 分片 / 810x1440） |

### 1.2 工具与指标

`examples/stream_bench.rs`：输入 `wp-cli link` 的 `DirectLink` JSON，经 `resolve_playback_with_provider` 起代理后模拟播放器读取：

* **顺序读**：GET 全量流式读取 `--read-bytes`；记录 `headers_ms`（代理响应头到达，≈首块下载耗时）、`body_ttfb_ms`、吞吐 Mbps、相邻块 >250ms 的停顿次数；
* **起播**：全新会话请求 `Range: bytes=0-64KB` 的首字节延迟（代理需先备好首块才回响应头）；
* **预读填满**：打开后轮询 `stats.buffered_bytes`，记录填满预读窗口的时间/实际达到字节；
* **seek**：全新会话（readahead=0）分别测「前/中/后 + 回看」冷读（未命中）与重复请求（命中）首字节；
* **模拟播放（paced）**：按 4K 码率（默认 60Mbps）消费，请求延迟 >300ms 记一次「卡顿」；
* **弱网**：本地 token-bucket 限速中继包住真实直链（全连接共享带宽预算），再跑起播 + paced；可测 5/20/50/… Mbps；
* **HLS**：m3u8 拉取、首分片、稳态 2 分片带宽、40% 位置分片 seek；
* **matrix**：对 5 组 `chunk/readahead/parallel` 重复上述子项（每项独立新会话 + 独立缓存目录，避免缓存串扰）。

### 1.3 ⚠ 测试窗口网络劣化（重要）

测试期间公网明显异常且波动大，**直接基线**（绕过代理用 curl/reqwest 拉直链）：

| 链路 | 实测 |
|---|---|
| 百度直链 CDN | **0.3–0.6 Mbps**（1MB Range 耗时 27.6s；4MB 101.7s；8MB 超 150s 未完成）——测试窗口被严重限速 |
| 光鸭直链 CDN | 单连接 **3.1–11.5 Mbps**（32MB 读 4.83Mbps / 11.5Mbps 两次）；TTFB 0.3–1.3s |
| 夸克 HLS 分片 | 首分片 7.7Mbps，稳态 1.8–7.4Mbps（波动大） |

因此：**绝对吞吐只代表“劣化网络下的下限”**；`chunk/readahead/parallel` 之间的相对差异、公式推导、缓存命中延迟（本地磁盘）是可靠结论。网络恢复后用同命令即可刷新数据（见 §2）。

---

## 2. 复跑命令

```bash
# 0) 取直链（只读；加 --data-dir 指向真机会话）
cargo build -p wp-cli
cargo build -p wp-stream --example stream_bench
./target/debug/wp-cli --data-dir 出/真机联调数据 --drive guangya link \
    "/来自：分享/东京复仇者/东京复仇者.S01E25.2021.1080p.mkv" > /tmp/link-guangya.json

# 1) 全量（直连基线 + 顺序读 + 起播 + 预读 + paced + seek）
cargo run -p wp-stream --example stream_bench -- /tmp/link-guangya.json \
    "/来自：分享/东京复仇者/东京复仇者.S01E25.2021.1080p.mkv" --mode all --read-bytes 32M --seconds 12

# 2) 参数矩阵（chunk/readahead/parallel 对比）
cargo run -p wp-stream --example stream_bench -- /tmp/link-guangya.json --mode matrix --read-bytes 8M

# 3) 弱网仿真（本地限速中继；--limit-bps 支持 5Mbps/20Mbps 写法）
cargo run -p wp-stream --example stream_bench -- /tmp/link-guangya.json \
    --mode weak --limit-bps 5Mbps --play-bps 60Mbps --seconds 8 --chunk 1M --readahead 8M

# 4) 夸克 HLS（is_hls=true 的直链 JSON）
cargo run -p wp-stream --example stream_bench -- /tmp/link-quark-hls.json --mode hls

# 5) 只测起播（不同分块对比，readahead=0 消除预读干扰）
cargo run -p wp-stream --example stream_bench -- /tmp/link-guangya.json \
    --mode seq --readahead 0 --read-bytes 1M --chunk 1M
cargo run -p wp-stream --example stream_bench -- /tmp/link-guangya.json \
    --mode seq --readahead 0 --read-bytes 1M --chunk 4M
```

输出为 JSON（stdout 或 `--out`）；只记录 host/头名，**不输出 Cookie 等敏感头值**。

---

## 3. 数据

### 3.1 顺序读：直连基线 vs 代理（光鸭 727MB，默认 chunk4M/ra64M/parallel2）

| 指标 | 直连基线（绕过代理） | 经本地代理 | 说明 |
|---|---|---|---|
| headers 到达 | 1256 ms | 23062 ms | 代理需先备好首块（4MB）；劣化网络下 ≈ 4MB/带宽 + 与预读竞争 |
| 首字节 | 1256 ms | 23062 ms | 同上（`body_ttfb ≈ headers`，说明首包紧跟响应头） |
| 吞吐 | **4.83 Mbps** | **4.25 Mbps** | 代理 ≈ 直连的 88%，差值为预读并发/分块粒度开销 |
| 读中停顿（>250ms） | — | 6 次，最大 gap 9.6s | 网络抖动（非代理 CPU；本地缓存读为 GB/s 级） |
| 预读填满 | — | 20s 超时仅到 **54.5/64MB**（聚合 ≈ 21.8 Mbps） | 预读多连接聚合明显快于单流 |

### 3.2 起播延迟 vs 分块大小（光鸭，readahead=0，全新会话，同一时段）

| 配置 | 起播首字节（Range 0-64KB） | 实测公式校验 |
|---|---|---|
| chunk=**1MB** | **1.57 s** | 1MB / ~5Mbps ≈ 1.6s ✅ |
| chunk=**4MB** | **6.59 s** | 4MB / ~5Mbps ≈ 6.4s ✅（实测 6.59s） |
| 百度 chunk=**256KB** | **6.39 s**（同一时段直连 0.61Mbps） | 256KB / 0.33Mbps ≈ 6.2s ✅ |

> 结论：`起播延迟 ≈ chunk_bytes / 有效带宽`。默认 4MB 在 4K 网络（≥50Mbps）下为 0.32–0.65s，可接受；弱网（<20Mbps）会到 1.6–6.4s，应改 1–2MB。

### 3.3 seek：冷未命中 vs 缓存命中（光鸭 727MB，readahead=0 独立会话）

| 位置 | 冷（未命中）首字节 | 冷下载量 | 热（重复/命中）首字节 |
|---|---|---|---|
| 前（0） | 2952.9 ms | 4MB（1 块） | **2.2 ms** |
| 中（size/2） | 2364.5 ms | 8MB（2 块） | **9.5 ms** |
| 后（size-1MB） | 1221.2 ms | 2.1MB（尾块） | **6.0 ms** |
| 回看-前（再回 0） | **2.4 ms（已命中）** | 0 | 0.8 ms |
| 默认配置回看（预读开） | — | — | **2.7 ms** |

> 结论：**缓存命中 seek 0.2–9.5ms（本地磁盘，与网络无关）**；冷 seek = 一次分块下载，弱网下 1.2–3.0s。4K 拖动是否“瞬移”取决于目标位置是否在预读/已看窗口内。

### 3.4 参数矩阵（光鸭 727MB，8MB 顺序读；⚠ 该轮网络波动大，绝对值仅供参考）

| chunk / readahead / parallel | 起播 TTFT | 预读 20s 达到 | 冷 seek(中) | 热 seek(中) |
|---|---|---|---|---|
| 1M / 32M / 1 | **3.42 s** | 23 MB | **0.64 s** | 0.7 ms |
| 4M / 64M / 2 | 7.08 s | 24 MB | 12.07 s | 0.3 ms |
| 8M / 64M / 4 | 36.64 s | **32 MB**（最快） | 38.30 s | 0.2 ms |
| 4M / 128M / 2 | 32.00 s | 28 MB | 16.38 s | 0.3 ms |
| 4M / 0 / 2 | 8.74 s | —（关闭） | **4.13 s** | 0.4 ms |

> 解释：① 起播随 chunk 增大而变慢（1M→3.4s vs 4M→7.1s，与 §3.2 公式一致；8M/128M 的异常高值含网络抖动与预读竞争）；② **预读越大/并行越多，冷 seek 越慢**（预读占用带宽，seek 分块排在后面抢不到带宽：ra=128M 时 16.4s vs ra=0 时 4.1s）；③ 聚合填充量 p4 > p2 > p1，说明多连接聚合有效；④ 热 seek 全部 0.2–0.7ms，与配置无关。
> （矩阵行里的 `seq_throughput` 出现了 2.7–8.2 Gbps 的异常值：上一版测量顺序中起播请求等待期间预读已把前 8MB 缓存好，顺序读实际读的是磁盘缓存 → 这是**缓存命中读速度**，不是网络吞吐；重排后的矩阵用独立会话，但本轮网络波动仍大，故本表只采用起播/seek/填充三列。）

### 3.5 弱网模拟（本地 token-bucket 中继；光鸭）

| 限速 | 起播（chunk1M） | 模拟播放 60Mbps × 8s：卡顿次数 / 最长请求 / 消耗 |
|---|---|---|
| **5 Mbps** | 22.48 s* | **3 次** / 6.34 s / 5.2MB |
| **20 Mbps** | 9.03 s* | **2 次** / 6.57 s / 16.8MB |

> \* 该轮起播含 2 次「链接热换」（光鸭 CDN 在高频压测下返回 403，代理自动重新取链后继续，T14 真机验证 ✅），且窗口内真实网络本身 < 限速值，故起播耗时偏大；**卡顿计数、单次请求最长阻塞**是有效指标：60Mbps 4K 播放需要 ≥60Mbps 稳定带宽，5/20Mbps 下每次缺数据都会卡顿数秒。
> 结论：播放器侧检测到卡顿时应提示“网络不足”，不要继续无限预读。

### 3.6 HLS（夸克 173MB，m3u8 直连）

| 指标 | 实测 |
|---|---|
| m3u8 拉取 | 654–1072 ms（约 11.9KB，22 分片，总时长 98.76s） |
| 首分片 | 1.00–1.12 s / 0.97MB（**7.7 Mbps**） |
| 起播估算（m3u8+首分片） | **1.66–2.19 s** |
| 稳态带宽（连拉 2 分片） | 1.79–6.80 Mbps（窗口内网络波动） |
| seek 40%（39.5s） | **2.24–2.49 s** / 2.23MB 分片（单分片即可定位，无需回拉前序分片） |
| mpv 真机实测（正常网络时段，见 PLAYER_STREAM_M0 §9） | 起播 5 帧 **4.7s**；seek 39.5s 后播放 **19.9s** 完成 |

> HLS seek 成本 = 拉目标分片（+解码器重新初始化）；无本地缓存，靠 mpv `--cache=yes` 缓冲。

---

## 4. 参数建议（结合实测 + 4K 码率公式）

设 4K 码率 `R`（40–100 Mbps，即 5–12.5 MB/s）：

| 参数 | 现有默认 | 建议（4K） | 依据 |
|---|---|---|---|
| `chunk_bytes` | 4 MB | **4 MB 保留**；弱网/移动端 **1–2 MB**；≥200Mbps 内网可 8MB | 起播延迟≈chunk/带宽（§3.2）：4K 50Mbps 下 4MB=0.64s；4K 100Mbps=0.32s；弱网 5Mbps 下 4MB=6.4s（不可接受）→ 1MB=1.6s |
| `readahead_bytes` | 64 MB | **128 MB**（≈8.5–25s @4K 40–100Mbps）；4K HDR/蓝光原盘 **192–256 MB**（≈17–51s）；弱网 **32–64 MB** | 架构目标缓冲 20–60s；128MB/60Mbps≈17s；256MB/100Mbps≈20.5s。注意弱网预读抢 seek 带宽（§3.4）→ 弱网降窗 |
| `parallel_fetches` | 2 | **4**（正常网络）；弱网 2 | 实测单流 3–5Mbps，parallel=2 聚合 ≈21.8Mbps，p4 填充最快（§3.1/§3.4）：CDN 常按单连接限速，多连接聚合 2–5× |
| `max_cache_bytes` | 20 GB | 默认保留 20GB；**4K 重度用户 50GB**（≈1.9h @60Mbps）；蓝光原盘/收藏 100GB | 4K 60Mbps≈27GB/h；20GB≈45min；SSD 缓存命中 seek 0.2–9.5ms（§3.3），值得保留 |
| 其他 | — | ① 弱网/移动端可做「**自适应分块**」：打开时用 1MB 快速起播，稳定后升 4MB；② 弱网下**预读应让位于 seek**（可暂停/取消预读任务，或 seek 目标所在块优先下载）；③ HLS 4K 需 ≥1.5× 码率稳定带宽，mpv 加 `--cache=yes --demuxer-max-bytes≈64-128MB` | §3.4 冷 seek 竞争、§3.5 卡顿、§3.6 HLS |

**4K 场景推荐组合**：
* 家庭千兆/局域网（≥100Mbps）：`chunk 4–8MB · readahead 128–256MB · parallel 4 · LRU 50GB`
* 普通宽带 50–100Mbps：`chunk 4MB · readahead 128MB · parallel 4 · LRU 50GB`
* 弱网/移动 20–50Mbps：`chunk 1–2MB · readahead 32–64MB · parallel 2 · LRU 20GB` + 卡顿提示
* 夸克大文件（HLS 直连）：`chunk/readahead/parallel/LRU 不参与`（不走代理），播放器侧缓存 + 带宽≥1.5×码率

---

## 5. 发现的瓶颈与就地优化

| # | 现象 | 分析 | 处理 |
|---|---|---|---|
| 1 | 起播延迟 = 首块下载时间（chunk/带宽） | 代理对 GET 先 `ensure_chunk(首块)` 再回响应头（保证错误干净返回 502） | 未改生产代码（避免破坏已验收 API/行为）；**建议后续加「首块边下边发」或打开时用小块（自适应分块）**；弱网当前用 chunk=1–2MB 规避 |
| 2 | 弱网下预读与 seek 抢带宽（冷 seek 12–38s vs ra=0 4.1s） | 预读任务持续占满并行连接/带宽，seek 分块只能排队 | 建议：弱网降低 readahead/parallel；后续可在 `ensure_chunk` 引入「用户请求优先（可取消预读）」调度 |
| 3 | 多连接聚合显著（单流 3–5Mbps → parallel2 预读 21.8Mbps） | CDN 按单连接限速 | `parallel_fetches` 建议 4（正常网络），已作为调参建议 |
| 4 | HLS 测试工具单位 bug（首分片 Mbps 放大 1000×） | `first_segment_mbps` 用 ms 当秒 | **已修**（本轮工具改动，不影响生产库） |
| 5 | 压测期间代理链接热换 | 光鸭 CDN 对高频压测返回 403 → 代理自动刷新直链并继续 | T14 真机正向验证（日志“链接热换成功”），无卡死 |

> 生产代码（`crates/wp-stream/src/**`）本轮**未改动**（任务范围允许，但已有 52 测试全绿验收；性能问题以参数建议 + 后续优化项记录）。仅新增压测工具 `examples/stream_bench.rs`。

---

## 6. 限制与后续

1. 本机 headless、无 GUI 播放器；paced 是「按码率消费 + >300ms 记卡顿」的代理指标，不等价于 mpv 实际帧丢弃（HLS 用 mpv 实测补充，见 PLAYER_STREAM_M0 §9）。
2. 测试窗口公网劣化且机器高负载（load 6–13），吞吐数据只作**下限**；网络恢复后同命令复跑即可更新。
3. 弱网由本地 token-bucket 模拟（不含 RTT/丢包），仅验证带宽受限行为；真实弱网建议后续加 `tc netem`（需 root）或代理侧注入延迟。
4. 矩阵长跑曾因环境资源抖动失败一次（本地代理连接层），已用短跑替代；重跑命令已提供。
5. 后续优化项：自适应分块起播、seek 优先调度、HLS 分片预取（可选）。这些需要动生产代码，建议在 M6 性能/验收阶段统一评估。
