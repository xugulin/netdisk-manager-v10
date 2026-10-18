# 第三方许可证文本（assets/licenses/）

> 本目录存放「播放器运行时」发行包随附的第三方许可证文本与来源说明。
> `scripts/package-player-runtime-linux.sh`（及 Windows 结构脚本）会把本目录整体复制到
> 发行包的 `LICENSES/` 下；**发行包不得缺少本目录中的任何文件**。
>
> 整理日期：2026-10-19；整理方式：优先复用 `vendor/prebuilt/linux-x86_64/licenses/`
> （我们的 LGPL libmpv 构建产物），其余从上游官方仓库/发行版源码服务获取（URL 见下表）。

## 文件清单与来源

| 文件 | 对应组件 | 许可证 | 来源 |
|---|---|---|---|
| `LGPL-2.1.txt` | GNU LGPL 2.1 通用文本（mpv / FFmpeg / libplacebo / libbluray / libaacs / libbdplus 共用） | LGPL-2.1 | 来自我们的 mpv 构建产物 `vendor/prebuilt/linux-x86_64/licenses/LICENSE.LGPL` |
| `GPL-2.0.txt` | GNU GPL 2.0 通用文本（**可选** GPL 构建 / 部分依赖的 GPL 变体说明用） | GPL-2.0 | 来自 `vendor/prebuilt/linux-x86_64/licenses/LICENSE.GPL` |
| `mpv-Copyright.txt` | mpv（含 mplayer2/MPlayer 版权声明） | LGPL-2.1+（本项目 `-Dgpl=false` 构建） | 来自 `vendor/prebuilt/linux-x86_64/licenses/Copyright` |
| `libass-ISC.txt` | libass（字幕渲染） | ISC | `https://github.com/libass/libass/blob/master/COPYING`（与系统 `/usr/share/licenses/libass/COPYING` 一致） |
| `FFmpeg-LGPL-2.1.txt` | FFmpeg（avcodec/avformat/avutil/avfilter/swscale/swresample） | LGPL-2.1+（**须为 LGPL 构建**，见主文档 §5） | `https://github.com/FFmpeg/FFmpeg/blob/master/COPYING.LGPLv2.1` |
| `libplacebo-LGPL-2.1.txt` | libplacebo（GPU 渲染） | LGPL-2.1+ | `https://github.com/haasn/libplacebo/blob/master/LICENSE` |
| `libbluray-LGPL-2.1.txt` | libbluray（蓝光） | LGPL-2.1 | Debian 源码服务 `libbluray 1:1.5.0-3` 的 `COPYING`（`https://sources.debian.org/src/libbluray/`） |
| `libaacs-LGPL-2.1.txt` | libaacs（可选 AACS） | LGPL-2.1 | Debian 源码服务 `libaacs 0.11.1-4` 的 `COPYING` |
| `libbdplus-LGPL-2.1.txt` | libbdplus（可选 BD+） | LGPL-2.1 | Debian 源码服务 `libbdplus 0.2.0-5` 的 `COPYING` |
| `ANGLE-BSD-3-Clause.txt` | ANGLE（Windows 上的 EGL/GLES 实现，`libEGL.dll`/`libGLESv2.dll`） | BSD-3-Clause | `https://github.com/google/angle/blob/main/LICENSE` |

## 合规用法

1. 本目录是**许可证文本**，不是「许可证声明」本身；声明文字（哪些组件、什么许可证、源码从哪拿）
   由 `docs/PLAYER_PACKAGING.md` §4 与发行包内 `LICENSES/README.md`（本文件复制品）共同构成。
2. mpv 采用 **LGPL-2.1+**（`-Dgpl=false` 构建）。LGPL 义务：动态链接 + 允许用户替换库 +
   提供对应源码获取方式。详见 `docs/PLAYER_PACKAGING.md` §4。
3. FFmpeg 必须使用 **LGPL 构建**才能与 LGPL mpv 一起随包分发；若链接系统 GPL 构建的 FFmpeg，
   则该组合在分发时构成 GPL 衍生（我们的「系统依赖模式」不随包含 FFmpeg，故发行包本身不含 FFmpeg；
   用户系统自带的 GPL FFmpeg 属于用户本机组合）。
4. ANGLE 为 BSD-3-Clause：保留版权声明与许可证文本即可；Windows 结构脚本会校验
   `LICENSES/ANGLE-BSD-3-Clause.txt` 存在。
5. 若未来引入新依赖（例如 libmujs、libarchive、libuchardet、真体/字体库等），
   **必须在 `vendor/prebuilt/*/BUILDINFO.txt` 与实际 `ldd`/`ntldd` 结果里核对**后追加本目录文本，
   并更新本表。

## 维护

新增/更新许可证文本时：
* 文件命名：`<组件名>-<许可证>-<可选版本>.txt`；
* 在本表追加「文件/组件/许可证/来源」一行；
* 跑一次 `scripts/package-player-runtime-linux.sh --verify-only` 确认发行包 LICENSES 校验通过。
