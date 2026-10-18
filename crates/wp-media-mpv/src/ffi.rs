//! libmpv C API 的最小手写绑定（只覆盖本项目实际用到的符号与类型）。
//!
//! 为什么手写而不是 bindgen / mpv-sys：
//! - 我们只需要约 30 个符号，手写可审计、无 build.rs 依赖、Windows/Linux 一套代码；
//! - 结构体布局逐项对照 mpv `client.h` / `render.h` / `render_gl.h`（client API 2.5，mpv 0.41.0）；
//! - 版本不匹配时给可读错误，而不是链接期失败。
//!
//! 安全约定：
//! - 本模块内部允许 unsafe；对上层（engine/render）只暴露「安全包装」。
//! - [`RawMpv`] 的句柄只在引擎事件线程中使用（`mpv_render_context_create` 除外，
//!   该调用按 mpv 文档允许在 render 线程携带 handle 创建，句柄只作 core 引用）。
//! - `mpv_*` 返回的字符串/节点一律由本模块负责释放（`mpv_free` / `mpv_free_node_contents`）。

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_int, c_ulong, c_void, CStr, CString};
use std::sync::Arc;

use wp_core::{Error, Result};

// ============================ 常量 ============================

// mpv_format（client.h）——只用到的几种。
pub(crate) const MPV_FORMAT_NONE: c_int = 0;
pub(crate) const MPV_FORMAT_STRING: c_int = 1;
pub(crate) const MPV_FORMAT_FLAG: c_int = 3;
pub(crate) const MPV_FORMAT_INT64: c_int = 4;
pub(crate) const MPV_FORMAT_DOUBLE: c_int = 5;
pub(crate) const MPV_FORMAT_NODE: c_int = 6;
pub(crate) const MPV_FORMAT_NODE_ARRAY: c_int = 7;
pub(crate) const MPV_FORMAT_NODE_MAP: c_int = 8;

// mpv_event_id（client.h）。
pub(crate) const MPV_EVENT_NONE: c_int = 0;
pub(crate) const MPV_EVENT_SHUTDOWN: c_int = 1;
pub(crate) const MPV_EVENT_LOG_MESSAGE: c_int = 2;
pub(crate) const MPV_EVENT_START_FILE: c_int = 6;
pub(crate) const MPV_EVENT_END_FILE: c_int = 7;
pub(crate) const MPV_EVENT_FILE_LOADED: c_int = 8;
pub(crate) const MPV_EVENT_IDLE: c_int = 11;
pub(crate) const MPV_EVENT_VIDEO_RECONFIG: c_int = 17;
pub(crate) const MPV_EVENT_SEEK: c_int = 20;
pub(crate) const MPV_EVENT_PLAYBACK_RESTART: c_int = 21;
pub(crate) const MPV_EVENT_PROPERTY_CHANGE: c_int = 22;
pub(crate) const MPV_EVENT_QUEUE_OVERFLOW: c_int = 24;

// mpv_end_file_reason（client.h）。
pub(crate) const MPV_END_FILE_REASON_EOF: c_int = 0;
pub(crate) const MPV_END_FILE_REASON_STOP: c_int = 2;
pub(crate) const MPV_END_FILE_REASON_ERROR: c_int = 4;

// mpv_render_param_type（render.h）。
pub(crate) const MPV_RENDER_PARAM_INVALID: c_int = 0;
pub(crate) const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
pub(crate) const MPV_RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
pub(crate) const MPV_RENDER_PARAM_OPENGL_FBO: c_int = 3;
pub(crate) const MPV_RENDER_PARAM_FLIP_Y: c_int = 4;
pub(crate) const MPV_RENDER_PARAM_ADVANCED_CONTROL: c_int = 10;

/// `mpv_render_context_update()` 的返回值：有新帧需要渲染。
pub(crate) const MPV_RENDER_UPDATE_FRAME: u64 = 1 << 0;
/// `MPV_RENDER_API_TYPE_OPENGL` 字符串（含结尾 NUL，直接给 API 用）。
pub(crate) const MPV_RENDER_API_TYPE_OPENGL: &[u8] = b"opengl\0";

// ============================ 不透明类型 ============================

#[repr(C)]
pub(crate) struct mpv_handle {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct mpv_render_context {
    _private: [u8; 0],
}

// ============================ 结构体 ============================

/// `mpv_node` 的联合体部分（布局与 client.h 一致）。
#[repr(C)]
pub(crate) union MpvNodeUnion {
    pub string: *mut c_char,
    pub flag: c_int,
    pub int64: i64,
    pub double_: f64,
    pub list: *mut MpvNodeList,
    pub ba: *mut MpvByteArray,
}

#[repr(C)]
pub(crate) struct MpvNode {
    pub u: MpvNodeUnion,
    pub format: c_int,
}

#[repr(C)]
pub(crate) struct MpvNodeList {
    pub num: c_int,
    pub values: *mut MpvNode,
    pub keys: *mut *mut c_char,
}

#[repr(C)]
pub(crate) struct MpvByteArray {
    pub data: *mut c_void,
    pub size: usize,
}

#[repr(C)]
pub(crate) struct MpvEvent {
    pub event_id: c_int,
    pub error: c_int,
    pub reply_userdata: u64,
    pub data: *mut c_void,
}

#[repr(C)]
pub(crate) struct MpvEventProperty {
    pub name: *const c_char,
    pub format: c_int,
    pub data: *mut c_void,
}

#[repr(C)]
pub(crate) struct MpvEventEndFile {
    pub reason: c_int,
    pub error: c_int,
    pub playlist_entry_id: i64,
    pub playlist_insert_id: i64,
    pub playlist_insert_num_entries: c_int,
}

#[repr(C)]
pub(crate) struct MpvEventStartFile {
    pub playlist_entry_id: i64,
}

#[repr(C)]
pub(crate) struct MpvRenderParam {
    pub type_: c_int,
    pub data: *mut c_void,
}

#[repr(C)]
pub(crate) struct MpvOpenglInitParams {
    pub get_proc_address: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void>,
    pub get_proc_address_ctx: *mut c_void,
}

#[repr(C)]
pub(crate) struct MpvOpenglFbo {
    pub fbo: c_int,
    pub w: c_int,
    pub h: c_int,
    pub internal_format: c_int,
}

/// render context 的「有新帧」回调类型。
pub(crate) type MpvRenderUpdateFn = unsafe extern "C" fn(*mut c_void);

// ============================ 函数表 ============================

/// libmpv 的符号表（一次加载，全进程共享）。
///
/// 字段全部是 C 函数指针；调用点集中在 [`RawMpv`] 与 render 模块里，不向外泄漏。
pub(crate) struct MpvApi {
    pub mpv_client_api_version: unsafe extern "C" fn() -> c_ulong,
    pub mpv_error_string: unsafe extern "C" fn(c_int) -> *const c_char,
    pub mpv_free: unsafe extern "C" fn(*mut c_void),
    pub mpv_free_node_contents: unsafe extern "C" fn(*mut MpvNode),

    pub mpv_create: unsafe extern "C" fn() -> *mut mpv_handle,
    pub mpv_initialize: unsafe extern "C" fn(*mut mpv_handle) -> c_int,
    pub mpv_terminate_destroy: unsafe extern "C" fn(*mut mpv_handle),

    pub mpv_set_option_string:
        unsafe extern "C" fn(*mut mpv_handle, *const c_char, *const c_char) -> c_int,
    pub mpv_set_property_string:
        unsafe extern "C" fn(*mut mpv_handle, *const c_char, *const c_char) -> c_int,
    pub mpv_set_property:
        unsafe extern "C" fn(*mut mpv_handle, *const c_char, c_int, *mut c_void) -> c_int,
    pub mpv_get_property:
        unsafe extern "C" fn(*mut mpv_handle, *const c_char, c_int, *mut c_void) -> c_int,
    pub mpv_get_property_string:
        unsafe extern "C" fn(*mut mpv_handle, *const c_char) -> *mut c_char,
    pub mpv_observe_property:
        unsafe extern "C" fn(*mut mpv_handle, u64, *const c_char, c_int) -> c_int,

    pub mpv_command: unsafe extern "C" fn(*mut mpv_handle, *const *const c_char) -> c_int,
    pub mpv_command_string: unsafe extern "C" fn(*mut mpv_handle, *const c_char) -> c_int,

    pub mpv_wait_event: unsafe extern "C" fn(*mut mpv_handle, f64) -> *mut MpvEvent,
    pub mpv_request_log_messages: unsafe extern "C" fn(*mut mpv_handle, *const c_char) -> c_int,

    pub mpv_render_context_create: unsafe extern "C" fn(
        *mut *mut mpv_render_context,
        *mut mpv_handle,
        *mut MpvRenderParam,
    ) -> c_int,
    pub mpv_render_context_set_update_callback:
        unsafe extern "C" fn(*mut mpv_render_context, Option<MpvRenderUpdateFn>, *mut c_void),
    pub mpv_render_context_update: unsafe extern "C" fn(*mut mpv_render_context) -> u64,
    pub mpv_render_context_render:
        unsafe extern "C" fn(*mut mpv_render_context, *mut MpvRenderParam) -> c_int,
    pub mpv_render_context_report_swap: unsafe extern "C" fn(*mut mpv_render_context),
    pub mpv_render_context_free: unsafe extern "C" fn(*mut mpv_render_context),
}

impl MpvApi {
    /// 从已加载的动态库解析全部符号。
    ///
    /// 失败信息里带上库路径与缺失符号名，便于「用户装错 mpv / 版本过旧」时排查。
    pub(crate) unsafe fn load(lib: &libloading::Library, lib_path: &str) -> Result<Self> {
        macro_rules! sym {
            ($ty:ty, $name:literal) => {{
                let s: libloading::Symbol<'_, $ty> = unsafe { lib.get(concat!($name, "\0")) }
                    .map_err(|e| {
                        Error::unsupported(format!(
                            "libmpv（{lib_path}）缺少符号 {}：{e}；\
                         需要 mpv 0.41+ / client API ≥ 1.100 的完整 libmpv 构建",
                            $name
                        ))
                    })?;
                *s
            }};
        }
        // `mpv_request_log_messages` 在极老的库上可能不存在，但仍作为必需符号加载：
        // 我们的最低版本要求是 0.41（API 2.x），不存在即视为不可用。
        Ok(MpvApi {
            mpv_client_api_version: sym!(
                unsafe extern "C" fn() -> c_ulong,
                "mpv_client_api_version"
            ),
            mpv_error_string: sym!(
                unsafe extern "C" fn(c_int) -> *const c_char,
                "mpv_error_string"
            ),
            mpv_free: sym!(unsafe extern "C" fn(*mut c_void), "mpv_free"),
            mpv_free_node_contents: sym!(
                unsafe extern "C" fn(*mut MpvNode),
                "mpv_free_node_contents"
            ),
            mpv_create: sym!(unsafe extern "C" fn() -> *mut mpv_handle, "mpv_create"),
            mpv_initialize: sym!(
                unsafe extern "C" fn(*mut mpv_handle) -> c_int,
                "mpv_initialize"
            ),
            mpv_terminate_destroy: sym!(
                unsafe extern "C" fn(*mut mpv_handle),
                "mpv_terminate_destroy"
            ),
            mpv_set_option_string: sym!(
                unsafe extern "C" fn(*mut mpv_handle, *const c_char, *const c_char) -> c_int,
                "mpv_set_option_string"
            ),
            mpv_set_property_string: sym!(
                unsafe extern "C" fn(*mut mpv_handle, *const c_char, *const c_char) -> c_int,
                "mpv_set_property_string"
            ),
            mpv_set_property: sym!(
                unsafe extern "C" fn(*mut mpv_handle, *const c_char, c_int, *mut c_void) -> c_int,
                "mpv_set_property"
            ),
            mpv_get_property: sym!(
                unsafe extern "C" fn(*mut mpv_handle, *const c_char, c_int, *mut c_void) -> c_int,
                "mpv_get_property"
            ),
            mpv_get_property_string: sym!(
                unsafe extern "C" fn(*mut mpv_handle, *const c_char) -> *mut c_char,
                "mpv_get_property_string"
            ),
            mpv_observe_property: sym!(
                unsafe extern "C" fn(*mut mpv_handle, u64, *const c_char, c_int) -> c_int,
                "mpv_observe_property"
            ),
            mpv_command: sym!(
                unsafe extern "C" fn(*mut mpv_handle, *const *const c_char) -> c_int,
                "mpv_command"
            ),
            mpv_command_string: sym!(
                unsafe extern "C" fn(*mut mpv_handle, *const c_char) -> c_int,
                "mpv_command_string"
            ),
            mpv_wait_event: sym!(
                unsafe extern "C" fn(*mut mpv_handle, f64) -> *mut MpvEvent,
                "mpv_wait_event"
            ),
            mpv_request_log_messages: sym!(
                unsafe extern "C" fn(*mut mpv_handle, *const c_char) -> c_int,
                "mpv_request_log_messages"
            ),
            mpv_render_context_create: sym!(
                unsafe extern "C" fn(
                    *mut *mut mpv_render_context,
                    *mut mpv_handle,
                    *mut MpvRenderParam,
                ) -> c_int,
                "mpv_render_context_create"
            ),
            mpv_render_context_set_update_callback: sym!(
                unsafe extern "C" fn(
                    *mut mpv_render_context,
                    Option<MpvRenderUpdateFn>,
                    *mut c_void,
                ),
                "mpv_render_context_set_update_callback"
            ),
            mpv_render_context_update: sym!(
                unsafe extern "C" fn(*mut mpv_render_context) -> u64,
                "mpv_render_context_update"
            ),
            mpv_render_context_render: sym!(
                unsafe extern "C" fn(*mut mpv_render_context, *mut MpvRenderParam) -> c_int,
                "mpv_render_context_render"
            ),
            mpv_render_context_report_swap: sym!(
                unsafe extern "C" fn(*mut mpv_render_context),
                "mpv_render_context_report_swap"
            ),
            mpv_render_context_free: sym!(
                unsafe extern "C" fn(*mut mpv_render_context),
                "mpv_render_context_free"
            ),
        })
    }

    /// client API 版本（major, minor）。
    pub(crate) fn api_version(&self) -> (u32, u32) {
        let v = unsafe { (self.mpv_client_api_version)() };
        (((v >> 16) & 0xffff) as u32, (v & 0xffff) as u32)
    }

    /// mpv 错误码 → 可读文案。
    pub(crate) fn error_string(&self, code: c_int) -> String {
        let p = unsafe { (self.mpv_error_string)(code) };
        if p.is_null() {
            format!("mpv 错误码 {code}")
        } else {
            unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
        }
    }
}

// ============================ RawMpv ============================

/// 一根 mpv 句柄的安全包装。
///
/// - 创建/初始化/属性/命令/事件全部在这里收口；
/// - 只在引擎事件线程使用（`Send` 手动实现：句柄随线程移动，不共享）；
/// - `Drop` 调 `mpv_terminate_destroy`（若上层保证先释放 render context）。
pub(crate) struct RawMpv {
    pub(crate) api: Arc<MpvApi>,
    handle: *mut mpv_handle,
}

// SAFETY: 句柄由单一引擎线程独占使用；mpv client API 本身线程安全，
// 这里只是避免把原始指针的 Send 语义泄漏到 safe 代码。
unsafe impl Send for RawMpv {}

impl RawMpv {
    /// 创建未初始化的 mpv 句柄（之后可用 [`Self::set_option`] 设置选项再 [`Self::initialize`]）。
    pub(crate) fn create(api: Arc<MpvApi>) -> Result<Self> {
        let handle = unsafe { (api.mpv_create)() };
        if handle.is_null() {
            return Err(Error::other(
                "mpv_create() 返回 NULL：libmpv 初始化失败（可能是环境/依赖不完整）",
            ));
        }
        Ok(Self { api, handle })
    }

    /// 原始句柄（仅供 render 模块创建 render context 时作 core 引用）。
    pub(crate) fn raw_handle(&self) -> *mut mpv_handle {
        self.handle
    }

    pub(crate) fn initialize(&self) -> Result<()> {
        let rc = unsafe { (self.api.mpv_initialize)(self.handle) };
        self.check(rc, "mpv_initialize")
    }

    fn check(&self, rc: c_int, what: &str) -> Result<()> {
        if rc >= 0 {
            return Ok(());
        }
        Err(Error::other(format!(
            "{what} 失败（{}）：{}",
            rc,
            self.api.error_string(rc)
        )))
    }

    fn cname(&self, name: &str) -> Result<CString> {
        CString::new(name).map_err(|_| Error::other(format!("属性/选项名含 NUL 字节：{name:?}")))
    }

    /// 设置 mpv 选项（必须在 `mpv_initialize` 之前调用）。
    pub(crate) fn set_option(&self, name: &str, value: &str) -> Result<()> {
        let (n, v) = (
            self.cname(name)?,
            CString::new(value)
                .map_err(|_| Error::other(format!("选项 {name} 的值含 NUL 字节：{value:?}")))?,
        );
        let rc = unsafe { (self.api.mpv_set_option_string)(self.handle, n.as_ptr(), v.as_ptr()) };
        self.check(rc, &format!("设置选项 {name}={value}"))
    }

    /// 设置 mpv 选项，失败不算致命（用于可选特性/兼容旧版本）。
    pub(crate) fn try_set_option(&self, name: &str, value: &str) -> bool {
        self.set_option(name, value).is_ok()
    }

    fn check_prop(&self, rc: c_int, what: &str, name: &str) -> Result<()> {
        if rc >= 0 {
            return Ok(());
        }
        Err(Error::other(format!(
            "{what}({name}) 失败（{}）：{}",
            rc,
            self.api.error_string(rc)
        )))
    }

    /// 设置字符串属性。
    pub(crate) fn set_property_string(&self, name: &str, value: &str) -> Result<()> {
        let n = self.cname(name)?;
        let v = CString::new(value)
            .map_err(|_| Error::other(format!("属性 {name} 的值含 NUL 字节：{value:?}")))?;
        let rc = unsafe { (self.api.mpv_set_property_string)(self.handle, n.as_ptr(), v.as_ptr()) };
        self.check_prop(rc, "设置属性", name)
    }

    /// 设置 f64 属性。
    pub(crate) fn set_property_f64(&self, name: &str, value: f64) -> Result<()> {
        let n = self.cname(name)?;
        let mut v = value;
        let rc = unsafe {
            (self.api.mpv_set_property)(
                self.handle,
                n.as_ptr(),
                MPV_FORMAT_DOUBLE,
                &mut v as *mut f64 as *mut c_void,
            )
        };
        self.check_prop(rc, "设置属性", name)
    }

    /// 设置 i64 属性。
    pub(crate) fn set_property_i64(&self, name: &str, value: i64) -> Result<()> {
        let n = self.cname(name)?;
        let mut v = value;
        let rc = unsafe {
            (self.api.mpv_set_property)(
                self.handle,
                n.as_ptr(),
                MPV_FORMAT_INT64,
                &mut v as *mut i64 as *mut c_void,
            )
        };
        self.check_prop(rc, "设置属性", name)
    }

    /// 设置布尔属性。
    pub(crate) fn set_property_flag(&self, name: &str, value: bool) -> Result<()> {
        let n = self.cname(name)?;
        let mut v: c_int = i32::from(value);
        let rc = unsafe {
            (self.api.mpv_set_property)(
                self.handle,
                n.as_ptr(),
                MPV_FORMAT_FLAG,
                &mut v as *mut c_int as *mut c_void,
            )
        };
        self.check_prop(rc, "设置属性", name)
    }

    /// 设置字符串 **list** 属性（如 `http-header-fields`）。
    ///
    /// 用 `MPV_FORMAT_NODE_ARRAY` 逐项传递，而不是拼成逗号分隔字符串：
    /// mpv 的 list option 在字符串形式下按逗号拆分，header 值（Cookie 等）一旦含逗号
    /// 就会被拆碎。实测：夸克 HLS 需要 Cookie/Referer/UA 三项，必须逐项传
    /// （quark-stream 在 mpv CLI 侧也确认 `--http-header-fields-add` 才能追加）。
    pub(crate) fn set_property_string_list(&self, name: &str, values: &[String]) -> Result<()> {
        let n = self.cname(name)?;
        let cstrings = values
            .iter()
            .map(|v| {
                CString::new(v.as_bytes())
                    .map_err(|_| Error::other(format!("属性 {name} 的列表项含 NUL 字节：{v:?}")))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut nodes: Vec<MpvNode> = cstrings
            .iter()
            .map(|c| MpvNode {
                u: MpvNodeUnion {
                    string: c.as_ptr() as *mut c_char,
                },
                format: MPV_FORMAT_STRING,
            })
            .collect();
        let mut list = MpvNodeList {
            num: nodes.len() as c_int,
            values: nodes.as_mut_ptr(),
            keys: std::ptr::null_mut(),
        };
        let mut node = MpvNode {
            u: MpvNodeUnion { list: &mut list },
            format: MPV_FORMAT_NODE_ARRAY,
        };
        let rc = unsafe {
            (self.api.mpv_set_property)(
                self.handle,
                n.as_ptr(),
                MPV_FORMAT_NODE,
                &mut node as *mut MpvNode as *mut c_void,
            )
        };
        self.check_prop(rc, "设置列表属性", name)
    }

    /// 读字符串属性；不存在/不可读返回 `None`（不视为错误，属性观察里大量使用）。
    pub(crate) fn get_property_string(&self, name: &str) -> Option<String> {
        let n = CString::new(name).ok()?;
        let p = unsafe { (self.api.mpv_get_property_string)(self.handle, n.as_ptr()) };
        if p.is_null() {
            return None;
        }
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        unsafe { (self.api.mpv_free)(p as *mut c_void) };
        Some(s)
    }

    /// 读 f64 属性。
    pub(crate) fn get_property_f64(&self, name: &str) -> Option<f64> {
        let n = CString::new(name).ok()?;
        let mut v: f64 = 0.0;
        let rc = unsafe {
            (self.api.mpv_get_property)(
                self.handle,
                n.as_ptr(),
                MPV_FORMAT_DOUBLE,
                &mut v as *mut f64 as *mut c_void,
            )
        };
        (rc >= 0).then_some(v)
    }

    /// 读 i64 属性。
    pub(crate) fn get_property_i64(&self, name: &str) -> Option<i64> {
        let n = CString::new(name).ok()?;
        let mut v: i64 = 0;
        let rc = unsafe {
            (self.api.mpv_get_property)(
                self.handle,
                n.as_ptr(),
                MPV_FORMAT_INT64,
                &mut v as *mut i64 as *mut c_void,
            )
        };
        (rc >= 0).then_some(v)
    }

    /// 读布尔属性。
    pub(crate) fn get_property_flag(&self, name: &str) -> Option<bool> {
        let n = CString::new(name).ok()?;
        let mut v: c_int = 0;
        let rc = unsafe {
            (self.api.mpv_get_property)(
                self.handle,
                n.as_ptr(),
                MPV_FORMAT_FLAG,
                &mut v as *mut c_int as *mut c_void,
            )
        };
        (rc >= 0).then_some(v != 0)
    }

    /// 读节点属性并转成 JSON（track-list / video-params / demuxer-cache-state 用）。
    pub(crate) fn get_property_json(&self, name: &str) -> Option<serde_json::Value> {
        let n = CString::new(name).ok()?;
        let mut node = MpvNode {
            u: MpvNodeUnion { int64: 0 },
            format: MPV_FORMAT_NONE,
        };
        let rc = unsafe {
            (self.api.mpv_get_property)(
                self.handle,
                n.as_ptr(),
                MPV_FORMAT_NODE,
                &mut node as *mut MpvNode as *mut c_void,
            )
        };
        if rc < 0 {
            return None;
        }
        let json = unsafe { node_to_json(&node) };
        unsafe { (self.api.mpv_free_node_contents)(&mut node) };
        Some(json)
    }

    /// 观察属性（不携带数据，收到 `PROPERTY_CHANGE` 后自行 get）。
    pub(crate) fn observe_property_none(&self, name: &str) -> Result<()> {
        let n = self.cname(name)?;
        let rc =
            unsafe { (self.api.mpv_observe_property)(self.handle, 0, n.as_ptr(), MPV_FORMAT_NONE) };
        self.check_prop(rc, "观察属性", name)
    }

    /// 执行命令（参数列表，自动补 NULL 结尾）。
    pub(crate) fn command(&self, args: &[&str]) -> Result<()> {
        let cstrs = args
            .iter()
            .map(|a| {
                CString::new(*a).map_err(|_| Error::other(format!("命令参数含 NUL 字节：{a:?}")))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut ptrs: Vec<*const c_char> = cstrs.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        let rc = unsafe { (self.api.mpv_command)(self.handle, ptrs.as_ptr()) };
        self.check(rc, &format!("命令 {args:?}"))
    }

    /// 执行命令字符串（mpv 的 input.conf 语法）。
    pub(crate) fn command_string(&self, cmd: &str) -> Result<()> {
        let c = CString::new(cmd)
            .map_err(|_| Error::other(format!("命令字符串含 NUL 字节：{cmd:?}")))?;
        let rc = unsafe { (self.api.mpv_command_string)(self.handle, c.as_ptr()) };
        self.check(rc, &format!("命令字符串 {cmd:?}"))
    }

    /// 请求日志级别（`no`/`error`/`warn`/`info`/`v`/`debug`/`trace`）。
    pub(crate) fn request_log_messages(&self, level: &str) -> Result<()> {
        let c = CString::new(level).map_err(|_| Error::other("日志级别含 NUL 字节"))?;
        let rc = unsafe { (self.api.mpv_request_log_messages)(self.handle, c.as_ptr()) };
        self.check(rc, "mpv_request_log_messages")
    }

    /// 取下一个事件（超时秒）。返回 `None` 表示本次无事件。
    pub(crate) fn wait_event(&self, timeout_secs: f64) -> Option<MpvEventRef> {
        let ev = unsafe { (self.api.mpv_wait_event)(self.handle, timeout_secs) };
        if ev.is_null() {
            return None;
        }
        let id = unsafe { (*ev).event_id };
        if id == MPV_EVENT_NONE {
            return None;
        }
        Some(MpvEventRef { ptr: ev })
    }

    /// `mpv-version` 属性（如 "mpv 0.41.0"）。
    pub(crate) fn mpv_version(&self) -> Option<String> {
        self.get_property_string("mpv-version")
    }
}

impl Drop for RawMpv {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { (self.api.mpv_terminate_destroy)(self.handle) };
            self.handle = std::ptr::null_mut();
        }
    }
}

// ============================ 事件视图 ============================

/// 事件指针的只读视图（生命周期到下一次 `wait_event` 为止）。
pub(crate) struct MpvEventRef {
    ptr: *mut MpvEvent,
}

impl MpvEventRef {
    pub(crate) fn id(&self) -> c_int {
        unsafe { (*self.ptr).event_id }
    }

    pub(crate) fn error(&self) -> c_int {
        unsafe { (*self.ptr).error }
    }

    /// `MPV_EVENT_PROPERTY_CHANGE` 的属性名（不存在返回 `None`）。
    pub(crate) fn property_name(&self) -> Option<String> {
        let p = unsafe { (*self.ptr).data as *const MpvEventProperty };
        if p.is_null() {
            return None;
        }
        let name = unsafe { (*p).name };
        if name.is_null() {
            return None;
        }
        Some(
            unsafe { CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned(),
        )
    }

    /// `MPV_EVENT_END_FILE` 的结束原因与错误码。
    pub(crate) fn end_file(&self) -> Option<(c_int, c_int)> {
        let p = unsafe { (*self.ptr).data as *const MpvEventEndFile };
        if p.is_null() {
            return None;
        }
        Some((unsafe { (*p).reason }, unsafe { (*p).error }))
    }

    /// `MPV_EVENT_LOG_MESSAGE` 的 (级别, 前缀, 文本)。
    pub(crate) fn log_message(&self) -> Option<(String, String, String)> {
        let p = unsafe { (*self.ptr).data as *const MpvLogMessage };
        if p.is_null() {
            return None;
        }
        let s = |q: *const c_char| {
            if q.is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(q) }.to_string_lossy().into_owned()
            }
        };
        Some((
            s(unsafe { (*p).level }),
            s(unsafe { (*p).prefix }),
            s(unsafe { (*p).text }),
        ))
    }
}

/// `mpv_event_log_message`（只在本模块使用，避免泄漏结构体定义）。
#[repr(C)]
struct MpvLogMessage {
    prefix: *const c_char,
    level: *const c_char,
    text: *const c_char,
    log_level: c_int,
}

// ============================ mpv_node → JSON ============================

/// 把 mpv 节点树转成 `serde_json::Value`（字符串/布尔/整数/浮点/数组/映射）。
///
/// 调用方保证 `node` 在转换期间有效（`mpv_get_property` 返回后、
/// `mpv_free_node_contents` 之前）。
pub(crate) unsafe fn node_to_json(node: *const MpvNode) -> serde_json::Value {
    use serde_json::Value;
    if node.is_null() {
        return Value::Null;
    }
    let n = unsafe { &*node };
    match n.format {
        MPV_FORMAT_NONE => Value::Null,
        MPV_FORMAT_STRING => {
            let p = unsafe { n.u.string };
            if p.is_null() {
                Value::Null
            } else {
                Value::String(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
            }
        }
        MPV_FORMAT_FLAG => Value::Bool(unsafe { n.u.flag } != 0),
        MPV_FORMAT_INT64 => Value::Number(unsafe { n.u.int64 }.into()),
        MPV_FORMAT_DOUBLE => {
            let v = unsafe { n.u.double_ };
            serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number)
        }
        MPV_FORMAT_NODE_ARRAY | MPV_FORMAT_NODE_MAP => {
            let list = unsafe { n.u.list };
            if list.is_null() {
                return Value::Null;
            }
            let list = unsafe { &*list };
            if list.num <= 0 || list.values.is_null() {
                return if n.format == MPV_FORMAT_NODE_MAP {
                    Value::Object(serde_json::Map::new())
                } else {
                    Value::Array(Vec::new())
                };
            }
            let values = unsafe { std::slice::from_raw_parts(list.values, list.num as usize) };
            if n.format == MPV_FORMAT_NODE_ARRAY {
                Value::Array(values.iter().map(|v| unsafe { node_to_json(v) }).collect())
            } else {
                let mut map = serde_json::Map::with_capacity(list.num as usize);
                if list.keys.is_null() {
                    return Value::Object(map);
                }
                let keys = unsafe { std::slice::from_raw_parts(list.keys, list.num as usize) };
                for (k, v) in keys.iter().zip(values.iter()) {
                    if k.is_null() {
                        continue;
                    }
                    let key = unsafe { CStr::from_ptr(*k) }.to_string_lossy().into_owned();
                    map.insert(key, unsafe { node_to_json(v) });
                }
                Value::Object(map)
            }
        }
        // BYTE_ARRAY 等类型不在我们的属性集合里出现，统一返回 Null。
        _ => Value::Null,
    }
}

// ============================ 测试 ============================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cstring_rejects_interior_nul() {
        assert!(CString::new("a\0b").is_err());
    }

    #[test]
    fn node_string_to_json() {
        let s = CString::new("你好").unwrap();
        let node = MpvNode {
            u: MpvNodeUnion {
                string: s.as_ptr() as *mut c_char,
            },
            format: MPV_FORMAT_STRING,
        };
        assert_eq!(unsafe { node_to_json(&node) }, json!("你好"));
    }

    #[test]
    fn node_scalars_to_json() {
        let mut n = MpvNode {
            u: MpvNodeUnion { int64: 42 },
            format: MPV_FORMAT_INT64,
        };
        assert_eq!(unsafe { node_to_json(&n) }, json!(42));
        n.format = MPV_FORMAT_DOUBLE;
        n.u = MpvNodeUnion { double_: 1.5 };
        assert_eq!(unsafe { node_to_json(&n) }, json!(1.5));
        n.format = MPV_FORMAT_FLAG;
        n.u = MpvNodeUnion { flag: 1 };
        assert_eq!(unsafe { node_to_json(&n) }, json!(true));
        assert_eq!(unsafe { node_to_json(std::ptr::null()) }, json!(null));
    }

    #[test]
    fn node_array_to_json() {
        let mut vals = [
            MpvNode {
                u: MpvNodeUnion { int64: 1 },
                format: MPV_FORMAT_INT64,
            },
            MpvNode {
                u: MpvNodeUnion { int64: 2 },
                format: MPV_FORMAT_INT64,
            },
        ];
        let mut list = MpvNodeList {
            num: vals.len() as c_int,
            values: vals.as_mut_ptr(),
            keys: std::ptr::null_mut(),
        };
        let node = MpvNode {
            u: MpvNodeUnion { list: &mut list },
            format: MPV_FORMAT_NODE_ARRAY,
        };
        assert_eq!(unsafe { node_to_json(&node) }, json!([1, 2]));
    }

    #[test]
    fn node_map_to_json() {
        let mut vals = [
            MpvNode {
                u: MpvNodeUnion { flag: 1 },
                format: MPV_FORMAT_FLAG,
            },
            MpvNode {
                u: MpvNodeUnion { int64: 7 },
                format: MPV_FORMAT_INT64,
            },
        ];
        let k1 = CString::new("a").unwrap();
        let k2 = CString::new("b").unwrap();
        let mut keys = [k1.as_ptr() as *mut c_char, k2.as_ptr() as *mut c_char];
        let mut list = MpvNodeList {
            num: 2,
            values: vals.as_mut_ptr(),
            keys: keys.as_mut_ptr(),
        };
        let node = MpvNode {
            u: MpvNodeUnion { list: &mut list },
            format: MPV_FORMAT_NODE_MAP,
        };
        assert_eq!(unsafe { node_to_json(&node) }, json!({"a": true, "b": 7}));
    }

    #[test]
    fn empty_map_and_array() {
        let mut list = MpvNodeList {
            num: 0,
            values: std::ptr::null_mut(),
            keys: std::ptr::null_mut(),
        };
        let map = MpvNode {
            u: MpvNodeUnion { list: &mut list },
            format: MPV_FORMAT_NODE_MAP,
        };
        assert_eq!(unsafe { node_to_json(&map) }, json!({}));
        let arr = MpvNode {
            u: MpvNodeUnion { list: &mut list },
            format: MPV_FORMAT_NODE_ARRAY,
        };
        assert_eq!(unsafe { node_to_json(&arr) }, json!([]));
    }
}
