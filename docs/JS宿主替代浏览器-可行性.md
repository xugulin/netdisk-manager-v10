# 用 TinyJS 封装替代浏览器做网盘登录：可行性判定

> 结论先行：**TinyJS 这一层不行；「自研 JS 宿主」这个方向只在一半成立。**
> 登录凭据（BDUSS / STOKEN / `__kps` / `__puus`）是服务端 `Set-Cookie` 发的，
> 不是 JS 算出来的 —— 换 JS 引擎不会多出任何一个字节的登录信息。
> JS 宿主真正能替代浏览器的地方是**请求签名 / 参数生成**，不是登录本身。

---

## 0. 先把「浏览器」拆开：它在这条链路里到底干了 5 件事

| # | 浏览器职责 | 本项目的现状 | 缺了会怎样 |
|---|---|---|---|
| 1 | DNS / TLS / HTTP2 / 重定向链 | 已有：`reqwest(rustls)`，`wp-net` | —— |
| 2 | Cookie 容器（domain/path/过期/`Set-Cookie` 合并） | 已有但是**简化版**：`wp-net/src/cookie.rs:3` 明确「不需要浏览器的 domain/path 语义」，只做有序 map | 跨子域种植（passport → pan）时会丢域信息，见 §3 |
| 3 | HTML 解析 + DOM + 事件 + 表单提交 | 无 | 登录页的「短信登录」是风控组件，点不到就进不去 |
| 4 | JS 运行时（ES2020 + webpack bundle + 定时器/微任务） | 无 | 登录页 JS 跑不起来；签名函数算不出 |
| 5 | 指纹面（canvas/WebGL/Audio/字体/时区/UA-CH/`dfp`） | 无 | 服务端判定「非浏览器」，直接风控 |

`browser_login.rs` 现在走的是「借一台真浏览器」：启动本机 Chrome/Edge，
用户在网页里正常登录，程序用 CDP `Network.getAllCookies` 把会话取回来
（`crates/wp-gui/src/browser_login.rs:11`、`:497`）。
它替代的其实只有第 3、4、5 项，第 1、2 项一直是 Rust 自己做的。

**所以要问的不是「TinyJS 能不能替代浏览器」，而是「第 3/4/5 项里，哪几项值得自研」。**

---

## 1. TinyJS 的能力边界：连第 4 项的门都进不去

TinyJS（gfwilliams/tiny-js）的定位是 **MCU 上的表达式/逻辑脚本**，约 2k 行 C++，
ES5 的一个小子集。对照登录页的真实要求：

| 登录页需要 | TinyJS | 后果 |
|---|---|---|
| ES2018+ 语法（`let/const`、箭头函数、可选链、解构、class、模板串） | 只有 `var` + 函数/对象/数组 | 现代 bundle **parse 阶段就挂**，不是运行出错，是根本读不进去 |
| 事件循环 / `Promise` / `async` / `queueMicrotask` | 无 | `await fetch(...)` 这类登录流一句都跑不了 |
| DOM / `document.cookie` / 事件 / `postMessage` / 跨域 iframe | 无 | 风控组件是 iframe + postMessage 握手，缺一个就卡在「请拖动滑块」 |
| `XMLHttpRequest` / `fetch` | 无（要自己写宿主绑定） | 等价于「用 C++ 重写一遍 fetch」，那还不如直接用 Rust 的 reqwest |
| `TextEncoder/Decoder`、UTF-16 语义、`String.length` 按码元 | 字符串是 UTF-8 `char*` | 中文登录页文案、`btoa`、签名里的中文参数全部错位 |
| `WebCrypto`（`crypto.subtle.digest/sign`） | 无 | 前端算 token 的地方直接断 |
| `ArrayBuffer` / `TypedArray` | 无 | 二进制签名、SHA 中间态处理没法做 |
| 完整 `try/catch`、getter/setter、`Proxy` | 子集/缺失 | 风控 SDK 的环境探测（`Object.defineProperty` 反调试）跑不通 |
| 数字模型 | 统一 double | 位运算签名（`>>>`、32 位截断）结果与浏览器不一致 |

结论：**TinyJS 连「把登录页 JS 读进来」这一步都过不了**，谈不上替代浏览器。
它的体积优势（~100 KB）在这里没有意义，因为省下的体积换不来任何一项能力。

---

## 2. 因果纠正：登录信息不是「逆向 JS」得到的

三家网盘的凭据来源（本仓库实测口径）：

| 网盘 | 凭据 | 来源 | JS 参与度 |
|---|---|---|---|
| 百度 | `BDUSS` | `passport.baidu.com` 的 `Set-Cookie` | 0（扫码/密码都由服务端下发） |
| 百度 | `STOKEN`（pan 域，写权限的钥匙） | 访问 `pan.baidu.com/disk/main` 时由服务端下发 | 0，但**只有真网页版会话才给**（`网盘管理/docs/实验_纯HTTP登录.md:11-12`、`:67`） |
| 夸克 | `__kps` / `__uid` / `__puus` | `pan.quark.cn/account/info` 重定向链上的 `Set-Cookie` | 0（`wp-drive-quark/src/auth.rs:1-12` 已按此实现） |
| 光鸭 | `access_token` / `refresh_token` | 登录接口 JSON body | 0（`wp-drive-guangya/src/auth.rs`） |

`document.cookie` 读到的也是这张 cookie jar —— 换成 JS 宿主来读，读到的还是同一批
`Set-Cookie`。所以「封装 JS 引擎 → 逆向获取登录信息」这条因果是反的：

- **能不能拿到凭据** = 会话链对不对（域、重定向、`Referer`、时序）+ 风控放不放行；
- **JS 引擎只影响**：请求参数/签名算不算得出来。

而签名这块，本项目已经用 Rust 手写完了（`wp-net/src/crypto.rs:3-7`：MD5/SHA1/SHA256/HMAC，
光鸭 OSS V1 + 埋点签名在 `wp-drive-guangya/src/sign.rs`），**已经在跑的代码不需要 JS 引擎**。

---

## 3. 那「自研 JS 宿主」还有价值吗？有，但只在 L1

推荐三层，按「是否必须真浏览器」切分：

| 层 | 组件 | 覆盖 | 体积 | 失败模式 |
|---|---|---|---|---|
| **L0** | Rust `reqwest` + `CookieJar` | 扫码登录（百度/夸克）、Cookie 导入、光鸭短信/令牌、全部业务 API | 0（已有） | 风控组件、滑块、`dfp` 校验 |
| **L1** | **迷你 JS 宿主**（QuickJS / rquickjs，或纯 Rust 的 Boa） | 跑**纯函数**：`sign`、`w_rid`、`dfp` 片段、JSONP 解包、设备参数生成 | ~1.5 MB | 依赖 DOM/事件的代码；环境指纹不一致 |
| **L2** | 本机 Chrome + CDP（已有） | 短信登录、滑块、设备指纹、跨域 iframe 握手 | 209 MB（外置，不计入包体） | 用户没装浏览器 |

**关键判据（决策矩阵）**：

```
接口只认 Cookie + 一个可离线复现的签名         → L1 够（甚至 L0 手写 Rust 就够）
接口校验 dfp / _bsi / ARMOR token / 滑块轨迹     → 必须 L2，宿主再强也没用
登录动作本身（发短信、验码、确认授权）           → 必须 L2 或人工
```

百度短信登录就是第三类：`browser_login.rs:4` 写明「登录页里的风控组件（ARMOR / 跨域
iframe / 设备指纹），老接口 `?getsmscode` 已 404、`?regphonesend` 回 200 但不真发短信」。
这不是 JS 引擎能力问题，是**服务端只在真浏览器会话里发短信**。

---

## 4. L1 最小实现骨架（rquickjs）

用途限定：**只跑签名/参数函数**，不加载页面、不执行 bundle。
依赖：`rquickjs = { version = "0.9", features = ["full-async"] }`（需联网拉取 + `cc` 编译，
本机 `cc/gcc/clang` 已具备）。

```rust
// crates/wp-net/src/jshost.rs（新增，约 120 行）
use rquickjs::{AsyncContext, AsyncRuntime, Function, Object, Ctx, Result as JsResult};
use std::sync::Arc;
use crate::cookie::CookieJar;

pub struct JsHost {
    rt: AsyncRuntime,
    ctx: AsyncContext,
    jar: Arc<tokio::sync::Mutex<CookieJar>>,
}

impl JsHost {
    pub async fn new(jar: Arc<tokio::sync::Mutex<CookieJar>>) -> JsResult<Self> {
        let rt = AsyncRuntime::new()?;
        let ctx = AsyncContext::full(&rt).await?;
        let host = Self { rt, ctx, jar };
        host.install_globals().await?;
        Ok(host)
    }

    /// 装一套「够签名函数用」的假浏览器：navigator/screen/location/crypto/setTimeout。
    async fn install_globals(&self) -> JsResult<()> {
        let jar = self.jar.clone();
        self.ctx.with(|ctx| {
            let g = ctx.globals();

            // navigator：只补签名函数真的会读的字段（UA 由调用方注入，不写死）
            let nav = Object::new(ctx.clone())?;
            nav.set("userAgent", js_ua())?;
            nav.set("platform", "Win32")?;
            nav.set("language", "zh-CN")?;
            nav.set("hardwareConcurrency", 8)?;
            g.set("navigator", nav)?;

            // document.cookie ←→ 共享 CookieJar（读写都走同一张表）
            let doc = Object::new(ctx.clone())?;
            let j1 = jar.clone();
            doc.set("cookie", Function::new(ctx.clone(), move || -> String {
                // 同步读快照：宿主只用来算签名，读一致性够用
                j1.try_lock().map(|j| j.header()).unwrap_or_default()
            })?)?;
            g.set("document", doc)?;

            // crypto.subtle.digest → Rust（避免在 JS 里重写 SHA）
            let subtle = Object::new(ctx.clone())?;
            subtle.set("digest", Function::new(ctx.clone(), |algo: String, data: Vec<u8>| {
                let hex = match algo.to_ascii_uppercase().as_str() {
                    "SHA-256" => crate::crypto::sha256_hex(&data),
                    "SHA-1"   => crate::crypto::sha1_hex(&data),
                    _         => crate::crypto::md5_hex(&data),
                };
                hex
            })?)?;
            let crypto = Object::new(ctx.clone())?;
            crypto.set("subtle", subtle)?;
            g.set("crypto", crypto)?;

            // 环境标记：让脚本能判断「我跑在宿主里」，便于我们自己的探针分支
            g.set("__WP_JS_HOST__", "rquickjs/0.9")?;
            Ok(())
        }).await
    }

    /// 跑一段脚本，返回字符串结果（签名/参数）。
    pub async fn eval_string(&self, code: &str) -> JsResult<String> {
        self.ctx.with(|ctx| ctx.eval::<String, _>(code)).await
    }
}

fn js_ua() -> &'static str {
    // 与 L2 浏览器保持一致，否则同一套参数两天两个结果
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"
}
```

配套的**宿主探针**（用于 §5 的 E2 对齐实验）：

```js
// probe.js —— 只依赖上面装好的全局，验证签名函数是否可离线复现
const sign = (params) => {
  const s = Object.keys(params).sort().map(k => `${k}=${params[k]}`).join('&');
  return crypto.subtle.digest('SHA-256', s);   // 真页面里可能是 md5/自定义
};
globalThis.__probe = (json) => sign(JSON.parse(json));
```

---

## 5. 三个对照实验（决定值不值得做 L1）

| 实验 | 做法 | 通过标准 | 不通过就 |
|---|---|---|---|
| **E1 语法可跑性** | 把登录页 bundle（`passport.baidu.com/v2/?login` 的 `static/js/*.js`）原样丢进宿主 `eval` | 能 parse 且不抛 `SyntaxError` | 放弃「整包跑」路线，只做**单函数抽取** |
| **E2 签名对齐** | 固定一组参数，① 宿主里算 ② CDP `Runtime.evaluate` 在真页面里算，逐字节比对 | 两次结果完全一致（含大小写、编码） | 说明依赖了 DOM/指纹，退回 Rust 移植该函数 |
| **E3 端到端** | 宿主按抓包顺序重放登录请求（含 `Referer`/`Origin`/时序），看 `Set-Cookie` | 出现 `.pan.baidu.com` 的 `STOKEN`，且写探针 `errno:0` | 保留 L2；L1 只做业务 API 签名 |

E3 的判据直接沿用老项目那张表（`网盘管理/docs/实验_纯HTTP登录.md:66-67`）：
> 出现 `.pan.baidu.com` 的 STOKEN 且写探针 `errno:0` → 纯 HTTP 路线成立；
> 只有 BDUSS 没有 STOKEN → **只有真浏览器渲染才给写权限**，止损。

**注意**：那张表当时是用**假账号干的跑**（同文件「干跑结果」一节），
真账号结论仍未跑。E3 之前先补这一次真机实验，否则会在错误前提上做架构决策。

---

## 6. 成本对照

| 方案 | 包体增量 | 工程量 | 能替代浏览器哪部分 | 风险 |
|---|---|---|---|---|
| TinyJS | +0.1 MB | 高（宿主绑定全自己写） | 只到「表达式求值」 | 连 parse 都过不去，等于白做 |
| QuickJS 宿主（L1） | +1.5 MB | 中（3~5 人日：桥 + 探针 + 对齐） | 签名/参数/JSONP | 指纹不一致 → 仍被风控 |
| Boa（纯 Rust） | +2~3 MB | 中 | 同上 | 生态较新，部分 API 缺失 |
| 本机 Chrome + CDP（L2） | +0（外置） | 已完成 | 全部（含风控 UI） | 用户没装浏览器 / 沙箱环境 |
| 内置 WebView（CEF/WebView2） | +100~209 MB | 高 | 全部 | 包体、签名、许可 |

**推荐**：保留 L2 作为「登录唯一通道」，L1 只在出现「接口签名挡住了业务 API」时按需引入，
且**一个函数一个函数地移植成 Rust**，不要整包跑 bundle。

---

## 7. TinyJS 唯一合适的落点

不碰登录，只做**用户可编辑的小规则**：重命名模板、过滤表达式、批量任务条件。
这类场景要的是「安全求值 + 极小体积」，正好是 TinyJS 的设计目标；
把它塞进 `wp-transfer` 的命名/过滤管线，比塞进登录链路合理得多。

---

## 8. 一句话回答原始问题

> 用 TinyJS 封装替代浏览器登录网盘 —— **做不到**，卡在语言子集与 DOM/事件/指纹三道门；
> 换成 QuickJS 也只能替代「签名计算」，替代不了「登录」；
> 而登录信息本来就不在 JS 里，在 `Set-Cookie` 里 —— 那部分 `wp-net` 已经在做了。

---

# 实测记录（2026-10-27，本机）

## 已落地代码

| 位置 | 作用 |
|---|---|
| `crates/wp-net/src/jshost.rs` | L1 宿主：QuickJS + 浏览器外壳（navigator/screen/location/document.cookie/crypto/btoa）+ 与 `CookieJar` 共享的 cookie 桥 |
| `crates/wp-net/examples/js_probe.rs` | 探针：`eval` / `parse`（E1）/ `run` / `call`（E2） |
| `scripts/js-host-parity.sh` | E2-parity：宿主 vs node(V8) 纯函数逐字节比对 |
| 依赖 | `rquickjs 0.14`（`full-async`），`wp-net` 的 `js-host` feature，默认不编译 → 包体零增量 |

## E1：真实 bundle 能不能进宿主

样本取自 `passport.baidu.com/v2/?login` 页面引用的两个脚本：

| 文件 | 大小 | SHA-256（前 16） | `parse`（语法） | `run`（执行） |
|---|---|---|---|---|
| `boot_login_1e30d1a.js` | 130,424 B | `cdcccf6b95663f0e` | **OK，38 ms** | `TypeError: not a function` @1:8206 |
| `core.js` | 114,384 B | `081a70b85325e47f` | **OK，28 ms** | `ReferenceError: rTag is not defined` @1:9896 |

结论：

1. **语法层不是瓶颈**——两个 100KB+ 的真实 bundle，QuickJS 都直接编译通过（TinyJS 在这一步就会挂）。
2. **执行层才是分界线**：`rTag` 不在任何一个 bundle 里，由页面内联脚本提供；
   `boot_login` 还依赖页面里真实存在的 DOM 方法。**整包跑 = 需要页面上下文 = 需要浏览器**。
3. 所以 L1 的边界被实测钉死：**只跑抽出来的纯函数，不跑整包**。

## E2-parity：宿主与 V8 的纯函数一致性

`scripts/js-host-parity.sh`：20 条用例（字符串/UTF-8/正则/位运算/JSON/异常类型/数组方法），
宿主与 node v26（V8）逐字节比对 → **PASS=20 FAIL=0**。

这条只证明「纯函数这一层两个引擎无差别」，**不**证明站点签名已对齐；
站点签名仍需 E2 真值（浏览器 CDP 取一次输出，宿主复算比对）。

## 还没做的（需要真账号 / 真浏览器）

| 编号 | 内容 | 卡点 |
|---|---|---|
| E2 真值 | 从 bundle 里抽出签名函数，CDP 在真页面取一次输出 | 需定位函数 + 起浏览器 |
| E3 | 纯 HTTP + L1 重放登录链，看 pan 域 `STOKEN` 与写探针 `errno` | 需真账号；老项目那次是假账号干跑 |

## T6：包体实测（release, lto=thin, strip）

| 产物 | 内容 | 字节 |
|---|---|---|
| `size_baseline` | 只链 `wp-net` 的 crypto（无 rquickjs） | 347,912 |
| `js_probe` | 同上 + `js-host` feature（QuickJS + 宿主胶水） | 2,000,008 |

**增量 ≈ 1.65 MB**。对照内嵌引擎基线 209 MB ⇒ 省 ≈ 207 MB（99.2%）。

说明：两者都是 example 二进制，GUI 主程序（27 MB）接入后增量应在同一量级；
精确值等 T7 把 feature 接进 `wp-gui` 后再测一次。

---

# 复核修正（2026-10-27 深夜）：浏览器到底是不是登录的必需品？

**（本节结论在当晚被真机日志推翻一次，已按最终证据改回，见下方「❌ 更正」）**

## 三家网盘的登录通道 vs 浏览器依赖

| 网盘 | 通道 | 是否要浏览器 | 凭证 |
|---|---|---|---|
| 百度 | 扫码登录 | **不要**（纯 HTTP 轮询） | `BDUSS` + 手动跟随 302 拿到的 pan 域 `STOKEN` |
| 百度 | Cookie 导入 | 不要 | 同上 |
| 百度 | 短信登录 | 要（网页风控组件） | 由浏览器会话导出 |
| 夸克 | 扫码登录 | **不要** | `__kps/__uid/__puus` |
| 夸克 | Cookie 导入 | 不要 | 同上 |
| 光鸭 | 设备码 / 短信 / 令牌导入 | **不要**（三通道全 HTTP） | `access/refresh token` |

## ❌ 更正：手动跟随 302 也拿不到 pan 域 `STOKEN`

`crates/wp-drive-baidu/src/auth.rs:397-470` 确实实现了「手动跟随 302、逐跳取 `Location` 的 `stoken=`」，
但**扫码会话走到这个接口会被服务端拒绝升级**：

* 真机日志 `出/网盘管理-linux-x64/数据/日志/运行-20261020.log:58-73`：
  `pan 域会话建立完成：STOKEN 64 → 64（12 个 Set-Cookie；捕获 BAIDUID/…）`
  → 紧接着 `gettemplatevariable errno=-6` → `当前会话只读`；
* `运行-20261019.log:61-75` 复现同一现象（扫码成功后写接口依旧 -6）；
* 基座 2026-10-18 就登记过根因（踩坑总集「百度扫码登录会话只读：passport域STOKEN与pan域STOKEN权限差异」）：
  `passport /v3/login/api/auth/?return_type=5` 对扫码会话 **302 到 wappass（notjump=1）拒绝**，
  所以 QR 登录无法自行升级为可写会话；pan 域 128 字符 STOKEN 只存在于**网页版登录态**。

**最终结论：百度写权限的唯一已知来源是浏览器网页登录会话**（或用户从浏览器导出 Cookie 导入）。
「手动跟随 302」解决的是「响应头不被丢」，解决不了「服务端不给」。

> 教训：这轮曾据代码注释把文档改成「纯 HTTP 可拿」，被真机日志推翻 ——
> 改动前应先查基座（`jizuo_exp_find`），注释与真机日志冲突时以日志为准。

## 结论调整

1. **209MB 引擎从始至终不是登录的必需品**；它是「短信登录便利性」的代价，不是能力门槛。
2. 真正要做的是「浏览器缺失时不要卡死」——已在 `crates/wp-gui/src/app.rs` 的
   `BrowserLoginEvent::Failed` 分支加自动回落：提示没浏览器 → 自动切到扫码标签并发起二维码。
3. L1（QuickJS 宿主）因此**没有登录侧消费者**：签名全在 Rust 里，整包 JS 又跑不了（E1 已证）。
   它的价值改为「以后出现 JS 签名接口时的工具箱」，默认不编译、包体零增量。
4. **写权限回落**（Round 2 新增）：百度扫码成功后若是只读会话，GUI 自动打开浏览器登录（CDP）
   拿可写 Cookie —— 这条才是目标里「登录失败自动回落 CDP」的真实落点
   （`crates/wp-gui/src/app.rs` 的 `Event::LoggedIn` 只读分支）。

## E3 预演（不需要账号的那一半，已实跑）

工具：`crates/wp-drive-baidu/examples/qr_probe.rs`

```bash
cargo run -p wp-drive-baidu --example baidu_qr_probe -- /tmp/wp-qr-probe            # 只读
cargo run -p wp-drive-baidu --example baidu_qr_probe -- /tmp/wp-qr-probe --write-probe  # 建目录再删
```

实跑输出（2026-10-27 23:49，本机）：

```
数据目录：/tmp/wp-qr-probe
浏览器：不使用；JS 引擎：不使用（全程 Rust HTTP）
[信息 百度] 已获取二维码：989 字节
二维码 URL：https://passport.baidu.com/v2/api/qrcode?sign=7cdf566de816dc3b49ff742a22a62ec1&lp=pc&qrloginfrom=pc
二维码 PNG：/tmp/wp-qr-probe/qr.png（989 字节）
等待扫码，最长 300 秒…
```

**这一半已经证明**：登录流程的起点（取 sign、下二维码、轮询）全程 Rust HTTP，
不启动浏览器、不加载 JS 引擎。

剩下的一半（换成 BDUSS → 建 pan 会话 → 拿 128 字符 STOKEN → 写探针 errno:0）
需要真账号扫码，属于唯一卡在「用户输入」的步骤；`--write-probe` 会在成功后
建一个 `/__wp_probe_<ts>` 目录并立刻删除，用来判定会话是否真的可写。

## T7 GUI 接入（已完成）

| 改动 | 位置 |
|---|---|
| `js-host` 可选 feature（默认不编译，包体零增量） | `crates/wp-gui/Cargo.toml` |
| 「关于 → 运行环境」新增一行 **JS 宿主（L1）**，首次渲染时真起一个 QuickJS 自检 `1+1` | `crates/wp-gui/src/pages/about.rs` |
| 浏览器缺失时**自动回落扫码**（原来只 toast 报错） | `crates/wp-gui/src/app.rs`（`BrowserLoginEvent::Failed`） |
| 冒烟测试：GUI 二进制里宿主可跑 | `about.rs::js_host_tests` |

验证：`cargo test -p wp-gui --features js-host` → **49 passed, 0 failed**（含宿主冒烟 1 条）。

## T7c GUI 真实包体增量（release, lto=thin, strip, codegen-units=1）

| 构建 | 主程序字节 | 说明 |
|---|---|---|
| `cargo build --release -p wp-gui` | 27,314,192 | 默认，不含 JS 宿主 |
| 同上 `--features js-host` | 28,778,944 | 含 QuickJS 宿主 |
| **增量** | **+1,464,752 B ≈ +1.40 MiB** | 对照内嵌引擎 209 MB ⇒ 省 ≈ 207.6 MB（99.3%） |

结论：`js-host` 作为可选 feature 时，包体代价 ≈ 1.4 MiB，远低于目标上限 2 MB；
默认构建零增量。

## 本轮状态小结

| 目标项 | 状态 | 证据 |
|---|---|---|
| 登录（读/浏览/下载）不依赖浏览器 | ✅ 三家均纯 HTTP | `qr_probe` 实跑；三家适配器无 CDP 依赖 |
| 百度写操作不依赖浏览器 | ❌ 不成立（需网页版登录态） | 真机日志 20261020:58-73；基座坑条目 |
| 扫码只读 → 自动回落浏览器登录 | ✅ 已实现 | `app.rs` 只读分支 + 48 测试 |
| L1 宿主落地 | ✅ 默认不编译，可选用 | 7 单测 + GUI 冒烟 + E2 20/20 |
| 包体增量 ~2MB | ✅ +1.40 MiB | 上表 |
| 登录失败自动回落 CDP | ✅ 反向回落：缺浏览器→自动扫码 | `app.rs` 补丁 + 49 测试 |

---

# 方案 B 落地：系统 WebView 登录（2026-10-28）

目标里「不依赖外部浏览器」的正解：不再借用户装的 Chrome + CDP，改用**系统 WebKitGTK** 开登录窗口，
进程内直接读 Cookie。包体几乎不增（WebKitGTK 是发行版系统库）。

## 新增 crate：`wp-weblogin`

| 文件 | 作用 |
|---|---|
| [`crates/wp-weblogin/src/lib.rs`](../crates/wp-weblogin/src/lib.rs) | GTK 窗口 + `WebViewBuilderExtUnix::build_gtk`；`gtk::glib::timeout_add_local` 轮询 `webview.cookies()`，凑齐必需键才回传 |
| [`examples/probe.rs`](../crates/wp-weblogin/examples/probe.rs) | 端到端探针：本地 HTTP 页 Set-Cookie → 断言取回 `BDUSS`+`STOKEN` |
| `collect_header` / `base_domain` | 纯函数（域名后缀过滤 + 按名排序拼接），3 项单测 |

**平台边界**：Linux 真实现；Windows 编译成「不支持」桩（tao/GTK 的 EventLoop 要求主线程，与 eframe 冲突），
Windows 继续走外部浏览器 + CDP。

## 端到端实测（无头 Xvfb，2026-10-28）

```bash
xvfb-run -a env GDK_BACKEND=x11 WEBKIT_DISABLE_COMPOSITING_MODE=1 \
  WEBKIT_DISABLE_DMABUF_RENDERER=1 cargo run -p wp-weblogin --example probe
```

```
测试页：http://127.0.0.1:35377/
事件：Started("已打开系统 WebView：http://127.0.0.1:35377/")
事件：Cookies("BDUSS=probe_bduss; STOKEN=probe_stoken")
RESULT: PASS（系统 WebView 取到完整 Cookie，不需要外部浏览器）
```

## GUI 接线

| 改动 | 说明 |
|---|---|
| `wp-gui` feature `webview-login` | 默认不开、零增量；`= ["dep:wp-weblogin"]` |
| `browser_login::run` | feature 打开时优先走系统 WebView；`WP_LOGIN_FORCE_EXTERNAL_BROWSER=1` 可强制旧通道 |
| 登录框文案 | 标签「🌐 浏览器」→「🪟 系统窗口」、按钮「打开系统登录窗口」、说明去掉「需装 Chrome」 |
| `scripts/build-linux.sh` | Linux 构建默认 `--features webview-login`；`WP_NO_WEBVIEW=1` 可关 |

测试：默认构建 48 passed、feature 构建 48 passed（含 `wp-weblogin` 3 项单测）。

## 还差什么

1. **真实桌面验证**：无头 Xvfb 已验证通道可用；但「GUI 主进程里再起 GTK 线程」在真实
   Wayland/X11 桌面的表现需要点一次（若 GTK 报线程问题，改「子进程 helper」模式即可彻底隔离）。
2. **写权限闭环**：系统 WebView 里正常登录会拿到网页版会话（含 pan 域 128 字符 STOKEN），
   理论上 `errno:-6` 问题随之消失 —— 需真账号点一次验证。

## 包体实测（release + LTO + strip，2026-10-28）

| 构建 | 主程序字节 | 说明 |
|---|---|---|
| 默认 | 27,314,192 | 无 JS 宿主、无系统 WebView |
| `--features webview-login` | 27,533,456 | 含系统 WebView 登录通道 |
| **增量** | **+219,264 B ≈ +214 KiB** | 对照内嵌引擎 209 MB ⇒ 省 ≈ 208.8 MB |

原因：WebKitGTK 是**系统动态库**，二进制里只有 wry 胶水与 GTK 绑定；
内嵌 CEF/WebView2 才会把 200MB+ 打进包里。

## 进程内验证：GUI 主进程里起 GTK WebView（2026-10-28）

上一节留的风险是「GUI 主进程里再起 GTK 线程会不会出事」。用验收钩子在无头 Xvfb 下实测通过：

```bash
# 构建：acceptance（自动化钩子）+ webview-login
cargo build -p wp-gui --features acceptance,webview-login

# 运行：自动打开登录框 → Browser 标签 → 自动点「打开系统登录窗口」
xvfb-run -a env GDK_BACKEND=x11 WEBKIT_DISABLE_COMPOSITING_MODE=1 \
  WEBKIT_DISABLE_DMABUF_RENDERER=1 LIBGL_ALWAYS_SOFTWARE=1 \
  WP_DATA_DIR=/tmp/wp-wv-test WP_DEMO_LOGIN_TAB=browser WP_DEMO_BROWSER_LOGIN=1 \
  ./target/debug/网盘管理 --dialog login --drive baidu --frames 600
```

日志（应用日志文件）：

```
[登录] 系统 WebView 通道：准备打开登录窗口 https://pan.baidu.com/
[界面] 已打开浏览器：请在网页里完成登录（短信/扫码/密码均可），完成后自动导入
```

判据与结论：

| 判据 | 结果 |
|---|---|
| 本机是否装了 Chrome/Edge/Chromium | **没有**（`command -v google-chrome/chromium` 全空）⇒ 不可能是外部浏览器通道 |
| 是否走到系统 WebView 分支 | ✅ 日志出现「系统 WebView 通道：准备打开登录窗口」 |
| 主进程是否因 GTK 线程崩溃 | ❌ 无 panic / GLib 错误，进程正常退出（exit 0），截图正常落盘 |
| 窗口内是否真加载了登录页 | ✅ 目标 URL 为 `https://pan.baidu.com/`（窗口建于 GTK 主循环内） |

**结论：在 eframe/winit 主进程里另起 GTK 线程是可行的（X11 环境）**；
Wayland 真桌面仍建议人工点一次确认。

## 部署注意（新增）

- 构建机：`libwebkit2gtk-4.1-dev`（pkg-config `webkit2gtk-4.1`）；
- 运行机：`webkit2gtk-4.1` 运行库（GNOME 桌面默认自带）；
- 不想要这个依赖：`WP_NO_WEBVIEW=1 ./scripts/build-linux.sh`，程序自动回落外部浏览器通道；
- `scripts/install-desktop.sh` 安装后会跑 `ldd` 自检，缺库时直接给出包管理器命令。

---

# 最终决策（2026-10-28）：只保留内置 Chromium 登录内核

用户拍板：**「只要内置 Chromium，并去除冗余代码」**。据此做的取舍：

## 保留

| 组件 | 说明 |
|---|---|
| `crates/wp-gui/src/browser_login.rs` | 内置 Chromium 启动 + CDP 读取会话（**只认程序自带的 `登录内核/`**，不再探测系统浏览器） |
| `scripts/fetch-login-chromium.sh` | 下载官方 Chromium 159 快照并精简为 `登录内核/`（255MB 压缩 / 392MB 解压，含 `chrome_crashpad_handler`） |
| `scripts/build-linux.sh` | 打包时把 `登录内核/` 一起放进绿色版 |
| 扫码 / Cookie / 短信 / 令牌 | 纯 HTTP 登录通道全部保留（百度/夸克扫码、光鸭短信/令牌） |

## 删除（冗余代码）

| 已删 | 原因 |
|---|---|
| `crates/wp-weblogin`（GTK + WebKitGTK 系统 WebView） | 与内置 Chromium 重复，且依赖系统库 |
| `crates/wp-net/src/jshost.rs` + `js-host` feature + `js_probe` | L1 JS 宿主没有业务消费者（签名全在 Rust），实证整包 JS 跑不了 |
| `scripts/js-host-parity.sh` | 随 L1 一起删 |
| 系统浏览器探测（Chrome/Edge/Chromium 路径表） | 产品只要内置内核，不再依赖用户环境 |
| 登录框 `webview-login` feature / 文案切换 | 统一为「内置浏览器」单一通道 |

## 新版登录链路

```
登录框「🌐 内置浏览器」
  └─ 启动 登录内核/chrome（自带 Chromium，独立临时配置 + 远程调试端口）
       └─ 用户在里面完成短信/扫码/密码登录
            └─ CDP Network.getAllCookies → 适配器 login_cookie 校验并落盘
```

失败路径：内核缺失 → 明确提示重装完整包 → 引导「Cookie」标签手动导入；
扫码只读 → 自动打开内置登录窗口拿可写会话（此逻辑保留）。

## 体积

| 项 | 体积 |
|---|---|
| 主程序 | ~27 MB |
| 播放器运行时 | ~126 MB |
| **登录内核（Chromium 159 精简）** | **392 MB（解压）/ 255 MB（压缩包）** |
| 绿色版分发体积 | ≈ 545 MB（不含用户数据） |

说明：这是「内置完整 Chromium」的物理下限——Chromium 主程序本身无法再安全裁剪；
语言包只留 zh-CN/en-US，已去掉调试符号与多余资源（chrome 二进制 strip 后 513MB→342MB）。

## Round 5：内置内核工程化收尾（2026-10-28）

| 项 | 内容 | 验证 |
|---|---|---|
| 内核预检（关于页） | 运行环境新增「登录内核」行，显示版本 + 规范化路径 | 截图 `出/验收截图/登录内核-关于页-20261028.png` 显示 `chromium-snapshot 1728157` |
| 内核预检（登录框） | 「🌐 内置浏览器」标签顶部显示内核状态；缺失时红字提示重装 | 截图 `出/验收截图/登录内核-登录框-20261028.png` |
| 内核自检脚本 | `scripts/verify-login-kernel.sh`：版本 → 真实启动 → CDP 握手，退出码可判 | 本机实跑 PASS：`Chromium 159.0.8128.0` + `CDP：Chrome/159.0.8128.0` |
| 下载硬化 | `fetch-login-chromium.sh`：zip 缺省校验和、下载后记录 `sha256`、`VERSION.txt` 带来源与校验值、附许可说明 | 脚本语法检查 + 复用已有 zip 路径走通 |
| Windows 对齐 | 新增 `scripts/fetch-login-chromium.ps1`（Win_x64 快照→`登录内核\`）；`build-windows.ps1` 打包带入内核并**保留 数据\** | 纯脚本，未在 Windows 实机验证 |
| 打包安全 | Linux/Windows 打包均改为「临时目录组装 → 保留 数据/ → 整体替换」 | 实跑输出 `已保留数据目录（2.0G）` |
| 测试 | +1 内核信息单测；CDP mock 超时放宽消除 flake | `cargo test -p wp-gui` 49 passed ×2 连续 |

产物：`出/网盘管理-linux-x64/`（主程序 27MB + 播放器 126MB + 登录内核 392MB，数据目录 2.0G 原样保留）。

## Round 6：真内核 CDP 联调 + 修掉一个生产级读取 bug（2026-10-28）

### 新增：真 Chromium 的 CDP 联调测试

`browser_login::tests::real_kernel_cdp_reads_cookies`（无内核/无显示自动跳过）：
启动随包 Chromium → 等页面 target → CDP `Network.setCookie` 注入 `BDUSS/STOKEN`（.baidu.com）
→ 用产品同款 `poll_cookies` 读回 → 断言 Cookie 头包含两个键。

实跑结果：

```
DevTools listening on ws://127.0.0.1:43475/devtools/browser/…
test browser_login::tests::real_kernel_cdp_reads_cookies ... ok
test result: ok. 50 passed; 0 failed
```

这条覆盖了「真实 Chromium Cookie 存储 + 我们的 WS/CDP/过滤管线」，
即「内置窗口登录后自动导入会话」的技术链路（真账号登录只剩最后一跳）。

### 修掉的生产 bug：`cdp_http_json` 依赖连接关闭

- 现象：Chrome 已监听、`curl` 0.1s 拿到 `/json/version`，但自研客户端 `read_to_end` 5s 后
  `Resource temporarily unavailable (os error 11)`，即读超时。
- 根因：DevTools HTTP 端不因 `Connection: close` 立刻关连接；`read_to_end` 等 EOF 等到超时。
- 修法：先读到 `\r\n\r\n` 拿响应头 → 按 `Content-Length` 读 body（chunked 走分块解码），
  不再依赖连接关闭。
- 影响：`wait_page_target` 是轮询 Cookie 的前置步骤，这个 bug 会让真机上的自动导入链路
  一直卡住；修复后真内核测试通过。

### 测试与清洁

| 项 | 结果 |
|---|---|
| `cargo test -p wp-gui` | **50 passed**, 连续 2 次（含真内核测试，约 3.5s） |
| 泄漏进程清理 | 测试结束按 profile 标记 pkill + 手动 `/proc` 扫描收尾，无残留 chrome |
| 构建告警 | 0 |

## Round 7：登录窗口生命周期收尾（2026-10-28）

绿色版真实使用中，用户会「点取消 / 关窗 / 放着超时」，这些路径必须不留垃圾进程与目录。本轮补齐并测试：

| 改动 | 说明 |
|---|---|
| 窗口尺寸 | 内置登录窗口固定 `--window-size=1024,768`，短信弹层不用横向滚动 |
| 取消响应 | 轮询里 `wait_page_target` 从 10s 降到 3s，取消/退出最迟约 3 秒生效 |
| 子进程清理 | `kill` 主进程后按 `--user-data-dir=<profile>` pkill 子进程，并轮询 pgrep 等它们真退出（最多 2s） |
| 目录清理 | `cleanup_profile`：删除失败重试 10×100ms（Chromium 收尾还在写文件会 ENOTEMPTY） |
| 文案统一 | 日志/界面统一为「内置登录窗口」「已取消内置登录」，不再出现「浏览器登录」字样 |

新增生命周期测试 `run_cancel_cleans_up_kernel`（无内核/无显示自动跳过）：
启动真内核 → 取消 → 断言 **Failed(已取消) 事件** + **临时配置目录已清空** + **无内核进程占用该目录**。

```
test browser_login::tests::run_cancel_cleans_up_kernel ... ok
test result: ok. 51 passed; 0 failed
```

## Round 8：三家登录起点「零浏览器」证据（2026-10-28）

浏览器只服务于百度/夸克的**短信登录**；三家各自的常规登录起点必须是纯 HTTP。
本轮把三个探针拆成不冲突的独立 example（此前同名 `qr_probe` 在共享 target 下互相覆盖，
跑「夸克」实际跑到的是光鸭二进制——这个坑也记进经验），并实跑取证：

| 网盘 | 探针 | 实跑输出（摘要） | 需要浏览器？ |
|---|---|---|---|
| 百度 | `baidu_qr_probe` | `qrcode?sign=7cdf…` + PNG 989B + 轮询 | 否 |
| 夸克 | `quark_qr_probe` | `https://su.quark.cn/4_eMHBJ?token=sta3263…` + PNG 4288B + `Query result is empty` 轮询 | 否 |
| 光鸭 | `guangya_qr_probe` | `device/?client_id=…&user_code=AWrg5zgJ…` + PNG 4477B + 等待确认轮询 | 否 |

命令：

```bash
cargo run -p wp-drive-baidu   --example baidu_qr_probe   -- /tmp/wp-baidu-probe
cargo run -p wp-drive-quark   --example quark_qr_probe   -- /tmp/wp-quark-probe
cargo run -p wp-drive-guangya --example guangya_qr_probe -- /tmp/wp-guangya-probe
cargo run -p wp-drive-baidu   --example baidu_qr_probe   -- /tmp/wp-qr-probe --write-probe  # 真账号写探针
```

结论：**绿色版解压后，三家登录入口都不依赖任何浏览器/JS 引擎**；
只有百度/夸克的「短信登录」需要内置 Chromium 窗口（登录内核）。

## Round 9：退出收尾 + 打包强约束（2026-10-28）

| 项 | 改动 | 验证 |
|---|---|---|
| 主程序退出清理 | `browser_login::shutdown_active()`：取消正在进行的登录 → pkill 内核子进程 → 清临时配置；在 `App::on_exit` 首先调用 | 新增测试 `shutdown_active_cancels_running_login`：启动真内核 → 调 shutdown → 线程结束、目录清空、无进程；幂等（空闲再调不 panic） |
| 全局状态串行 | 三条真内核测试（真内核读 Cookie / 取消清理 / 退出清理）加 `KERNEL_TEST_LOCK`，避免并行测试互相 kill | 全套 **52 passed ×2**（8.3s） |
| 打包强约束 | `build-linux.sh --portable` 缺 `登录内核/` 直接报错退出（`WP_ALLOW_NO_KERNEL=1` 才放行） | 实测：移走内核 → `❌ 缺少 登录内核/` exit=1；恢复后 392M 完好 |

说明：`shutdown_active` 是「关主程序」这条路径的兜底——不修的话，用户开着登录窗口直接退出，
登录线程随进程消失但 Chromium 子进程会留在桌面且占着临时配置目录。

## Round 10：异常退出自愈 + 运行依赖盘点（2026-10-28）

| 项 | 内容 | 验证 |
|---|---|---|
| 启动自愈 | `browser_login::sweep_stale()`：App 启动时扫描 `数据/浏览器登录/*`，逐个 pkill 内核进程 + 删临时配置（防崩溃/强杀留下的孤儿与垃圾） | 单测 + **GUI 实跑**：预置残留目录 → 启动 → 日志 `已清理上次异常退出残留的内置登录配置：1 个`，目录清空 |
| 运行依赖盘点 | `objdump -T chrome` 实测最高只需 **GLIBC_2.25**；动态依赖 38 个系统库（nss/gbm/alsa/atk/pango/x11 等） | 本机 `ldd` 无缺失；README 已列运行机要求 |
| 测试 | 全套 **53 passed ×2**（8.3s） | — |

说明：绿色版不是完全静态自包含——Chromium 内核天然依赖桌面发行版的 X11/GL/多媒体基础库；
可以裁剪的只是「内置浏览器」这一层的分发方式（不再要求用户装 Chrome/Edge）。

## 用户反馈修复（2026-10-28 02:3x）：百度/光鸭/界面精简

### 1. 百度「内置登录后仍然只读」根因与修复
- 现象（用户真机日志 `运行-20261028.log:358-378`）：内置窗口登录页面 `https://pan.baidu.com/`，
  `取到 baidu 的会话` 后立即 `errno:-6 / 会话只读`。
- 根因：登录过程中 **passport 域先下发 64 字符 STOKEN**，旧逻辑「凑齐 BDUSS+STOKEN 就返回」
  会在 pan 域 128 字符 STOKEN 下发前把会话抓走 → 导入的必然是只读会话。
- 修复：`format_cookie_header` 对百度**强制要求 pan 域 STOKEN**（评分 30），
  passport 域 STOKEN 一律不返回；轮询时每 ~6 秒用 CDP `Page.navigate` 回 `pan.baidu.com`
  触发 pan 会话下发；成功日志打印 `百度 STOKEN 域=pan.baidu.com 评分=30 长度=N`。
- 验证：单测（passport-only → None、pan → 选中）+ 注入真内核链路 PASS。

### 2. 光鸭「没有内置浏览器登录」
- 光鸭只用 access_token（无 Cookie 登录能力），因此浏览器登录实现为：
  内置窗口打开**设备授权页** + App 侧复用设备码轮询，授权确认后自动导入（窗口自动关闭）。
- 实跑日志：`内置登录窗口：设备授权页 https://account.guangyapan.com/__/auth/device/?...`
  `已打开内置登录窗口（设备授权页）`。

### 3. 界面精简（只留登录内容）
- 内核启动改为 `--app=<登录页>`：app 窗口，无标签栏/地址栏/书签栏/菜单；
- 关闭登录用不到的组件：extensions、component-update、breakpad/crashpad 上报、
  default-apps、Translate/MediaRouter/OptimizationHints 等 features。

### 4. 内核体积
- `strip --strip-all` 实测无收益（官方快照已 strip，358,588,656 B 不变），
  内核体积下限即 Chromium 本体；可精简的是窗口形态与运行组件，不是二进制。

### 5. app 窗口实拍验证（2026-10-28）
`--app=https://pan.baidu.com/` 在 Xvfb 下截图（`出/验收截图/内置浏览器-app窗口-20261028.png`）：
窗口内容从页面顶栏开始，**没有标签栏/地址栏/书签栏/菜单**，只有登录页本身。

### 6. 百度补偿导航的误伤修复（Round 14）
上一版「每 6 秒导航回 pan.baidu.com」是**无条件**的：用户还在输短信验证码时窗口就被刷回首页，
可能直接破坏百度登录。现改为严格策略（`should_nudge_pan`）：
- **只在登录已发生（Cookie 里有 BDUSS）且缺 pan 域 STOKEN 时**才导航；
- 导航间隔 ≥8 秒、总次数 ≤5；
- 登录前绝不导航（不打断填表），并加单测覆盖 5 种边界（无 BDUSS / 仅 passport STOKEN / 已有 pan STOKEN / 次数上限）。
测试：55 passed。

### 7. 百度新版 Cookie `BDUSS_BFESS`（Round 16）
真机日志 `运行-20261028.log:030717` 显示适配器已兼容 `BDUSS_BFESS`（`session.rs` 明确：
最小有效集 = (`BDUSS` 或 `BDUSS_BFESS`) + `STOKEN`）。内置浏览器抓取侧的同步修复：
- 百度登录态**允许 `BDUSS_BFESS` 替代 `BDUSS`**（任一存在即可），
  避免百度切换新版 Cookie 后「一直等不到 BDUSS → 超时失败」；
- 仍强制 pan 域 STOKEN；抓到的 `BDUSS_BFESS` 会原样进 Cookie 头交给适配器。
单测覆盖：只有 BDUSS_BFESS + pan STOKEN → 可用。

### 8. 光鸭登录框缺「内置浏览器」标签 + 减少一次点击（Round 17）
用户反馈：百度内置浏览器登录**已成功**；光鸭登录框仍只有 扫码/短信/令牌。
- 根因：光鸭适配器 `caps.auth.browser` 是 `false`（标签按该能力显示），而光鸭的浏览器登录
  实际是「设备授权页窗口 + 设备码轮询」，能力应视为可用。
- 修复：`wp-drive-guangya` 的 `browser` 能力改为 `true`；点「内置浏览器」标签时
  **直接触发登录**（`LoginOutcome::StartBrowser`），不再需要在标签里再点一次按钮。
- 实测：`WP_DEMO_LOGIN_TAB=browser` + 光鸭 → 日志 `内置登录窗口：设备授权页 https://account.guangyapan.com/...`。

### 9. 三个登录窗口问题修复（Round 18，用户真机反馈）
| 问题 | 根因 | 修复 |
|---|---|---|
| 光鸭内置浏览器打开 404 | API 返回的 `verification_uri` 直接打开是 404 页（它更适合给 App 扫码）；网页版设备授权页是 `https://www.guangyapan.com/device?code=<用户码>` | 光鸭内置窗口改用用户码拼 web 设备页 URL（实测页面正常渲染，非 404） |
| 夸克反复弹「访问此设备上的其他应用和服务（屏蔽/允许）」 | Chromium 新版权限提示（本地网络访问等），登录窗口不需要设备权限 | 启动参数加 `--deny-permission-prompts` + `--disable-notifications`，disable-features 增加 LocalNetworkAccess*；实测窗口内不再弹权限框 |
| 百度还要点一下「去登录」 | 打开的是网盘落地页 | 直达统一登录页 `wappass.baidu.com/passport/?login&tpl=netdisk&u=https://pan.baidu.com/`；实测窗口直接显示登录表单（手机号/用户名/邮箱 + 下一步） |
