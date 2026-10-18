//! M2 字幕真机联调素材生成器（task-17 可复现套件）。
//!
//! 一条命令生成 render_probe 真机联调所需的全部素材 + 矩阵脚本：
//! - 字幕素材：SRT / VTT / ASS（大字号+颜色+描边+`\pos`）、主+副字幕、延迟样本、AI 双语源与译文；
//! - 视频素材（需 ffmpeg）：10s testsrc2 彩条片（展示用）、10s 静态背景片（像素差分用）、
//!   内嵌 SRT 的 mkv（default / 非 default 两种轨标记）；
//! - 双语 ASS（`build_bilingual_ass_from_doc`；与 `wp_ai::integration::translate_to_bilingual_ass`
//!   内部同一 API，正式 AI 链路证据另见 docs/PLAYER_SUBTITLE_M0.md）；
//! - `run_matrix.sh`：Xvfb + media-core `render_probe` 的 20+ 用例矩阵（截图 + 字幕带像素 AE 指标）。
//!
//! 运行（默认写到 `出/验收截图/字幕弹幕/字幕联调/`）：
//! ```text
//! cargo run -p wp-subtitle --example m2_probe_kit                # 只生成素材
//! cargo run -p wp-subtitle --example m2_probe_kit -- --run       # 生成后直接跑矩阵（需 Xvfb :99）
//! cargo run -p wp-subtitle --example m2_probe_kit -- /tmp/m2-kit --run
//! ```
//! 前置：`cargo build -p wp-media-mpv --example render_probe`（矩阵用）、Xvfb（默认 `:99`）、
//! ffmpeg + ImageMagick（`magick`/`convert`）。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const ZH_SRT: &str = r##"1
00:00:01,000 --> 00:00:03,000
简体中文：字幕显示正常

2
00:00:03,500 --> 00:00:06,000
日本語：字幕が表示されます 🎌

3
00:00:06,500 --> 00:00:09,000
Emoji 测试 🎉🚀 与 English mixed
"##;
const ZH_VTT: &str = r##"WEBVTT - 字幕矩阵 M2 真机联调

NOTE 该文件用于 render_probe 外挂 VTT 显示验证

id-a
00:00:01.000 --> 00:00:03.000 line:90% align:center
简体中文：VTT 字幕显示正常

id-b
00:00:03.500 --> 00:00:06.000
日本語：VTT 字幕が表示されます 🎌

id-c
00:00:06.500 --> 00:00:09.000
Emoji 测试 🎉🚀 与 English mixed
"##;
const STYLED_ASS: &str = r##"[Script Info]
; M2 真机联调：ASS 样式（大字号/颜色/描边/位置/淡入淡出）
ScriptType: v4.00+
PlayResX: 1280
PlayResY: 720
WrapStyle: 2
ScaledBorderAndShadow: yes
YCbCr Matrix: TV.709

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Big,Noto Sans CJK SC,72,&H0000FFFF,&H000000FF,&H00000000,&H80000000,-1,0,0,0,100,100,0,0,1,3,2,2,20,20,40,1
Style: Top,Noto Sans CJK SC,48,&H00FFFF00,&H000000FF,&H00202020,&H80000000,-1,0,0,0,100,100,0,0,1,2,1,8,20,20,30,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:04.00,Big,,0,0,0,,大字号 72px 黄色描边（底部居中）
Dialogue: 0,0:00:04.50,0:00:08.00,Top,,0,0,0,,{\pos(640,120)\fad(300,300)}顶部 \pos(640,120) 位置实测
Dialogue: 1,0:00:08.00,0:00:09.50,Big,,0,0,0,,{\b1}粗体\b0 与 {\i1}斜体{\i0} 样式
"##;
const PRIMARY_SRT: &str = r##"1
00:00:01,000 --> 00:00:09,000
【主字幕】中文 primary subtitle（底部）
"##;
const SECONDARY_ASS: &str = r##"[Script Info]
ScriptType: v4.00+
PlayResX: 1280
PlayResY: 720
WrapStyle: 2
ScaledBorderAndShadow: yes

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Secondary,Noto Sans CJK SC,44,&H0000FF00,&H000000FF,&H00000000,&H80000000,-1,0,0,0,100,100,0,0,1,2,1,8,20,20,36,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:09.00,Secondary,,0,0,0,,【副字幕】SECONDARY subtitle（顶部绿色）
"##;
const DELAY_SRT: &str = r##"1
00:00:01,000 --> 00:00:03,000
延迟测试：第一句（1.0~3.0s）

2
00:00:05,000 --> 00:00:07,000
延迟测试：第二句（5.0~7.0s）
"##;
const AI_SOURCE_SRT: &str = r##"1
00:00:01,000 --> 00:00:03,000
欢迎使用本播放器

2
00:00:03,500 --> 00:00:06,000
字幕支持双语显示

3
00:00:06,500 --> 00:00:09,000
这是 AI 翻译示例
"##;
const AI_TRANSLATIONS_JSON: &str = r##"[
  "Welcome to this player",
  "Subtitles support bilingual display",
  "This is an AI translation example"
]
"##;
const RUN_MATRIX_SH: &str = r##"#!/usr/bin/env bash
# M2 字幕真机联调矩阵（task-17）：media-core 的 render_probe 在 Xvfb 下逐用例渲染并截图。
#
# 设计：
#  - 差分用例用「静态背景 10s 视频」(static-10s.mp4)：同 seek 无字幕控制组 → 字幕带像素 AE 差分；
#  - 展示用例用「testsrc2 彩条视频」(test-10s.mp4) → 肉眼可读的真实画面截图；
#  - --seconds 5.5 保证 seek 已生效后再退出（1.2s 时首帧延迟会导致 seek 竞态）；
#  - 偶发 seek 黑帧（截图早于新帧渲染）自动重试一次；
#  - 字幕 AE 分带：bottom=底部主字幕带、top=顶部（副字幕/\pos）、mid=上移位置带。
#
# 依赖：DISPLAY 上已有 Xvfb（默认 :99）、render_probe 已构建、ffmpeg + ImageMagick
# 用法：DISPLAY=:99 bash run_matrix.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../../../.." && pwd)"
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/debug/examples/render_probe"
STATIC="$HERE/assets/static-10s.mp4"
TESTSRC="$HERE/assets/test-10s.mp4"
BILINGUAL="$HERE/assets/bilingual.ass"
OUT="$HERE/raw"; SCR="$HERE/screens"
mkdir -p "$OUT" "$SCR"
export DISPLAY="${DISPLAY:-:99}"
COMMON=(--size 1280x720 --hwdec no --seconds 5.5)

conv() { local ppm=$1 png=$2; if command -v magick >/dev/null 2>&1; then magick "$ppm" "$png"; else convert "$ppm" "$png"; fi; }
mean() { magick "$1" -colorspace Gray -format "%[fx:mean]" info: 2>/dev/null || echo 1; }

run() { # run <video> <name> <seek> [额外参数...]
  local video=$1 name=$2 seek=$3; shift 3
  # 允许用 --source <path> 覆盖视频（内嵌字幕用例）
  local -a extra=()
  while [[ $# -gt 0 ]]; do
    if [[ "$1" == "--source" ]]; then video="$2"; shift 2; else extra+=("$1"); shift; fi
  done
  local attempt png="$SCR/$name.png"
  for attempt in 1 2 3; do
    timeout 90 "$BIN" "$video" "${COMMON[@]}" --seek "$seek" "${extra[@]}" \
      --screenshot "$OUT/$name.ppm" --json > "$OUT/$name.json" 2> "$OUT/$name.log"
    local rc=$?
    [[ -s "$OUT/$name.ppm" ]] && conv "$OUT/$name.ppm" "$png"
    # 黑帧（seek 竞态）重试：静态背景 mean≈0.12、testsrc2≈0.4；黑帧 < 0.02
    if [[ -s "$png" ]] && awk -v m="$(mean "$png")" 'BEGIN{exit !(m < 0.02)}'; then
      echo "  [retry] $name 黑帧（attempt $attempt）"; continue
    fi
    break
  done
  local pos; pos=$(python3 -c "import json;d=json.load(open('$OUT/$name.json'));print(round(d['final_snapshot']['position_secs'],2))" 2>/dev/null)
  local cues; cues=$(grep -o '"subtitle_cues":\[[^]]*\]' "$OUT/$name.json" | head -c 220)
  printf '%-30s seek=%-4s pos=%-6s rc=%-3s %s\n' "$name" "$seek" "${pos:-?}" "$rc" "$cues"
}

echo "== render_probe: $BIN"; echo "== DISPLAY=$DISPLAY"
[[ -x "$BIN" ]] || { echo "缺少 render_probe"; exit 1; }
[[ -s "$STATIC" && -s "$TESTSRC" && -s "$BILINGUAL" ]] || { echo "缺少素材（视频/bilingual.ass）"; exit 1; }

# 期望有字幕的用例：渲染后检查字幕带像素是否真的出现，未出现则重试（高负载下偶发 seek/注入竞态）
band_ae_vs() { # <case_png> <ctrl_png> <bottom|top|both>
  local case_png=$1 ctrl_png=$2 band=$3 crop
  case "$band" in
    bottom) crop="1280x220+0+500" ;;
    top)    crop="940x180+340+0" ;;
    *)      crop="1280x720+0+0" ;;
  esac
  magick "$case_png" -crop "$crop" +repage /tmp/sub-a.png
  magick "$ctrl_png" -crop "$crop" +repage /tmp/sub-b.png
  magick compare -metric AE /tmp/sub-a.png /tmp/sub-b.png null: 2>&1 | awk '{print $1}'
}
run_sub() { # run_sub <video> <name> <seek> <ctrl_seek> <band> [额外参数...]
  local video=$1 name=$2 seek=$3 ctrl=$4 band=$5; shift 5
  local attempt ae
  for attempt in 1 2 3; do
    run "$video" "$name" "$seek" "$@"
    ae=$(band_ae_vs "$SCR/$name.png" "$SCR/ctrl-$ctrl.png" "$band")
    if awk -v v="${ae:-0}" 'BEGIN{exit !(v > 0)}'; then return 0; fi
    echo "  [retry] $name 字幕未出现（AE=${ae:-?}，attempt $attempt）"
  done
}

# ============ 差分矩阵：静态背景 + 无字幕控制组 ============
for seek in 1.2 1.3 3.1 3.8 4.8; do run "$STATIC" "ctrl-$seek" "$seek"; done

run_sub "$STATIC" 01-srt 1.2 1.2 bottom --sub "$HERE/assets/zh-ja-emoji.srt"
run_sub "$STATIC" 02-vtt 3.8 3.8 bottom --sub "$HERE/assets/zh-ja-emoji.vtt"
run_sub "$STATIC" 03-ass-styled 1.3 1.3 bottom --sub "$HERE/assets/styled.ass"
run_sub "$STATIC" 04-ass-pos 4.8 4.8 top --sub "$HERE/assets/styled.ass"
run_sub "$STATIC" 05-main-secondary 1.2 1.2 bottom \
  --sub "$HERE/assets/primary.srt" --sub "$HERE/assets/secondary.ass" \
  --prop secondary-sid=2 --prop secondary-sub-visibility=yes
run_sub "$STATIC" 06-style-override 1.3 1.3 bottom --sub "$HERE/assets/styled.ass" \
  --prop sub-ass-override=force --prop sub-font-size=90 --prop sub-color=#FF00FF
# 延迟：同 seek 下 0s 显示第一句、2s 延迟后该句已过（空）；另 seek 下延迟后显示第二句
run_sub "$STATIC" 07-delay-baseline 1.2 1.2 bottom --sub "$HERE/assets/delay.srt"
run "$STATIC" 07b-delay-2s-same 1.2 --sub "$HERE/assets/delay.srt" --prop sub-delay=2.0
run_sub "$STATIC" 08-delay-2s-second 3.1 3.1 bottom --sub "$HERE/assets/delay.srt" --prop sub-delay=2.0
run_sub "$STATIC" 09-font-serif 1.3 1.3 bottom --sub "$HERE/assets/styled.ass" \
  --prop sub-ass-override=force --prop sub-font="Noto Serif CJK SC" --prop sub-font-size=72
run_sub "$STATIC" 10-ai-bilingual 1.2 1.2 bottom --sub "$BILINGUAL"
# 内嵌字幕 A：mkv 内 sub 轨带 default disposition → mpv 自动选中（无需任何 prop）
run_sub "$STATIC" 11-embedded-default 1.2 1.2 bottom --source "$HERE/assets/embedded-default.mkv"
# 内嵌字幕 B：sub 轨无 default → mpv 默认 sid=no（实测），需要显式选轨 sid=1
run_sub "$STATIC" 15-embedded-explicit-sid 1.2 1.2 bottom   --source "$HERE/assets/embedded-nodefault.mkv" --prop sid=1
# 字幕可见性开关（GUI「字幕开关」）
run "$STATIC" 12-sub-visibility-off 1.2 --sub "$HERE/assets/zh-ja-emoji.srt" --prop sub-visibility=no
# 位置上移（mpv sub-pos：100=底部默认，80=更靠上；实测字幕移到 y≈520-540）
run_sub "$STATIC" 13-sub-pos-up 1.2 1.2 bottom --sub "$HERE/assets/zh-ja-emoji.srt" --prop sub-pos=80
# 副字幕可见性开关（GUI「副字幕开关」）
run_sub "$STATIC" 14-secondary-off 1.2 1.2 bottom \
  --sub "$HERE/assets/primary.srt" --sub "$HERE/assets/secondary.ass" \
  --prop secondary-sid=2 --prop secondary-sub-visibility=no

# ============ 展示用例：testsrc2 彩条画面（人工可读） ============
run "$TESTSRC" show-01-srt 1.2 --sub "$HERE/assets/zh-ja-emoji.srt"
run "$TESTSRC" show-03-ass-styled 1.3 --sub "$HERE/assets/styled.ass"
run "$TESTSRC" show-05-main-secondary 1.2 \
  --sub "$HERE/assets/primary.srt" --sub "$HERE/assets/secondary.ass" \
  --prop secondary-sid=2 --prop secondary-sub-visibility=yes
run "$TESTSRC" show-10-ai-bilingual 1.2 --sub "$BILINGUAL" \
  --screenshot-window "$OUT/show-10-window.ppm"

# 窗口截图（egui 叠加层 + 视频 + 字幕同帧）转 PNG
if [[ -s "$OUT/show-10-window.ppm" ]]; then conv "$OUT/show-10-window.ppm" "$SCR/show-10-window.png"; fi

# ============ 字幕像素差分（静态背景 ⇒ 差值即字幕像素） ============
metrics="$OUT/case-metrics.txt"
: > "$metrics"
ae() { local a=$1 b=$2; magick compare -metric AE "$a" "$b" null: 2>&1 | awk '{print $1}'; }
crop_band() { magick "$1" -crop "$2" +repage "/tmp/band-$3.png"; }
pair() { # pair <case> <ctrl>
  local case_png="$SCR/$1.png" ctrl_png="$SCR/$2.png"
  [[ -s "$case_png" && -s "$ctrl_png" ]] || { echo "$1: MISSING png" >> "$metrics"; return; }
  local full bot top mid
  full=$(ae "$case_png" "$ctrl_png")
  crop_band "$case_png" "1280x220+0+500" a_bot; crop_band "$ctrl_png" "1280x220+0+500" b_bot
  bot=$(ae /tmp/band-a_bot.png /tmp/band-b_bot.png)
  crop_band "$case_png" "940x180+340+0" a_top; crop_band "$ctrl_png" "940x180+340+0" b_top
  top=$(ae /tmp/band-a_top.png /tmp/band-b_top.png)
  crop_band "$case_png" "1280x160+0+440" a_mid; crop_band "$ctrl_png" "1280x160+0+440" b_mid
  mid=$(ae /tmp/band-a_mid.png /tmp/band-b_mid.png)
  printf '%-26s ctrl=%-10s full=%-12s bottom=%-12s top=%-12s mid=%s\n' "$1" "$2" "$full" "$bot" "$top" "$mid" | tee -a "$metrics"
}
{
  echo "===== 字幕像素差分（AE：0=与无字幕控制组相同；>0=该带出现/变化像素）====="
  echo "static = 静态背景视频；band: bottom=主字幕带(y500-720) top=顶部带(y0-180, x>340避开时间戳) mid=上移带(y440-600)"
} | tee -a "$metrics"
pair 01-srt ctrl-1.2
pair 02-vtt ctrl-3.8
pair 03-ass-styled ctrl-1.3
pair 04-ass-pos ctrl-4.8
pair 05-main-secondary ctrl-1.2
pair 06-style-override ctrl-1.3
pair 07-delay-baseline ctrl-1.2
pair 07b-delay-2s-same ctrl-1.2
pair 08-delay-2s-second ctrl-3.1
pair 09-font-serif ctrl-1.3
pair 10-ai-bilingual ctrl-1.2
pair 11-embedded-default ctrl-1.2
pair 12-sub-visibility-off ctrl-1.2
pair 13-sub-pos-up ctrl-1.2
pair 14-secondary-off ctrl-1.2
pair 15-embedded-explicit-sid ctrl-1.2
{
  echo "===== 对照（不同字幕内容/样式之间的差异）====="
  printf '延迟 0s vs 2s（同 seek）       AE=%s\n' "$(ae "$SCR/07-delay-baseline.png" "$SCR/07b-delay-2s-same.png")"
  printf '延迟后第一句 vs 第二句         AE=%s\n' "$(ae "$SCR/07-delay-baseline.png" "$SCR/08-delay-2s-second.png")"
  printf 'ASS 默认样式 vs serif 字体     AE=%s\n' "$(ae "$SCR/03-ass-styled.png" "$SCR/09-font-serif.png")"
  printf 'ASS 默认样式 vs 强制覆盖样式   AE=%s\n' "$(ae "$SCR/03-ass-styled.png" "$SCR/06-style-override.png")"
  printf '主+副字幕 vs 关闭副字幕        AE=%s\n' "$(ae "$SCR/05-main-secondary.png" "$SCR/14-secondary-off.png")"
  printf 'SRT 默认位置 vs sub-pos=80     AE=%s\n' "$(ae "$SCR/01-srt.png" "$SCR/13-sub-pos-up.png")"
  printf '外挂 vs 内嵌 default（同内容不同轨道） AE=%s\n' "$(ae "$SCR/01-srt.png" "$SCR/15-embedded-explicit-sid.png")"
  printf '内嵌 default vs 非 default 显式选轨 AE=%s\n' "$(ae "$SCR/11-embedded-default.png" "$SCR/15-embedded-explicit-sid.png")"
} | tee -a "$metrics"
echo
echo "== 完成 $(date -Is)"; echo "PNG: $(ls "$SCR" | wc -l) 张；指标: $metrics"
"##;

fn default_out_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join("出/验收截图/字幕弹幕/字幕联调"))
        .unwrap_or_else(|| PathBuf::from("m2-subtitle-kit"))
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(err) => {
            eprintln!("m2_probe_kit 失败：{err}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut out_dir: Option<PathBuf> = None;
    let mut do_run = false;
    for arg in &args {
        match arg.as_str() {
            "--run" => do_run = true,
            other if other.starts_with("--") => return Err(format!("未知参数：{other}")),
            other => out_dir = Some(PathBuf::from(other)),
        }
    }
    let out = out_dir.unwrap_or_else(default_out_dir);
    let assets = out.join("assets");
    fs::create_dir_all(&assets).map_err(|e| format!("创建 {} 失败：{e}", assets.display()))?;

    // 1) 文本素材（确定性内容，与 docs/PLAYER_SUBTITLE_M0.md 的用例一致）
    let files: [(&str, &str); 8] = [
        ("zh-ja-emoji.srt", ZH_SRT),
        ("zh-ja-emoji.vtt", ZH_VTT),
        ("styled.ass", STYLED_ASS),
        ("primary.srt", PRIMARY_SRT),
        ("secondary.ass", SECONDARY_ASS),
        ("delay.srt", DELAY_SRT),
        ("ai_source.srt", AI_SOURCE_SRT),
        ("ai_translations.json", AI_TRANSLATIONS_JSON),
    ];
    for (name, content) in files {
        write_text(&assets.join(name), content)?;
    }

    // 2) 双语 ASS（wp_subtitle 直接生成；与 wp_ai::integration 同 API 同产物）
    generate_bilingual_ass(&assets)?;

    // 3) 视频/内嵌 mkv（需要 ffmpeg）
    if have("ffmpeg") {
        make_videos(&assets)?;
        println!("[视频] 已生成 test-10s.mp4 / static-10s.mp4 / embedded-default.mkv / embedded-nodefault.mkv");
    } else {
        println!("[视频] 跳过：未找到 ffmpeg（矩阵差分/展示用例需要视频，请安装 ffmpeg 后重跑）");
    }

    // 4) 矩阵脚本 + 使用说明
    let script_path = out.join("run_matrix.sh");
    write_text(&script_path, RUN_MATRIX_SH)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&script_path)
            .map_err(|e| format!("读取脚本权限失败：{e}"))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).map_err(|e| format!("设置脚本可执行失败：{e}"))?;
    }
    write_text(&out.join("README.md"), KIT_README)?;

    println!("[完成] 素材目录：{}", out.display());
    println!("  下一步：");
    println!("    cargo build -p wp-media-mpv --example render_probe");
    println!("    Xvfb :99 -screen 0 1280x800x24 &   # 若尚未运行");
    println!("    DISPLAY=:99 bash {}/run_matrix.sh", out.display());

    if do_run {
        println!("[run] 执行矩阵脚本 ...");
        let status = Command::new("bash")
            .arg(&script_path)
            .env(
                "DISPLAY",
                std::env::var("DISPLAY").unwrap_or_else(|_| ":99".into()),
            )
            .status()
            .map_err(|e| format!("执行矩阵脚本失败：{e}"))?;
        if !status.success() {
            return Err(format!("矩阵脚本退出码：{:?}", status.code()));
        }
        println!(
            "[run] 矩阵完成，报告见 {} 与 docs/PLAYER_SUBTITLE_M0.md",
            out.join("raw").display()
        );
    }
    Ok(())
}

fn write_text(path: &Path, content: &str) -> Result<(), String> {
    fs::write(path, content.as_bytes()).map_err(|e| format!("写 {} 失败：{e}", path.display()))
}

fn have(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn ffmpeg(args: &[&str]) -> Result<(), String> {
    let output = Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(args)
        .output()
        .map_err(|e| format!("执行 ffmpeg 失败：{e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "ffmpeg 失败（{}）：{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// 找一个可用的 CJK 字体文件给 drawtext 打时间戳（找不到则视频不带时间戳，不影响字幕用例）。
fn cjk_font() -> Option<String> {
    if let Ok(output) = Command::new("fc-match")
        .args(["-f", "%{file}", "Noto Sans CJK SC"])
        .output()
    {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() && Path::new(&path).is_file() {
            return Some(path);
        }
    }
    let fallback = "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc";
    Path::new(fallback).is_file().then(|| fallback.to_string())
}

fn font_filter() -> String {
    match cjk_font() {
        Some(font) => format!(
            "drawtext=fontfile={font}:text='t=%{{pts\\:hms}}':x=40:y=24:fontsize=36:fontcolor=white:box=1:boxcolor=black@0.6"
        ),
        None => "null".to_string(),
    }
}

fn make_videos(assets: &Path) -> Result<(), String> {
    let test = assets.join("test-10s.mp4");
    let stat = assets.join("static-10s.mp4");
    let vf = font_filter();
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=1280x720:rate=30:duration=10",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:duration=10",
        "-vf",
        &vf,
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-b:a",
        "96k",
        "-shortest",
        test.to_str().ok_or("路径编码失败")?,
    ])?;
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "color=c=0x202028:size=1280x720:rate=30:duration=10",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:duration=10",
        "-vf",
        &vf,
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-b:a",
        "96k",
        "-shortest",
        stat.to_str().ok_or("路径编码失败")?,
    ])?;
    let srt = assets.join("zh-ja-emoji.srt");
    let srt = srt.to_str().ok_or("路径编码失败")?;
    let stat_s = stat.to_str().ok_or("路径编码失败")?;
    let variants: [(&str, &str, &str); 2] = [
        ("embedded-default.mkv", "default", "中文测试字幕(default)"),
        ("embedded-nodefault.mkv", "0", "中文测试字幕(非default)"),
    ];
    for (name, disposition, title) in variants {
        let out = assets.join(name);
        let title_meta = format!("title={title}");
        ffmpeg(&[
            "-i",
            stat_s,
            "-i",
            srt,
            "-map",
            "0",
            "-map",
            "1",
            "-c",
            "copy",
            "-c:s",
            "srt",
            "-disposition:s:0",
            disposition,
            "-metadata:s:s:0",
            "language=chi",
            "-metadata:s:s:0",
            &title_meta,
            out.to_str().ok_or("路径编码失败")?,
        ])?;
    }
    Ok(())
}

/// 用 wp_subtitle 生成双语 ASS（与 wp_ai::integration 链路产物一致）。
fn generate_bilingual_ass(assets: &Path) -> Result<(), String> {
    use wp_subtitle::{build_bilingual_ass_from_doc, parse, AssOptions, BilingualLayout};
    let doc = parse(wp_subtitle::SubtitleFormat::Srt, AI_SOURCE_SRT)
        .map_err(|e| format!("解析 AI 源字幕失败：{e}"))?;
    let translations: Vec<String> = vec![
        "Welcome to this player".into(),
        "Subtitles support bilingual display".into(),
        "This is an AI translation example".into(),
    ];
    let cues: Vec<wp_subtitle::Cue> = doc
        .cues
        .iter()
        .zip(translations.iter())
        .map(|(cue, text)| wp_subtitle::Cue {
            text: text.clone(),
            ..cue.clone()
        })
        .collect();
    let opts = AssOptions {
        layout: BilingualLayout::TranslatedBelow,
        margin_v: 40,
        font_size: 48,
        ..AssOptions::default()
    };
    let report = build_bilingual_ass_from_doc(&doc, &cues, &opts);
    write_text(&assets.join("bilingual.ass"), &report.content)?;
    println!(
        "[双语 ASS] matched={} missing={} events={}（wp_subtitle 直接生成）",
        report.matched,
        report.missing,
        report.content.matches("Dialogue:").count()
    );
    Ok(())
}

const KIT_README: &str = r##"# M2 字幕真机联调素材（m2_probe_kit 生成）

由 `cargo run -p wp-subtitle --example m2_probe_kit` 生成，对应文档 `docs/PLAYER_SUBTITLE_M0.md`。

## 目录

| 路径 | 内容 |
|---|---|
| `assets/*.srt|vtt|ass` | 外挂字幕用例（中文/日文/emoji、大字号样式、主+副、延迟、AI 双语源） |
| `assets/bilingual.ass` | 双语 ASS（原文上/译文下双样式，wp_subtitle 生成） |
| `assets/test-10s.mp4` | 10s testsrc2 彩条测试片（展示截图用，含时间戳） |
| `assets/static-10s.mp4` | 10s 静态背景测试片（字幕像素差分用，含时间戳） |
| `assets/embedded-*.mkv` | 内嵌 SRT 的 mkv（default / 非 default 两种轨标记，验证内嵌字幕） |
| `run_matrix.sh` | 20+ 用例矩阵：render_probe（Xvfb）截图 + 字幕带像素 AE 指标 |
| `raw/` `screens/` | 脚本产物：JSON/日志/PPM 与 PNG 截图（运行后生成） |

## 运行矩阵

```bash
cargo build -p wp-media-mpv --example render_probe
Xvfb :99 -screen 0 1280x800x24 &          # 若未运行
DISPLAY=:99 bash run_matrix.sh            # 生成 raw/case-metrics.txt + screens/*.png
```

或用一条命令（需 Xvfb 已起）：`cargo run -p wp-subtitle --example m2_probe_kit -- --run`
"##;
