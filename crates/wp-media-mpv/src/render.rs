//! RenderBridge（Mode A）：用 **进程内 GL render API** 把 mpv 画面贴进 egui。
//!
//! 这是用户指定方案：**不做原生窗口嵌入**，视频作为 egui 的一层绘制内容，
//! UI（控制条/字幕面板/弹幕/AI HUD）可以自由覆盖在视频上。
//!
//! # 集成步骤（GUI 侧照抄即可）
//!
//! ```no_run
//! # use wp_media_mpv::{MpvConfig, MpvEngine};
//! # use wp_media_mpv::render::MpvRenderContext;
//! # fn demo(cc: &eframe::CreationContext<'_>) -> Result<(), Box<dyn std::error::Error>> {
//! // ① 引擎（无 libmpv 时返回可读 Err，不 panic）
//! let mut engine = MpvEngine::new(MpvConfig::default())?;
//!
//! // ② 渲染桥：`new` 只保存参数，不要求 GL 上下文 current；
//! //    真正的 mpv_render_context_create 在首次绘制回调里（那时 GL 一定 current）。
//! let render = MpvRenderContext::new(
//!     engine.render_source()?,
//!     cc.get_proc_address.clone().expect("需要 eframe 的 glow 后端"),
//!     cc.egui_ctx.clone(), // 新帧时自动 request_repaint
//! )?;
//!
//! // ③ 播放页每帧（在 egui 布局里）：
//! //    let _ = render.paint_video(ui, egui::vec2(video_w, video_h));
//!
//! // ④ App::on_exit 必须按序释放（否则 mpv/render ctx 泄漏或退出时崩）：
//! //    render.destroy(glow_context);  // eframe on_exit 的 `gl` 参数
//! //    engine.shutdown();
//! # Ok(()) }
//! ```
//!
//! 若要在 `CreationContext` 阶段就初始化 GL（可选）：
//! `render.ensure_init(cc.gl.as_deref().expect("glow"))?`（仅当 GL current 时可用）。
//!
//! # 实现要点
//!
//! - mpv 渲染到**自管 FBO + 纹理**（尺寸 = 视频矩形物理像素，随窗口缩放重建）；
//! - 然后在 `egui_glow::CallbackFn` 里用一个小着色器把该纹理铺满视频矩形，
//!   即「egui 绘制回调内用当前 GL 上下文完成渲染 + 合成」；
//! - `MPV_RENDER_PARAM_ADVANCED_CONTROL` + update callback + `report_swap()` 做帧同步：
//!   mpv 有新帧才回调 → `egui::Context::request_repaint()`，避免无意义 60fps 重绘；
//! - mpv render API 的线程规则：render context 的**创建/渲染/释放全部在 GL 线程**
//!   （即 egui 绘制回调内），绝不调用普通 libmpv API。
//!
//! # 线程安全
//!
//! [`MpvRenderContext`] 内部是 `Arc<RenderShared>`，`Send + Sync`，可以 clone 后
//! 放进 `'static + Send + Sync` 的 `CallbackFn` 闭包；GL 资源只在 GL 线程访问
//! （由 egui 绘制回调保证）。

use std::ffi::{c_void, CStr};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use glow::HasContext as _;
use wp_core::{Error, Result};

use crate::engine::RenderSource;
use crate::ffi::{self, MpvApi, MpvOpenglFbo, MpvOpenglInitParams, MpvRenderParam};

/// GL 的 `get_proc_address`（eframe `CreationContext::get_proc_address` 的类型）。
pub type GetProcAddress = Arc<dyn Fn(&CStr) -> *const c_void + Send + Sync>;

/// 渲染统计（性能面板/E2 实验用）。
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderStats {
    /// egui 绘制回调被调用次数。
    pub paint_calls: u64,
    /// 实际从 mpv 取到并渲染的新帧数。
    pub frames_rendered: u64,
    /// 回调里「无新帧」只做合成的次数。
    pub frames_reused: u64,
    /// 最近一次 `mpv_render_context_update()` 的 flags。
    pub last_update_flags: u64,
    /// 是否已渲染过首帧。
    pub first_frame: bool,
    /// 最近一次 GL 错误码（0 表示无）。
    pub gl_error: u32,
}

/// 自管离屏目标（高级用法/实验：拿 GL 纹理句柄自己合成）。
#[derive(Debug, Clone, Copy)]
pub struct OffscreenTexture {
    /// GL framebuffer id。
    pub framebuffer: u32,
    /// GL 颜色纹理 id（mpv 画面）。
    pub texture: u32,
    pub width: i32,
    pub height: i32,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// `get_proc_address` 的 C 回调上下文（堆上稳定地址，render ctx 存活期间有效）。
struct ProcAddressCtx {
    function: GetProcAddress,
}

unsafe extern "C" fn gl_get_proc_address(
    ctx: *mut c_void,
    name: *const std::ffi::c_char,
) -> *mut c_void {
    if ctx.is_null() || name.is_null() {
        return std::ptr::null_mut();
    }
    let proc_ctx = unsafe { &*(ctx as *const ProcAddressCtx) };
    let name = unsafe { CStr::from_ptr(name) };
    (proc_ctx.function)(name) as *mut c_void
}

/// mpv render context 的「有新帧」回调：只置标志 + 请求 egui 重绘（不得调用 mpv API）。
unsafe extern "C" fn on_mpv_update(ctx: *mut c_void) {
    if ctx.is_null() {
        return;
    }
    let shared = unsafe { &*(ctx as *const RenderShared) };
    shared.pending.store(true, Ordering::Release);
    shared.egui_ctx.request_repaint();
    let hook = lock(&shared.redraw_hook).clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// 自管 GL 目标（FBO + 颜色纹理 + 合成用着色器/几何）。
struct RenderTarget {
    framebuffer: glow::Framebuffer,
    texture: glow::Texture,
    program: glow::Program,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    u_tex: glow::UniformLocation,
    width: i32,
    height: i32,
}

impl RenderTarget {
    fn framebuffer_id(&self) -> i32 {
        self.framebuffer.0.get() as i32
    }

    fn texture_id(&self) -> u32 {
        self.texture.0.get()
    }
}

fn framebuffer_from_id(id: i32) -> Option<glow::Framebuffer> {
    NonZeroU32::new(id as u32).map(glow::NativeFramebuffer)
}

/// GL 渲染状态快照（回调结束前恢复，避免破坏 egui 的绘制状态）。
///
/// 注意 `SCISSOR_BOX` 也必须保存：mpv render API 会改 scissor 状态，
/// 只恢复 `SCISSOR_TEST` 开关而不恢复 box 会残留裁剪矩形（GUI 实测：视频右侧/上侧被裁掉）。
struct SavedGlState {
    framebuffer: Option<glow::Framebuffer>,
    viewport: [i32; 4],
    scissor_box: [i32; 4],
    scissor: bool,
    blend: bool,
}

unsafe fn save_gl_state(gl: &glow::Context) -> SavedGlState {
    let framebuffer =
        framebuffer_from_id(unsafe { gl.get_parameter_i32(glow::FRAMEBUFFER_BINDING) });
    let mut viewport = [0i32; 4];
    unsafe { gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport) };
    let mut scissor_box = [0i32; 4];
    unsafe { gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut scissor_box) };
    SavedGlState {
        framebuffer,
        viewport,
        scissor_box,
        scissor: unsafe { gl.is_enabled(glow::SCISSOR_TEST) },
        blend: unsafe { gl.is_enabled(glow::BLEND) },
    }
}

unsafe fn restore_gl_state(gl: &glow::Context, saved: &SavedGlState) {
    unsafe {
        gl.bind_framebuffer(glow::FRAMEBUFFER, saved.framebuffer);
        gl.viewport(
            saved.viewport[0],
            saved.viewport[1],
            saved.viewport[2],
            saved.viewport[3],
        );
        gl.scissor(
            saved.scissor_box[0],
            saved.scissor_box[1],
            saved.scissor_box[2],
            saved.scissor_box[3],
        );
        if saved.scissor {
            gl.enable(glow::SCISSOR_TEST);
        } else {
            gl.disable(glow::SCISSOR_TEST);
        }
        if saved.blend {
            gl.enable(glow::BLEND);
        } else {
            gl.disable(glow::BLEND);
        }
    }
}

/// 内部状态（只在 GL 线程访问，用 `Mutex` 避免误用时数据竞争）。
struct GlState {
    render_ctx: *mut ffi::mpv_render_context,
    proc_ctx: *mut ProcAddressCtx,
    target: Option<RenderTarget>,
    gl_objects_created: bool,
}

// SAFETY: 字段里的裸指针只由 GL 线程（egui 绘制回调）创建/渲染/释放，
// 由 `state` Mutex 串行化；`MpvRenderContext` 的 Send/Sync 由外层保证。
unsafe impl Send for GlState {}

/// 渲染桥共享状态（`Arc` 里，可跨线程 clone）。
struct RenderShared {
    source: RenderSource,
    get_proc_address: Option<GetProcAddress>,
    egui_ctx: egui::Context,
    redraw_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    state: Mutex<GlState>,
    pending: AtomicBool,
    first_frame: AtomicBool,
    stats: Mutex<RenderStats>,
    last_error: Mutex<Option<String>>,
}

// SAFETY: GL 资源只经 `state` Mutex 在 GL 线程访问；其它字段均线程安全。
// update callback（mpv 内部线程）只访问 pending/egui_ctx/hook。
unsafe impl Send for RenderShared {}
unsafe impl Sync for RenderShared {}

impl RenderShared {
    fn api(&self) -> &Arc<MpvApi> {
        self.source.core.api()
    }

    fn record_error(&self, message: String) {
        *lock(&self.last_error) = Some(message);
    }

    /// 创建 mpv render context（GL current 时调用；幂等）。
    fn ensure_init(&self, _gl: &glow::Context) -> Result<()> {
        let mut state = lock(&self.state);
        if !state.render_ctx.is_null() {
            return Ok(());
        }
        let raw_handle = self.source.core.raw_handle();
        if raw_handle.is_null() {
            return Err(Error::other("mpv 句柄已销毁，无法创建渲染桥"));
        }
        let Some(get_proc_address) = self.get_proc_address.clone() else {
            return Err(Error::unsupported(
                "缺少 GL get_proc_address（需要 eframe 的 glow 后端；wgpu 后端不支持 Mode A）",
            ));
        };
        let proc_ctx = Box::into_raw(Box::new(ProcAddressCtx {
            function: get_proc_address,
        }));
        let mut init_params = MpvOpenglInitParams {
            get_proc_address: Some(gl_get_proc_address),
            get_proc_address_ctx: proc_ctx as *mut c_void,
        };
        // 帧同步：让 mpv 用 update callback + report_swap 驱动，而不是内部 sleep
        let mut advanced_control: i32 = 1;
        let mut params = [
            MpvRenderParam {
                type_: ffi::MPV_RENDER_PARAM_API_TYPE,
                data: ffi::MPV_RENDER_API_TYPE_OPENGL.as_ptr() as *mut c_void,
            },
            MpvRenderParam {
                type_: ffi::MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: &mut init_params as *mut MpvOpenglInitParams as *mut c_void,
            },
            MpvRenderParam {
                type_: ffi::MPV_RENDER_PARAM_ADVANCED_CONTROL,
                data: &mut advanced_control as *mut i32 as *mut c_void,
            },
            MpvRenderParam {
                type_: ffi::MPV_RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        let mut render_ctx: *mut ffi::mpv_render_context = std::ptr::null_mut();
        let rc = unsafe {
            (self.api().mpv_render_context_create)(&mut render_ctx, raw_handle, params.as_mut_ptr())
        };
        if rc < 0 {
            // 释放临时 proc ctx
            drop(unsafe { Box::from_raw(proc_ctx) });
            return Err(Error::other(format!(
                "mpv_render_context_create 失败（{}）：{}；\
                 提示：当前 GL 上下文/驱动不支持 render API（软件渲染或缺少 hwdec interop 时常见）",
                rc,
                self.api().error_string(rc)
            )));
        }
        state.render_ctx = render_ctx;
        state.proc_ctx = proc_ctx;
        // update callback：ctx 指向 Arc<RenderShared> 内的稳定地址
        let self_ptr = self as *const RenderShared as *mut c_void;
        unsafe {
            (self.api().mpv_render_context_set_update_callback)(
                render_ctx,
                Some(on_mpv_update),
                self_ptr,
            );
        }
        // 通知引擎：render context 已就绪（引擎会执行排队中的 open；见 engine.rs）
        self.source.mark_render_ready();
        Ok(())
    }

    /// 确保离屏目标存在且尺寸匹配（GL current 时调用）。
    fn ensure_target(&self, gl: &glow::Context, width: i32, height: i32) -> Result<()> {
        let mut state = lock(&self.state);
        if let Some(target) = &state.target {
            if target.width == width && target.height == height {
                return Ok(());
            }
        }
        if let Some(old) = state.target.take() {
            unsafe { destroy_target(gl, old) };
        }
        let target = unsafe { create_target(gl, width, height)? };
        state.target = Some(target);
        state.gl_objects_created = true;
        Ok(())
    }

    /// 渲染当前视频帧到自管离屏目标（GL current 时调用）。
    fn render_into_target(&self, gl: &glow::Context) -> Result<bool> {
        let api = self.api().clone();
        let state = lock(&self.state);
        let render_ctx = state.render_ctx;
        if render_ctx.is_null() {
            return Ok(false);
        }
        // 询问 mpv 是否有新帧（ADVANCED_CONTROL 规范）
        let flags = unsafe { (api.mpv_render_context_update)(render_ctx) };
        lock(&self.stats).last_update_flags = flags;
        let has_frame = flags & ffi::MPV_RENDER_UPDATE_FRAME != 0;
        if !has_frame {
            return Ok(false);
        }
        let Some(target) = &state.target else {
            return Ok(false);
        };
        let (fbo_id, width, height) = (target.framebuffer_id(), target.width, target.height);
        let mut fbo = MpvOpenglFbo {
            fbo: fbo_id,
            w: width,
            h: height,
            internal_format: glow::RGBA8 as i32,
        };
        // 朝向（M0-E2 实测校准）：mpv 渲染到 FBO 默认输出是「行 0 = 画面顶部」
        // （UI 习惯），而 GL 纹理采样 v=0 对应纹理行 0（屏幕底部）⇒ 直接采样会上下颠倒。
        // 置 FLIP_Y=1 让 mpv 按 GL 约定输出（纹理 v=0 = 画面底部），合成即可正向显示。
        let mut flip_y: i32 = 1;
        let mut params = [
            MpvRenderParam {
                type_: ffi::MPV_RENDER_PARAM_OPENGL_FBO,
                data: &mut fbo as *mut MpvOpenglFbo as *mut c_void,
            },
            MpvRenderParam {
                type_: ffi::MPV_RENDER_PARAM_FLIP_Y,
                data: &mut flip_y as *mut i32 as *mut c_void,
            },
            MpvRenderParam {
                type_: ffi::MPV_RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(target.framebuffer));
            gl.viewport(0, 0, width, height);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::BLEND);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }
        let rc = unsafe { (api.mpv_render_context_render)(render_ctx, params.as_mut_ptr()) };
        if rc < 0 {
            return Err(Error::other(format!(
                "mpv_render_context_render 失败（{rc}）：{}",
                api.error_string(rc)
            )));
        }
        Ok(true)
    }

    /// egui 绘制回调入口：渲染 + 合成 + 帧同步（GL current）。
    fn paint(&self, info: egui::PaintCallbackInfo, gl: &glow::Context) -> Result<()> {
        self.ensure_init(gl)?;
        let viewport = info.viewport_in_pixels();
        let width = viewport.width_px.max(1);
        let height = viewport.height_px.max(1);
        self.ensure_target(gl, width, height)?;

        let saved = unsafe { save_gl_state(gl) };
        let rendered = self.render_into_target(gl)?;

        // mpv render API 明确：`mpv_render_context_render()` 返回后 **GL 状态未定义**
        // （framebuffer / viewport / scissor / blend 都可能被改）。合成前必须把 egui
        // 期望的状态恢复回来，否则画面会画到错误的位置/尺寸（GUI 实测过：整块偏移）。
        unsafe { restore_gl_state(gl, &saved) };

        // 再显式按本回调的 clip_rect 设置 scissor（与 egui painter 的 set_clip_rect 一致）：
        // 只恢复 SCISSOR_TEST 开关而不恢复 box 会残留 mpv 的裁剪矩形 → 视频被裁边。
        {
            let clip = info.clip_rect_in_pixels();
            unsafe {
                gl.enable(glow::SCISSOR_TEST);
                gl.scissor(
                    clip.left_px,
                    clip.from_bottom_px,
                    clip.width_px.max(0),
                    clip.height_px.max(0),
                );
            }
        }

        // 合成：把 FBO 纹理铺满回调矩形（viewport 已恢复为 egui 给的回调矩形）
        {
            let state = lock(&self.state);
            if let Some(target) = &state.target {
                unsafe { composite_target(gl, target) };
            }
        }
        unsafe { restore_gl_state(gl, &saved) };

        if rendered {
            let state = lock(&self.state);
            if !state.render_ctx.is_null() {
                unsafe { (self.api().mpv_render_context_report_swap)(state.render_ctx) };
            }
            drop(state);
            self.first_frame.store(true, Ordering::Release);
        }

        // 统计 + GL 错误检查
        {
            let gl_error = unsafe { gl.get_error() };
            let mut stats = lock(&self.stats);
            stats.paint_calls += 1;
            if rendered {
                stats.frames_rendered += 1;
            } else {
                stats.frames_reused += 1;
            }
            stats.first_frame = self.first_frame.load(Ordering::Acquire);
            if gl_error != 0 {
                stats.gl_error = gl_error;
            }
        }
        self.pending.store(false, Ordering::Release);
        Ok(())
    }

    /// 释放 GL 对象与 mpv render context（GL current 时调用；幂等）。
    fn destroy(&self, gl: Option<&glow::Context>) {
        let mut state = lock(&self.state);
        let render_ctx = state.render_ctx;
        if !render_ctx.is_null() {
            // 先摘掉 update callback，避免释放过程中被回调
            unsafe {
                (self.api().mpv_render_context_set_update_callback)(
                    render_ctx,
                    None,
                    std::ptr::null_mut(),
                );
            }
            if let Some(gl) = gl {
                if let Some(target) = state.target.take() {
                    unsafe { destroy_target(gl, target) };
                }
            } else if state.gl_objects_created {
                // 没有当前 GL 上下文：GL 对象只能泄漏（mpv render ctx 必须 GL current 才能释放干净）
                #[cfg(debug_assertions)]
                eprintln!(
                    "[wp-media-mpv] 警告：MpvRenderContext::destroy 未拿到 GL 上下文，\
                     将跳过 GL 对象释放（请在 App::on_exit 传入 gl）"
                );
            }
            unsafe { (self.api().mpv_render_context_free)(render_ctx) };
            state.render_ctx = std::ptr::null_mut();
        }
        if !state.proc_ctx.is_null() {
            drop(unsafe { Box::from_raw(state.proc_ctx) });
            state.proc_ctx = std::ptr::null_mut();
        }
        state.target = None;
        state.gl_objects_created = false;
    }
}

impl Drop for RenderShared {
    fn drop(&mut self) {
        let state = lock(&self.state);
        if !state.render_ctx.is_null() {
            // 没走 destroy：render ctx 泄漏是唯一安全选择（无 GL 上下文时 free 可能崩）。
            #[cfg(debug_assertions)]
            eprintln!(
                "[wp-media-mpv] 警告：MpvRenderContext 未调用 destroy() 就被释放，\
                 mpv render context 将泄漏；请在 App::on_exit 里调用 destroy(gl)"
            );
        }
    }
}

// ---------------- GL 资源创建/销毁（全部在 GL 线程） ----------------

/// 根据 GL 版本选择着色器（纯逻辑，便于单测）。
fn shader_sources(embedded: bool, major: u32, minor: u32) -> (&'static str, &'static str) {
    if embedded {
        if major >= 3 {
            (
                "#version 300 es\n\
                 in vec2 a_pos;\n\
                 in vec2 a_uv;\n\
                 out vec2 v_uv;\n\
                 void main() { v_uv = a_uv; gl_Position = vec4(a_pos, 0.0, 1.0); }\n",
                "#version 300 es\n\
                 precision mediump float;\n\
                 in vec2 v_uv;\n\
                 out vec4 f_color;\n\
                 uniform sampler2D u_tex;\n\
                 void main() { f_color = texture(u_tex, v_uv); }\n",
            )
        } else {
            (
                "#version 100\n\
                 attribute vec2 a_pos;\n\
                 attribute vec2 a_uv;\n\
                 varying vec2 v_uv;\n\
                 void main() { v_uv = a_uv; gl_Position = vec4(a_pos, 0.0, 1.0); }\n",
                "#version 100\n\
                 precision mediump float;\n\
                 varying vec2 v_uv;\n\
                 uniform sampler2D u_tex;\n\
                 void main() { gl_FragColor = texture2D(u_tex, v_uv); }\n",
            )
        }
    } else if major >= 3 && minor >= 2 {
        (
            "#version 150\n\
             in vec2 a_pos;\n\
             in vec2 a_uv;\n\
             out vec2 v_uv;\n\
             void main() { v_uv = a_uv; gl_Position = vec4(a_pos, 0.0, 1.0); }\n",
            "#version 150\n\
             in vec2 v_uv;\n\
             out vec4 f_color;\n\
             uniform sampler2D u_tex;\n\
             void main() { f_color = texture(u_tex, v_uv); }\n",
        )
    } else {
        (
            "#version 120\n\
             attribute vec2 a_pos;\n\
             attribute vec2 a_uv;\n\
             varying vec2 v_uv;\n\
             void main() { v_uv = a_uv; gl_Position = vec4(a_pos, 0.0, 1.0); }\n",
            "#version 120\n\
             varying vec2 v_uv;\n\
             uniform sampler2D u_tex;\n\
             void main() { gl_FragColor = texture2D(u_tex, v_uv); }\n",
        )
    }
}

unsafe fn compile_shader(
    gl: &glow::Context,
    kind: u32,
    source: &str,
    what: &str,
) -> Result<glow::Shader> {
    let shader = unsafe { gl.create_shader(kind) }
        .map_err(|e| Error::other(format!("创建 {what} 着色器失败：{e}")))?;
    unsafe {
        gl.shader_source(shader, source);
        gl.compile_shader(shader);
    }
    if !unsafe { gl.get_shader_compile_status(shader) } {
        let log = unsafe { gl.get_shader_info_log(shader) };
        unsafe { gl.delete_shader(shader) };
        return Err(Error::other(format!("{what} 着色器编译失败：{log}")));
    }
    Ok(shader)
}

unsafe fn create_program(gl: &glow::Context) -> Result<glow::Program> {
    let version = gl.version().clone();
    let (vertex_source, fragment_source) =
        shader_sources(version.is_embedded, version.major, version.minor);
    let vertex = unsafe { compile_shader(gl, glow::VERTEX_SHADER, vertex_source, "顶点") }?;
    let fragment =
        match unsafe { compile_shader(gl, glow::FRAGMENT_SHADER, fragment_source, "片元") } {
            Ok(fragment) => fragment,
            Err(e) => {
                unsafe { gl.delete_shader(vertex) };
                return Err(e);
            }
        };
    let program = unsafe { gl.create_program() }
        .map_err(|e| Error::other(format!("创建 GL program 失败：{e}")))?;
    unsafe {
        gl.attach_shader(program, vertex);
        gl.attach_shader(program, fragment);
        gl.link_program(program);
    }
    let linked = unsafe { gl.get_program_link_status(program) };
    let log = if linked {
        String::new()
    } else {
        unsafe { gl.get_program_info_log(program) }
    };
    unsafe {
        gl.detach_shader(program, vertex);
        gl.detach_shader(program, fragment);
        gl.delete_shader(vertex);
        gl.delete_shader(fragment);
    }
    if !linked {
        unsafe { gl.delete_program(program) };
        return Err(Error::other(format!("GL program 链接失败：{log}")));
    }
    Ok(program)
}

unsafe fn create_target(gl: &glow::Context, width: i32, height: i32) -> Result<RenderTarget> {
    let program = unsafe { create_program(gl) }?;

    // 纹理
    let texture = match unsafe { gl.create_texture() } {
        Ok(texture) => texture,
        Err(e) => {
            unsafe { gl.delete_program(program) };
            return Err(Error::other(format!("创建视频纹理失败：{e}")));
        }
    };
    unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            width,
            height,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_T,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.bind_texture(glow::TEXTURE_2D, None);
    }

    // FBO
    let framebuffer = match unsafe { gl.create_framebuffer() } {
        Ok(framebuffer) => framebuffer,
        Err(e) => {
            unsafe {
                gl.delete_texture(texture);
                gl.delete_program(program);
            }
            return Err(Error::other(format!("创建 FBO 失败：{e}")));
        }
    };
    unsafe {
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(texture),
            0,
        );
    }
    let status = unsafe { gl.check_framebuffer_status(glow::FRAMEBUFFER) };
    unsafe { gl.bind_framebuffer(glow::FRAMEBUFFER, None) };
    if status != glow::FRAMEBUFFER_COMPLETE {
        unsafe {
            gl.delete_framebuffer(framebuffer);
            gl.delete_texture(texture);
            gl.delete_program(program);
        }
        return Err(Error::other(format!(
            "FBO 不完整（status=0x{status:x}），无法渲染视频"
        )));
    }

    // 顶点数据：两个三角形组成矩形（TRIANGLE_STRIP）
    // 顶点位置（NDC）+ 纹理坐标；纹理 v=0 在底部（与 GL/mpv 输出一致）
    #[rustfmt::skip]
    let vertices: [f32; 16] = [
        -1.0, -1.0, 0.0, 0.0,
         1.0, -1.0, 1.0, 0.0,
        -1.0,  1.0, 0.0, 1.0,
         1.0,  1.0, 1.0, 1.0,
    ];
    let vbo = match unsafe { gl.create_buffer() } {
        Ok(vbo) => vbo,
        Err(e) => {
            unsafe {
                gl.delete_framebuffer(framebuffer);
                gl.delete_texture(texture);
                gl.delete_program(program);
            }
            return Err(Error::other(format!("创建顶点缓冲失败：{e}")));
        }
    };
    let vao = match unsafe { gl.create_vertex_array() } {
        Ok(vao) => vao,
        Err(e) => {
            unsafe {
                gl.delete_buffer(vbo);
                gl.delete_framebuffer(framebuffer);
                gl.delete_texture(texture);
                gl.delete_program(program);
            }
            return Err(Error::other(format!(
                "创建 VAO 失败：{e}；当前 GL 上下文可能过旧（需要 OpenGL 2.1+/GLES3）"
            )));
        }
    };
    let a_pos = unsafe { gl.get_attrib_location(program, "a_pos") }
        .ok_or_else(|| Error::other("着色器缺少 a_pos 属性（GL program 异常）"));
    let a_pos = match a_pos {
        Ok(location) => location,
        Err(e) => {
            unsafe {
                gl.delete_vertex_array(vao);
                gl.delete_buffer(vbo);
                gl.delete_framebuffer(framebuffer);
                gl.delete_texture(texture);
                gl.delete_program(program);
            }
            return Err(e);
        }
    };
    let a_uv = unsafe { gl.get_attrib_location(program, "a_uv") }.unwrap_or(a_pos + 1);
    let u_tex = unsafe { gl.get_uniform_location(program, "u_tex") };
    unsafe {
        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        gl.buffer_data_u8_slice(
            glow::ARRAY_BUFFER,
            bytemuck_f32_slice(&vertices),
            glow::STATIC_DRAW,
        );
        gl.enable_vertex_attrib_array(a_pos);
        gl.vertex_attrib_pointer_f32(a_pos, 2, glow::FLOAT, false, 4 * 4, 0);
        gl.enable_vertex_attrib_array(a_uv);
        gl.vertex_attrib_pointer_f32(a_uv, 2, glow::FLOAT, false, 4 * 4, 2 * 4);
        gl.bind_buffer(glow::ARRAY_BUFFER, None);
        gl.bind_vertex_array(None);
    }
    let Some(u_tex) = u_tex else {
        unsafe {
            gl.delete_vertex_array(vao);
            gl.delete_buffer(vbo);
            gl.delete_framebuffer(framebuffer);
            gl.delete_texture(texture);
            gl.delete_program(program);
        }
        return Err(Error::other("着色器缺少 u_tex uniform"));
    };

    Ok(RenderTarget {
        framebuffer,
        texture,
        program,
        vao,
        vbo,
        u_tex,
        width,
        height,
    })
}

/// `&[f32]` → `&[u8]`（小工具，避免引入 bytemuck 依赖）。
fn bytemuck_f32_slice(floats: &[f32]) -> &[u8] {
    // SAFETY: f32 任意位模式都是合法字节序列；只读视图。
    unsafe {
        std::slice::from_raw_parts(floats.as_ptr() as *const u8, std::mem::size_of_val(floats))
    }
}

unsafe fn composite_target(gl: &glow::Context, target: &RenderTarget) {
    unsafe {
        gl.disable(glow::BLEND);
        gl.use_program(Some(target.program));
        gl.bind_vertex_array(Some(target.vao));
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(target.texture));
        gl.uniform_1_i32(Some(&target.u_tex), 0);
        gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        gl.bind_texture(glow::TEXTURE_2D, None);
        gl.bind_vertex_array(None);
        gl.use_program(None);
    }
}

unsafe fn destroy_target(gl: &glow::Context, target: RenderTarget) {
    unsafe {
        gl.delete_vertex_array(target.vao);
        gl.delete_buffer(target.vbo);
        gl.delete_framebuffer(target.framebuffer);
        gl.delete_texture(target.texture);
        gl.delete_program(target.program);
    }
}

// ============================ 公开类型 ============================

/// Mode A 渲染桥：把 mpv 画面渲染进 egui（进程内 GL，可被 UI 覆盖）。
///
/// `Clone` 是浅拷贝（`Arc`），可安全放进 `egui_glow::CallbackFn` 闭包。
#[derive(Clone)]
pub struct MpvRenderContext {
    shared: Arc<RenderShared>,
}

impl MpvRenderContext {
    /// 创建渲染桥。
    ///
    /// - `source`：[`MpvEngine::render_source`](crate::MpvEngine::render_source)；
    /// - `get_proc_address`：eframe `CreationContext::get_proc_address`（glow 后端才有）；
    /// - `egui_ctx`：新帧到达时自动 `request_repaint`，让 egui 恰好在新帧时重绘。
    ///
    /// **不要求当前 GL 上下文**：mpv render context 延迟到首次绘制回调里创建。
    pub fn new(
        source: RenderSource,
        get_proc_address: GetProcAddress,
        egui_ctx: egui::Context,
    ) -> Result<Self> {
        Ok(Self {
            shared: Arc::new(RenderShared {
                source,
                get_proc_address: Some(get_proc_address),
                egui_ctx,
                redraw_hook: Mutex::new(None),
                state: Mutex::new(GlState {
                    render_ctx: std::ptr::null_mut(),
                    proc_ctx: std::ptr::null_mut(),
                    target: None,
                    gl_objects_created: false,
                }),
                pending: AtomicBool::new(false),
                first_frame: AtomicBool::new(false),
                stats: Mutex::new(RenderStats::default()),
                last_error: Mutex::new(None),
            }),
        })
    }

    /// 当前 GL 上下文可用时提前创建 mpv render context（可选；通常在首次绘制自动完成）。
    pub fn ensure_init(&self, gl: &glow::Context) -> Result<()> {
        self.shared.ensure_init(gl)
    }

    /// 渲染桥是否已创建 mpv render context（首帧绘制后为 true）。
    ///
    /// GUI 若想「先 open 再进播放页」，可等 `is_ready()` 为 true 再 open；
    /// 但即使直接 open 也不会丢视频：引擎会把 open 排队到渲染桥就绪后执行。
    pub fn is_ready(&self) -> bool {
        !lock(&self.shared.state).render_ctx.is_null()
    }

    /// 在 egui 视频矩形里绘制当前视频帧（**推荐入口**）。
    ///
    /// 内部自动添加 `egui::PaintCallback`：GL 渲染在 egui 绘制回调内、使用当前
    /// GL 上下文完成；UI 可以在返回后继续正常覆盖绘制。
    pub fn paint_video(&self, ui: &mut egui::Ui, size: egui::Vec2) -> egui::Response {
        let size = egui::vec2(size.x.max(1.0), size.y.max(1.0));
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
        self.paint_rect(ui, rect);
        response
    }

    /// 在指定矩形内绘制当前视频帧（自行分配空间时用）。
    pub fn paint_rect(&self, ui: &egui::Ui, rect: egui::Rect) {
        self.paint_with_painter(ui.painter(), rect);
    }

    /// 直接向 painter 添加绘制回调（用于自定义布局）。
    pub fn paint_with_painter(&self, painter: &egui::Painter, rect: egui::Rect) {
        if !rect.is_positive() {
            return;
        }
        let shared = self.shared.clone();
        let callback = egui::PaintCallback {
            rect,
            callback: Arc::new(egui_glow::CallbackFn::new(
                move |info: egui::PaintCallbackInfo, painter: &egui_glow::Painter| {
                    if let Err(e) = shared.paint(info, painter.gl()) {
                        shared.record_error(e.to_string());
                    }
                },
            )),
        };
        painter.add(callback);
    }

    /// 低层：渲染当前帧到外部 FBO（GL 线程调用）。
    ///
    /// Lead 建议的 `render(fbo, w, h)` 即此函数（`gl` 参数是 egui 绘制回调提供的
    /// 当前 GL 上下文；没有它无法调用 GL）。返回 `true` 表示本次确实渲染了新帧。
    pub fn render(&self, gl: &glow::Context, fbo: u32, w: i32, h: i32) -> Result<bool> {
        self.shared.ensure_init(gl)?;
        self.shared.ensure_target(gl, w.max(1), h.max(1))?;
        let api = self.shared.api().clone();
        let update_flags = {
            let state = lock(&self.shared.state);
            if state.render_ctx.is_null() {
                0
            } else {
                unsafe { (api.mpv_render_context_update)(state.render_ctx) }
            }
        };
        if update_flags & ffi::MPV_RENDER_UPDATE_FRAME == 0 {
            return Ok(false);
        }
        let mut fbo_param = MpvOpenglFbo {
            fbo: fbo as i32,
            w,
            h,
            internal_format: glow::RGBA8 as i32,
        };
        // 同 render_into_target：FLIP_Y=1 保证纹理方向与 GL 采样一致
        let mut flip_y: i32 = 1;
        let mut params = [
            MpvRenderParam {
                type_: ffi::MPV_RENDER_PARAM_OPENGL_FBO,
                data: &mut fbo_param as *mut MpvOpenglFbo as *mut c_void,
            },
            MpvRenderParam {
                type_: ffi::MPV_RENDER_PARAM_FLIP_Y,
                data: &mut flip_y as *mut i32 as *mut c_void,
            },
            MpvRenderParam {
                type_: ffi::MPV_RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        let state = lock(&self.shared.state);
        if state.render_ctx.is_null() {
            return Ok(false);
        }
        let rc = unsafe { (api.mpv_render_context_render)(state.render_ctx, params.as_mut_ptr()) };
        if rc < 0 {
            return Err(Error::other(format!(
                "mpv_render_context_render 失败（{rc}）：{}",
                api.error_string(rc)
            )));
        }
        Ok(true)
    }

    /// 低层：离屏渲染并返回自管 FBO 的 GL 句柄（高级用法；帧同步由调用方负责）。
    pub fn render_offscreen(
        &self,
        gl: &glow::Context,
        width: i32,
        height: i32,
    ) -> Result<OffscreenTexture> {
        self.shared.ensure_init(gl)?;
        self.shared.ensure_target(gl, width.max(1), height.max(1))?;
        let state = lock(&self.shared.state);
        let target = state
            .target
            .as_ref()
            .ok_or_else(|| Error::other("离屏渲染目标未创建"))?;
        Ok(OffscreenTexture {
            framebuffer: target.framebuffer_id() as u32,
            texture: target.texture_id(),
            width: target.width,
            height: target.height,
        })
    }

    /// `mpv_render_context_update()` 的 flags（GL 线程；`MPV_RENDER_UPDATE_FRAME=1` 表示有新帧）。
    pub fn update_flags(&self) -> u64 {
        let api = self.shared.api().clone();
        let state = lock(&self.shared.state);
        if state.render_ctx.is_null() {
            return 0;
        }
        unsafe { (api.mpv_render_context_update)(state.render_ctx) }
    }

    /// 通知 mpv「这一帧已经显示」——让帧同步/显示时间戳准确（GL 线程）。
    pub fn report_swap(&self) {
        let api = self.shared.api().clone();
        let state = lock(&self.shared.state);
        if state.render_ctx.is_null() {
            return;
        }
        unsafe { (api.mpv_render_context_report_swap)(state.render_ctx) };
    }

    /// 是否已有新帧待绘制（由 mpv update callback 置位；绘制后清零）。
    pub fn has_new_frame(&self) -> bool {
        self.shared.pending.load(Ordering::Acquire)
    }

    /// 注册新帧通知钩子（内部已自动 `egui::Context::request_repaint`，这里是追加钩子）。
    ///
    /// 回调在 mpv 内部线程触发，**不要**在钩子里调用任何 mpv API。
    pub fn set_update_callback(&self, hook: Box<dyn Fn() + Send + Sync>) {
        *lock(&self.shared.redraw_hook) = Some(Arc::from(hook));
    }

    /// 最新视频帧的 GL 纹理 id（自管 FBO 的颜色纹理；高级用法）。
    pub fn texture(&self) -> Option<u32> {
        lock(&self.shared.state)
            .target
            .as_ref()
            .map(RenderTarget::texture_id)
    }

    /// 读取当前离屏目标像素（RGBA8，GL 原点在左下；GL 线程调用）。
    ///
    /// 用途：GUI 截图（不依赖 mpv 的 screenshot 命令）、M0 实验的帧内容取证。
    /// 返回 `(width, height, rgba_pixels)`，像素行序为 GL 的 bottom-up。
    pub fn read_pixels(&self, gl: &glow::Context) -> Result<(i32, i32, Vec<u8>)> {
        let state = lock(&self.shared.state);
        let target = state
            .target
            .as_ref()
            .ok_or_else(|| Error::other("尚无渲染目标（先 paint 一帧再读像素）"))?;
        let (width, height) = (target.width, target.height);
        let mut buffer = vec![0u8; (width.max(1) * height.max(1) * 4) as usize];
        let previous =
            framebuffer_from_id(unsafe { gl.get_parameter_i32(glow::FRAMEBUFFER_BINDING) });
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(target.framebuffer));
            gl.read_pixels(
                0,
                0,
                width,
                height,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut buffer)),
            );
            gl.bind_framebuffer(glow::FRAMEBUFFER, previous);
        }
        Ok((width, height, buffer))
    }

    /// 渲染统计（性能面板/E2）。
    pub fn stats(&self) -> RenderStats {
        *lock(&self.shared.stats)
    }

    /// 是否已经渲染过首帧。
    pub fn is_first_frame(&self) -> bool {
        self.shared.first_frame.load(Ordering::Acquire)
    }

    /// 渲染桥内部错误（最近一次；如 GL 创建失败、mpv render 失败）。
    pub fn last_error(&self) -> Option<String> {
        lock(&self.shared.last_error).clone()
    }

    /// 主动触发 repaint（例如播放状态变化导致需要立即重绘时）。
    pub fn request_repaint(&self) {
        self.shared.egui_ctx.request_repaint();
    }

    /// 释放 render context 与 GL 资源（**GL 线程**，`App::on_exit` 里调用；幂等）。
    ///
    /// 传 `None` 时只释放 mpv render context、跳过 GL 对象（会泄漏 GL 资源并打警告）。
    /// 必须在 `MpvEngine::shutdown()` 之前调用（顺序见模块文档）。
    pub fn destroy(&self, gl: Option<&glow::Context>) {
        self.shared.destroy(gl);
    }
}

// ============================ 测试 ============================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_variants_by_gl_version() {
        // 桌面 GL 4.5 → #version 150
        let (vs, fs) = shader_sources(false, 4, 5);
        assert!(vs.starts_with("#version 150"));
        assert!(fs.starts_with("#version 150"));
        // 桌面 GL 2.1 → #version 120（attribute/varying 语法）
        let (vs, fs) = shader_sources(false, 2, 1);
        assert!(vs.starts_with("#version 120"));
        assert!(vs.contains("attribute"));
        assert!(fs.contains("gl_FragColor"));
        // GLES 3.0 → 300 es
        let (vs, fs) = shader_sources(true, 3, 0);
        assert!(vs.starts_with("#version 300 es"));
        assert!(fs.contains("precision mediump float"));
        // GLES 2.0 → 100
        let (vs, _fs) = shader_sources(true, 2, 0);
        assert!(vs.starts_with("#version 100"));
    }

    #[test]
    fn framebuffer_from_id_roundtrip() {
        assert!(framebuffer_from_id(0).is_none(), "0 = 默认 framebuffer");
        let fb = framebuffer_from_id(7).expect("有效 id");
        assert_eq!(fb.0.get(), 7);
    }

    #[test]
    fn bytemuck_view_keeps_bytes() {
        let floats = [1.0f32, -2.5, 3.25];
        let bytes = bytemuck_f32_slice(&floats);
        assert_eq!(bytes.len(), 12);
        assert_eq!(bytes.len(), std::mem::size_of_val(&floats));
    }
}
