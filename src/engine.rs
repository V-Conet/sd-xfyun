//! SDK 生命周期与一次朗读的编排。
//!
//! 一次朗读 = 文本切块 → 每块 `AIKIT_Start` / `AIKIT_Write`（一次送入）→ 回调
//! **流式**吐 PCM（实测每块约 960 字节 ≈ 30ms 音频）→ `AIKIT_End`。
//! 边生成边把 PCM 交给 sink（服务端播放或本地播放器），所以首字延迟低、
//! STOP 时最多只损失当前这一句。

use std::ffi::CString;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::ffi::{self, cstr};
use crate::keys::Keys;
use crate::license::License;
use crate::voices::SpeakSettings;
use crate::{log_debug, log_error, log_info, log_warn, Config};

/// 引擎输出格式（官方文档 6.6.3：pcm / 16000Hz / 单声道 / 16bit）。
pub const PCM_SAMPLE_RATE: u32 = 16_000;
pub const PCM_CHANNELS: u16 = 1;
pub const PCM_BITS: u16 = 16;

/// 单次朗读的最长等待时间，防御 SDK 卡死。
const SPEAK_TIMEOUT: Duration = Duration::from_secs(120);

pub const STOP_NONE: i32 = 0;
pub const STOP_STOP: i32 = 1;
pub const STOP_PAUSE: i32 = 2;

/// 停止请求（STOP / PAUSE 命令 → 合成线程）。
#[derive(Debug, Default)]
pub struct StopFlag {
    kind: AtomicI32,
}

impl StopFlag {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn request(&self, kind: i32) {
        self.kind.store(kind, Ordering::SeqCst);
    }

    pub fn is_set(&self) -> bool {
        self.kind.load(Ordering::SeqCst) != STOP_NONE
    }

    /// 取出并清零（朗读结束后由合成线程调用）。
    pub fn take(&self) -> i32 {
        self.kind.swap(STOP_NONE, Ordering::SeqCst)
    }

    pub fn clear(&self) {
        self.kind.store(STOP_NONE, Ordering::SeqCst);
    }
}

/// PCM 出口。实现可以是“发给 speech-dispatcher 服务端播放”或“喂给本地播放器”。
/// 事件（701/702/703）不在这里发，由上层统一发出以保证顺序。
pub trait AudioSink: Send {
    fn pcm(&mut self, data: &[u8]) -> std::io::Result<()>;
    /// 正常结束
    fn finish(&mut self);
    /// 被 STOP/PAUSE 打断
    fn abort(&mut self);
}

/// 丢弃所有音频（既没有服务端音频也没有播放器时的兜底）。
pub struct NullSink;
impl AudioSink for NullSink {
    fn pcm(&mut self, _data: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
    fn finish(&mut self) {}
    fn abort(&mut self) {}
}

#[derive(Debug)]
pub enum SpeakOutcome {
    Spoken { bytes: u64 },
    Stopped { kind: i32, bytes: u64 },
    Error { code: i32, message: String },
}

/// SDK 回调用的会话上下文，通过 `AIKIT_Start` 的 usrContext 传进去。
struct SessionCtx {
    stop: Arc<StopFlag>,
    sink: Mutex<Box<dyn AudioSink>>,
    active: AtomicBool,
    bytes: AtomicU64,
    finished: AtomicBool,
    error: Mutex<Option<(i32, String)>>,
    discard: AtomicBool,
}

impl SessionCtx {
    fn report_error(&self, code: i32, message: String) {
        let mut slot = self.error.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some((code, message));
        }
        self.finished.store(true, Ordering::SeqCst);
    }
}

unsafe extern "C" fn on_output(handle: *mut ffi::Handle, list: *const ffi::BaseDataList) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() || list.is_null() {
            return;
        }
        let ctx = (*handle).usr_context as *const SessionCtx;
        if ctx.is_null() {
            return;
        }
        let ctx = &*ctx;
        if !ctx.active.load(Ordering::SeqCst) || ctx.discard.load(Ordering::SeqCst) || ctx.stop.is_set() {
            ctx.discard.store(true, Ordering::SeqCst);
            return;
        }
        let mut node = (*list).node;
        while !node.is_null() {
            let data = &*node;
            let key = cstr(data.key).unwrap_or_default();
            if !data.value.is_null() && data.len > 0 && (key == "audio" || key.is_empty()) {
                let slice = std::slice::from_raw_parts(data.value as *const u8, data.len as usize);
                let mut sink = ctx.sink.lock().unwrap_or_else(|e| e.into_inner());
                match sink.pcm(slice) {
                    Ok(()) => {
                        ctx.bytes.fetch_add(slice.len() as u64, Ordering::Relaxed);
                    }
                    Err(e) => {
                        // 播放端断了（例如播放器退出），丢弃后续音频但别让 SDK 线程 panic
                        ctx.discard.store(true, Ordering::SeqCst);
                        log_warn!("音频输出失败，放弃本条剩余音频: {e}");
                    }
                }
            }
            node = data.next;
        }
    }));
}

unsafe extern "C" fn on_event(handle: *mut ffi::Handle, event: i32, _value: *const ffi::BaseParamList) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() {
            return;
        }
        let ctx = (*handle).usr_context as *const SessionCtx;
        if ctx.is_null() {
            return;
        }
        let ctx = &*ctx;
        match event {
            ffi::EVENT_START => log_debug!("引擎开始合成"),
            ffi::EVENT_PROGRESS => log_debug!("引擎合成进度事件"),
            ffi::EVENT_TIMEOUT => ctx.report_error(0, "引擎合成超时".to_string()),
            ffi::EVENT_END => ctx.finished.store(true, Ordering::SeqCst),
            other => log_debug!("引擎事件 {other}"),
        }
    }));
}

unsafe extern "C" fn on_error(handle: *mut ffi::Handle, code: i32, desc: *const std::os::raw::c_char) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let message = cstr(desc).unwrap_or_default();
        log_error!("引擎错误 {code} {} {}", crate::errcode::describe(code), message);
        if handle.is_null() {
            return;
        }
        let ctx = (*handle).usr_context as *const SessionCtx;
        if !ctx.is_null() {
            (*ctx).report_error(code, message);
        }
    }));
}

pub struct Engine {
    ability: CString,
    device_id: Option<String>,
    version: Option<String>,
    engine_version: Option<String>,
    key_fingerprint: String,
}

impl Engine {
    /// 初始化 SDK。必须保证 stdout 不被 SDK 日志污染（模块的 stdout 是协议通道）。
    pub fn init(cfg: &Config, keys: &Keys, license: &License, log_file: Option<&std::path::Path>) -> Result<Engine, String> {
        // SDK 日志默认走 stdout！先把它挪走，级别按配置。
        let sdk_level = match cfg.log_level {
            crate::log::TRACE => ffi::LOG_LVL_VERBOSE,
            crate::log::DEBUG => ffi::LOG_LVL_DEBUG,
            crate::log::INFO => ffi::LOG_LVL_INFO,
            crate::log::WARN => ffi::LOG_LVL_WARN,
            _ => ffi::LOG_LVL_ERROR,
        };
        // SDK 日志绝不能落到 stdout（模块的 stdout 是协议通道）。默认写进 workDir/sdk.log。
        let sdk_log_path = log_file
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| cfg.work_dir.join("sdk.log"));
        let _ = std::fs::create_dir_all(&cfg.work_dir);
        unsafe {
            ffi::AIKIT_SetLogLevel(sdk_level);
            ffi::AIKIT_SetLogMode(ffi::LOG_FILE);
            if let Ok(c) = CString::new(sdk_log_path.to_string_lossy().as_bytes()) {
                ffi::AIKIT_SetLogPath(c.as_ptr());
            }
            // 关掉本地 ilog，避免在进程 cwd 下产生 files/ 目录
            ffi::AIKIT_SetILogOpen(false);
            ffi::AIKIT_SetAuthCheckInterval(cfg.auth_check_interval);
        }

        let app_id = keys.app_id.clone();
        let ability = CString::new(crate::keys::ability()).map_err(|_| "能力 ID 非法".to_string())?;
        let work_dir = CString::new(cfg.work_dir.to_string_lossy().as_bytes())
            .map_err(|_| "workDir 非法".to_string())?;
        let res_dir = CString::new(cfg.res_dir.to_string_lossy().as_bytes())
            .map_err(|_| "resDir 非法".to_string())?;
        let license_file = match license.seed_file.as_ref() {
            Some(p) => CString::new(p.to_string_lossy().as_bytes()).map_err(|_| "licenseFile 非法".to_string())?,
            None => CString::new("").unwrap(),
        };
        let device_id_buffer = Mutex::new(None);

        let mut param = ffi::InitParam {
            auth_type: 0, // 0 = 设备级授权
            app_id: app_id.as_ptr(),
            api_key: keys.api_key.as_ptr(),
            api_secret: keys.api_secret.as_ptr(),
            work_dir: work_dir.as_ptr(),
            res_dir: res_dir.as_ptr(),
            license_file: license_file.as_ptr(),
            batch_id: std::ptr::null(),
            udid: std::ptr::null(),
            cfg_file: std::ptr::null(),
        };

        let ret = unsafe { ffi::AIKIT_Init(&mut param) };
        if ret != 0 {
            return Err(format!(
                "AIKIT_Init 失败：{}（{}）",
                crate::errcode::fmt(ret),
                engine_hint(ret, cfg)
            ));
        }

        let cbs = ffi::Callbacks {
            output_cb: Some(on_output),
            event_cb: Some(on_event),
            error_cb: Some(on_error),
        };
        let reg = unsafe { ffi::AIKIT_RegisterAbilityCallback(ability.as_ptr(), cbs) };
        if reg != 0 {
            unsafe { ffi::AIKIT_UnInit() };
            return Err(format!("AIKIT_RegisterAbilityCallback 失败：{}", crate::errcode::fmt(reg)));
        }

        let device_id = unsafe {
            let mut ptr: *const std::os::raw::c_char = std::ptr::null();
            if ffi::AIKIT_GetDeviceID(&mut ptr) == 0 {
                cstr(ptr)
            } else {
                None
            }
        };
        *device_id_buffer.lock().unwrap() = device_id.clone();

        let version = unsafe { cstr(ffi::AIKIT_GetVersion()) };
        let engine_version = unsafe { cstr(ffi::AIKIT_GetEngineVersion(ability.as_ptr())) };

        log_info!(
            "SDK 就绪：版本 {}，能力 {}，引擎 {}，设备指纹 {}",
            version.clone().unwrap_or_else(|| "?".into()),
            crate::keys::ability(),
            engine_version.clone().unwrap_or_else(|| "(SDK 未上报)".into()),
            device_id.as_deref().map(short).unwrap_or_else(|| "?".into())
        );
        log_info!("{}", license.describe());

        Ok(Engine {
            ability,
            device_id,
            version,
            engine_version,
            key_fingerprint: keys.fingerprint.clone(),
        })
    }

    pub fn device_id(&self) -> Option<&str> {
        self.device_id.as_deref()
    }

    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    pub fn engine_version(&self) -> Option<&str> {
        self.engine_version.as_deref()
    }

    pub fn key_fingerprint(&self) -> &str {
        &self.key_fingerprint
    }

    /// 合成整段文本（内部按句切块），PCM 流式写进 `sink`。
    ///
    /// 一次 utterance 复用同一个会话上下文：ctx（含 sink）在整个过程里保持存活，
    /// 因此 SDK 在回调线程里访问 sink 始终安全。
    pub fn speak_all(
        &self,
        text: &str,
        settings: &SpeakSettings,
        sink: Box<dyn AudioSink>,
        stop: Arc<StopFlag>,
        max_chunk: usize,
    ) -> SpeakOutcome {
        let ctx = Box::new(SessionCtx {
            stop: stop.clone(),
            sink: Mutex::new(sink),
            active: AtomicBool::new(true),
            bytes: AtomicU64::new(0),
            finished: AtomicBool::new(false),
            error: Mutex::new(None),
            discard: AtomicBool::new(false),
        });
        let ctx_ptr: *mut SessionCtx = Box::into_raw(ctx);

        let chunks = split_chunks(text, max_chunk);
        let mut outcome = SpeakOutcome::Spoken { bytes: 0 };
        for chunk in &chunks {
            if stop.is_set() {
                break;
            }
            let ctx = unsafe { &mut *ctx_ptr };
            ctx.discard.store(false, Ordering::SeqCst);
            let ret = unsafe { self.run_session(chunk, settings, ctx_ptr) };
            if let Some((code, message)) = ctx.error.lock().unwrap_or_else(|e| e.into_inner()).take() {
                outcome = SpeakOutcome::Error { code, message };
                break;
            }
            if ret != 0 && !ctx.error.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                outcome = SpeakOutcome::Error { code: ret, message: crate::errcode::fmt(ret) };
                break;
            }
        }

        let ctx = unsafe { &*ctx_ptr };
        let bytes = ctx.bytes.load(Ordering::Relaxed);
        // 之后不再有新的会话，标记为不活跃：迟到的回调会直接丢弃
        ctx.active.store(false, Ordering::SeqCst);
        ctx.discard.store(true, Ordering::SeqCst);

        let kind = stop.take();
        {
            let mut sink = ctx.sink.lock().unwrap_or_else(|e| e.into_inner());
            if kind != STOP_NONE {
                sink.abort();
            } else {
                sink.finish();
            }
        }

        let final_outcome = match outcome {
            SpeakOutcome::Spoken { .. } if kind != STOP_NONE => SpeakOutcome::Stopped { kind, bytes },
            SpeakOutcome::Spoken { .. } => SpeakOutcome::Spoken { bytes },
            SpeakOutcome::Stopped { kind, .. } => SpeakOutcome::Stopped { kind, bytes },
            other => other,
        };

        // 防御性延迟：SDK 未承诺 End 返回后立即停止回调，留一点缓冲再释放 ctx，
        // 避免极端的 use-after-free（代价是每次朗读多 30ms）。
        std::thread::sleep(Duration::from_millis(30));
        drop(unsafe { Box::from_raw(ctx_ptr) });
        final_outcome
    }

    unsafe fn run_session(&self, text: &str, settings: &SpeakSettings, ctx: *mut SessionCtx) -> i32 {
        (*ctx).finished.store(false, Ordering::SeqCst);
        let param_builder = ffi::AIKITBuilder_Create(ffi::BUILDER_TYPE_PARAM);
        if param_builder.is_null() {
            (*ctx).report_error(0, "创建参数构造器失败".to_string());
            return -1;
        }
        let add_str = |key: &str, value: &str| {
            let k = CString::new(key).unwrap();
            let v = CString::new(value).unwrap();
            ffi::AIKITBuilder_AddString(param_builder, k.as_ptr(), v.as_ptr(), value.len() as i32);
        };
        add_str("vcn", settings.vcn);
        add_str("vcnModel", settings.vcn);
        add_str("textEncoding", "UTF-8");
        let add_int = |key: &str, value: i32| {
            let k = CString::new(key).unwrap();
            ffi::AIKITBuilder_AddInt(param_builder, k.as_ptr(), value);
        };
        add_int("language", settings.language_id);
        add_int("speed", settings.speed as i32);
        add_int("pitch", settings.pitch as i32);
        add_int("volume", settings.volume as i32);

        let param = ffi::AIKITBuilder_BuildParam(param_builder);
        if param.is_null() {
            ffi::AIKITBuilder_Destroy(param_builder);
            (*ctx).report_error(0, "构建能力参数失败".to_string());
            return -1;
        }

        let mut handle: *mut ffi::Handle = std::ptr::null_mut();
        let start = ffi::AIKIT_Start(
            self.ability.as_ptr(),
            param,
            ctx as *mut std::os::raw::c_void,
            &mut handle,
        );
        ffi::AIKITBuilder_Destroy(param_builder);
        if start != 0 || handle.is_null() {
            (*ctx).report_error(start, format!("AIKIT_Start 失败：{}", crate::errcode::fmt(start)));
            return start;
        }

        let data_builder = ffi::AIKITBuilder_Create(ffi::BUILDER_TYPE_DATA);
        let write_ret = if data_builder.is_null() {
            -1
        } else {
            let key = CString::new("text").unwrap();
            let mut node = ffi::BuilderData {
                data_type: ffi::DATA_TYPE_TEXT,
                name: key.as_ptr(),
                data: text.as_ptr() as *mut std::os::raw::c_void,
                len: text.len() as i32,
                status: ffi::DATA_STATUS_ONCE,
            };
            ffi::AIKITBuilder_AddBuf(data_builder, &mut node);
            let input = ffi::AIKITBuilder_BuildData(data_builder);
            let ret = if input.is_null() { -1 } else { ffi::AIKIT_Write(handle, input) };
            ffi::AIKITBuilder_Destroy(data_builder);
            ret
        };

        if write_ret != 0 {
            (*ctx).report_error(write_ret, format!("AIKIT_Write 失败：{}", crate::errcode::fmt(write_ret)));
        } else {
            // 等回调把音频吐完（或被打断 / 超时）
            let deadline = Instant::now() + SPEAK_TIMEOUT;
            while !(*ctx).finished.load(Ordering::SeqCst) {
                if (*ctx).stop.is_set() {
                    (*ctx).discard.store(true, Ordering::SeqCst);
                    log_debug!("收到停止请求，放弃剩余音频");
                    break;
                }
                if Instant::now() > deadline {
                    log_error!("等待合成结果超时（{} 秒）", SPEAK_TIMEOUT.as_secs());
                    (*ctx).discard.store(true, Ordering::SeqCst);
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        (*ctx).discard.store(true, Ordering::SeqCst);
        let end = ffi::AIKIT_End(handle);
        if end != 0 {
            log_warn!("AIKIT_End 返回 {}", crate::errcode::fmt(end));
        }
        write_ret
    }

    /// 释放 SDK（进程退出/QUIT 时调用）。
    pub fn shutdown(&self) {
        unsafe {
            ffi::AIKIT_UnInit();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn short(s: &str) -> String {
    if s.len() > 16 {
        format!("{}…", &s[..16])
    } else {
        s.to_string()
    }
}

/// 常见失败原因提示。
fn engine_hint(code: i32, cfg: &Config) -> String {
    match code {
        18201 => format!(
            "引擎动态库没被加载：确认 {} 里有 *_aee.so，并让该目录出现在 LD_LIBRARY_PATH（本程序会自动 re-exec，可用 XfyunReexec 0 关闭）",
            cfg.libs_dir.display()
        ),
        18105 => format!(
            "资源缺失：{} 或 {} 下需要有 xtts/ 目录（把 SDK 包的 xtts 放过去，或设置 XfyunResDir）",
            cfg.res_dir.display(),
            cfg.work_dir.display()
        ),
        18708 => "无可用授权：装机量已满或该能力不支持当前平台/架构。开发阶段请复用已有授权文件（XfyunLicenseFile / workDir）"
            .to_string(),
        18013 => "授权文件里没有本设备：需要联网激活一次，或从托管控制台申请包含本设备的离线激活文件"
            .to_string(),
        18000 => format!("workDir {} 下没有授权文件，且 SDK 也没能联网激活", cfg.work_dir.display()),
        _ => "详见 stderr 日志（设置 XfyunLogLevel 3 打开调试日志）".to_string(),
    }
}

/// 按标点把长文本切成若干块：降低首字延迟，也让 STOP 更快生效。
pub fn split_chunks(text: &str, max_len: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let hard_limit = max_len.max(20) * 3;
    for ch in text.chars() {
        current.push(ch);
        let is_break = matches!(ch, '。' | '！' | '？' | '；' | '…' | '\n' | '.' | '!' | '?' | ';' | '、' | '，' | ',');
        if (is_break && current.chars().count() >= max_len / 2) || current.chars().count() >= hard_limit {
            chunks.push(std::mem::take(&mut current));
        }
    }
    if !current.trim().is_empty() {
        chunks.push(current);
    }
    chunks.retain(|c| c.chars().any(|ch| !ch.is_whitespace()));
    if chunks.is_empty() && !text.trim().is_empty() {
        chunks.push(text.to_string());
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_sentence_boundaries() {
        let text = "第一句。第二句！第三句？";
        let chunks = split_chunks(text, 4);
        assert_eq!(chunks.len(), 3);
        assert!(chunks[0].starts_with("第一句"));
    }

    #[test]
    fn hard_limit_forces_split_without_punctuation() {
        let text = "あ".repeat(100);
        let chunks = split_chunks(&text, 20);
        assert!(chunks.len() >= 2);
        assert!(chunks.iter().all(|c| c.chars().count() <= 60));
    }

    #[test]
    fn whitespace_only_text_produces_no_chunks() {
        assert!(split_chunks("   \n  ", 50).is_empty());
    }

    #[test]
    fn stop_flag_roundtrip() {
        let f = StopFlag::new();
        assert!(!f.is_set());
        f.request(STOP_PAUSE);
        assert!(f.is_set());
        assert_eq!(f.take(), STOP_PAUSE);
        assert_eq!(f.take(), STOP_NONE);
    }
}
