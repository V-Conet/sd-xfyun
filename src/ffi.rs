//! 讯飞 AIkit 离线合成 SDK 的 C 接口绑定。
//!
//! libaikit.so 同时导出 C++（修饰名）和 C（未修饰）两套符号，这里只用 C 接口：
//! 它与 C++ 版功能等价，但不需要写 C++ 桥接层，Rust 侧直接 `extern "C"` 即可。
//! 结构体布局对应 `include/aikit_biz_type.h` / `include/aikit_type.h`。

#![allow(non_snake_case)]

use std::os::raw::{c_char, c_int, c_void};

/// 构造器类型：参数 / 数据
pub const BUILDER_TYPE_PARAM: c_int = 0;
pub const BUILDER_TYPE_DATA: c_int = 1;

/// 构造器输入数据类型
pub const DATA_TYPE_TEXT: c_int = 0;

/// AIKIT_DataStatus：3 = 非会话单次输入输出
pub const DATA_STATUS_ONCE: c_int = 3;

/// 日志级别（aikit_constant.h）
pub const LOG_LVL_VERBOSE: c_int = 0;
pub const LOG_LVL_DEBUG: c_int = 1;
pub const LOG_LVL_INFO: c_int = 2;
pub const LOG_LVL_WARN: c_int = 3;
pub const LOG_LVL_ERROR: c_int = 4;
pub const LOG_LVL_OFF: c_int = 100;

/// 日志模式
pub const LOG_STDOUT: c_int = 0;
pub const LOG_FILE: c_int = 2;

/// 事件类型（AIKIT_EVENT）
pub const EVENT_UNKNOWN: c_int = 0;
pub const EVENT_START: c_int = 1;
pub const EVENT_END: c_int = 2;
pub const EVENT_TIMEOUT: c_int = 3;
pub const EVENT_PROGRESS: c_int = 4;

#[repr(C)]
pub struct InitParam {
    pub auth_type: c_int,
    pub app_id: *const c_char,
    pub api_key: *const c_char,
    pub api_secret: *const c_char,
    pub work_dir: *const c_char,
    pub res_dir: *const c_char,
    pub license_file: *const c_char,
    pub batch_id: *const c_char,
    pub udid: *const c_char,
    pub cfg_file: *const c_char,
}

#[repr(C)]
pub struct Handle {
    pub usr_context: *mut c_void,
    pub ability_id: *const c_char,
    pub handle_id: usize,
}

#[repr(C)]
pub struct BaseParam {
    pub next: *mut BaseParam,
    pub key: *const c_char,
    pub value: *mut c_void,
    pub reserved: *mut c_void,
    pub len: c_int,
    pub var_type: c_int,
}

#[repr(C)]
pub struct BaseData {
    pub next: *mut BaseData,
    pub desc: *mut BaseParam,
    pub key: *const c_char,
    pub value: *mut c_void,
    pub reserved: *mut c_void,
    pub len: c_int,
    pub data_type: c_int,
    pub status: c_int,
    pub from: c_int,
}

#[repr(C)]
pub struct BaseDataList {
    pub node: *mut BaseData,
    pub count: c_int,
    pub total_len: c_int,
}

#[repr(C)]
pub struct BaseParamList {
    pub node: *mut BaseParam,
    pub count: c_int,
    pub total_len: c_int,
}

pub type OutputCallback = unsafe extern "C" fn(*mut Handle, *const BaseDataList);
pub type EventCallback = unsafe extern "C" fn(*mut Handle, c_int, *const BaseParamList);
pub type ErrorCallback = unsafe extern "C" fn(*mut Handle, c_int, *const c_char);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Callbacks {
    pub output_cb: Option<OutputCallback>,
    pub event_cb: Option<EventCallback>,
    pub error_cb: Option<ErrorCallback>,
}

#[repr(C)]
pub struct BuilderData {
    pub data_type: c_int,
    pub name: *const c_char,
    pub data: *mut c_void,
    pub len: c_int,
    pub status: c_int,
}

#[repr(C)]
pub struct BuilderContext {
    pub builder_inst: *mut c_void,
    pub builder_type: c_int,
}

extern "C" {
    pub fn AIKIT_Init(param: *mut InitParam) -> c_int;
    pub fn AIKIT_UnInit() -> c_int;
    pub fn AIKIT_RegisterAbilityCallback(ability: *const c_char, cbs: Callbacks) -> c_int;
    pub fn AIKIT_Start(
        ability: *const c_char,
        param: *mut BaseParam,
        usr_context: *mut c_void,
        out_handle: *mut *mut Handle,
    ) -> c_int;
    pub fn AIKIT_Write(handle: *mut Handle, input: *mut BaseData) -> c_int;
    pub fn AIKIT_End(handle: *mut Handle) -> c_int;

    pub fn AIKIT_GetDeviceID(device_id: *mut *const c_char) -> c_int;
    pub fn AIKIT_GetVersion() -> *const c_char;
    pub fn AIKIT_GetEngineVersion(ability: *const c_char) -> *const c_char;
    pub fn AIKIT_SetAuthCheckInterval(interval_secs: u32) -> c_int;

    pub fn AIKIT_SetLogLevel(level: c_int) -> c_int;
    pub fn AIKIT_SetLogMode(mode: c_int) -> c_int;
    pub fn AIKIT_SetLogPath(path: *const c_char) -> c_int;
    pub fn AIKIT_SetILogOpen(open: bool) -> c_int;

    pub fn AIKITBuilder_Create(builder_type: c_int) -> *mut BuilderContext;
    pub fn AIKITBuilder_AddInt(handle: *mut BuilderContext, key: *const c_char, value: c_int) -> c_int;
    pub fn AIKITBuilder_AddString(
        handle: *mut BuilderContext,
        key: *const c_char,
        value: *const c_char,
        len: c_int,
    ) -> c_int;
    pub fn AIKITBuilder_AddBuf(handle: *mut BuilderContext, data: *mut BuilderData) -> c_int;
    pub fn AIKITBuilder_BuildParam(handle: *mut BuilderContext) -> *mut BaseParam;
    pub fn AIKITBuilder_BuildData(handle: *mut BuilderContext) -> *mut BaseData;
    pub fn AIKITBuilder_Clear(handle: *mut BuilderContext);
    pub fn AIKITBuilder_Destroy(handle: *mut BuilderContext);
}

/// 读取 SDK 返回的 C 字符串（空指针安全返回 `None`）。
///
/// # Safety
/// `p` 必须是 SDK 返回的、以 NUL 结尾且在该调用期间仍然有效的 C 字符串指针。
pub unsafe fn cstr(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    Some(std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned())
}
