//! speech-dispatcher 输出模块：INIT 握手、命令分发、朗读编排。
//!
//! 线程模型：
//! - **主线程**：只做协议读写（读命令、回回复），保证 STOP/PAUSE 随时可被处理；
//! - **合成线程**：从队列取朗读任务，逐句调用 SDK，把 PCM 交给 sink，
//!   并负责发出 `701 BEGIN` / `702 END` / `703 STOP` / `704 PAUSE` 事件。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use crate::engine::{AudioSink, Engine, NullSink, SpeakOutcome, StopFlag, STOP_PAUSE, STOP_STOP};
use crate::license::{self, LicenseMode};
use crate::proto::{ProtoReader, ProtoWriter};
use crate::sink::{self, FileWavSink, PcmFormat, PlayerProcess, ServerAudioSink, StreamWavSink};
use crate::voices::{self, SpeakSettings};
use crate::{keys, log_debug, log_error, log_info, log_warn, Config};

/// 一次性朗读任务。
struct Job {
    text: String,
    settings: SpeakSettings,
    stop: Arc<StopFlag>,
}

/// speech-dispatcher 通过 SET 下发、并影响发声的参数。
#[derive(Debug, Default, Clone)]
struct SdSettings {
    voice_type: Option<String>,
    synthesis_voice: Option<String>,
    language: Option<String>,
    rate: i32,
    pitch: i32,
    volume: i32,
}

impl SdSettings {
    fn apply(&mut self, key: &str, value: &str) -> bool {
        match key {
            "voice" => self.voice_type = non_null(value),
            "synthesis_voice" => self.synthesis_voice = non_null(value),
            "language" => self.language = non_null(value),
            "rate" => self.rate = value.trim().parse().unwrap_or(0),
            "pitch" => self.pitch = value.trim().parse().unwrap_or(0),
            "volume" => self.volume = value.trim().parse().unwrap_or(0),
            // 本后端不实现但必须接受，否则整批设置会被判为非法
            "pitch_range" | "punctuation_mode" | "spelling_mode" | "cap_let_recogn" => {}
            _ => return false,
        }
        true
    }

    /// 结合模块默认值算出真正要下发给引擎的参数。
    /// 请求的发音人若不在本机资源里，会退回到默认发音人（否则引擎会返回 18501）。
    fn resolve(&self, cfg: &Config, available: &[&'static voices::Voice]) -> SpeakSettings {
        let fallback = available
            .iter()
            .find(|v| v.name == cfg.default_voice)
            .or_else(|| available.first())
            .copied()
            .unwrap_or(&voices::VOICES[0]);

        let wanted = self
            .synthesis_voice
            .as_deref()
            .or(match (self.voice_type.as_deref(), self.language.as_deref()) {
                (Some(vt), Some(lang)) => Some(voices::for_voice_type(vt, lang).name),
                (Some(vt), None) => Some(voices::for_voice_type(vt, &cfg.default_language).name),
                (None, Some(lang)) => Some(voices::default_for_language(lang).name),
                (None, None) => None,
            });
        let voice = voices::pick(wanted, available, fallback);

        SpeakSettings {
            vcn: voice.vcn,
            language_id: voice.language_id,
            speed: SpeakSettings::sd_to_percent(self.rate, cfg.speed),
            pitch: SpeakSettings::sd_to_percent(self.pitch, cfg.pitch),
            volume: SpeakSettings::sd_to_percent(self.volume, cfg.volume),
        }
    }
}

fn non_null(value: &str) -> Option<String> {
    let v = value.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("NULL") {
        None
    } else {
        Some(v.to_string())
    }
}

/// 音频出口模式，由 `AUDIO` 命令协商决定。
#[derive(Debug, Clone)]
enum AudioMode {
    /// 交给 speech-dispatcher 服务端播放（默认，服务端会先提出这个方案）
    Server,
    /// 模块自己拉起播放器
    Player(Option<sink::PlayerCommand>),
    /// 既不能服务端播放也没有播放器
    None,
}

struct Module {
    cfg: Config,
    /// 本机资源里真实可用的发音人（以 xtts/ 资源文件为准）
    available: Vec<&'static voices::Voice>,
    writer: Arc<ProtoWriter>,
    settings: Mutex<SdSettings>,
    audio: Mutex<AudioMode>,
    audio_method: Mutex<Option<String>>,
    stop: Arc<StopFlag>,
    speaking: Arc<AtomicBool>,
    tx: Mutex<Option<Sender<Job>>>,
    current_player: Arc<Mutex<Option<Arc<PlayerProcess>>>>,
}

/// 模块入口，返回进程退出码。
pub fn run(config_path: Option<PathBuf>) -> i32 {
    let cfg = Config::load(config_path.as_deref());
    crate::bootstrap_logging(&cfg, None);

    // 让引擎动态库能被 SDK dlopen 到（必要时 re-exec；exec 成功后本函数不会返回）
    let argv: Vec<String> = std::env::args().collect();
    if let Err(e) = crate::loader::ensure_engine_loadable(&cfg.libs_dir, cfg.reexec, &argv[1..]) {
        log_error!("{e}");
    }
    log_debug!("模块启动，配置：{cfg:?}");

    let writer = Arc::new(ProtoWriter::new());
    let mut reader = ProtoReader::new();

    // 1) INIT 握手
    match reader.read_line() {
        Ok(Some(line)) if line == "INIT" => {}
        Ok(Some(line)) => {
            log_error!("服务端首条命令应为 INIT，实际是 {line:?}");
            return 3;
        }
        Ok(None) => {
            log_error!("stdin 已关闭，未收到 INIT");
            return 3;
        }
        Err(e) => {
            log_error!("读取 INIT 失败：{e}");
            return 3;
        }
    }

    // 2) 密钥 + 授权文件
    let keys = match keys::Keys::load() {
        Ok(k) => k,
        Err(e) => return fail_init(&writer, &e),
    };
    let app_id = keys.app_id.to_string_lossy().into_owned();
    let lic = match license::prepare(&cfg, &app_id) {
        Ok(l) => l,
        Err(e) => return fail_init(&writer, &e),
    };
    // 3) SDK 初始化
    let sdk_log = cfg.log_file.clone();
    let engine = match Engine::init(&cfg, &keys, &lic, sdk_log.as_deref()) {
        Ok(e) => e,
        Err(e) => return fail_init(&writer, &e),
    };

    // 4) 上报就绪
    let ready = format!(
        "sd-xfyun {}，引擎 {}，授权：{}",
        engine.version().unwrap_or("?"),
        engine.engine_version().unwrap_or("SDK 未上报"),
        match lic.mode {
            LicenseMode::Cached => "复用 workDir 缓存（未联网）",
            LicenseMode::Seeded => "已从离线授权文件导入",
            LicenseMode::OnlineActivation => "已联网首次激活",
        }
    );
    writer.multiline(&[format!("299-{ready}"), "299 OK LOADED SUCCESSFULLY".to_string()]);
    log_info!("模块就绪：{ready}");

    // 5) 合成线程
    let (tx, rx) = mpsc::channel::<Job>();
    let available = voices::available_voices(&[cfg.res_dir.clone(), cfg.work_dir.clone()]);
    log_info!(
        "可用发音人（{}）：{}",
        available.len(),
        available.iter().map(|v| v.name).collect::<Vec<_>>().join(" / ")
    );

    let module = Arc::new(Module {
        cfg,
        available,
        writer,
        settings: Mutex::new(SdSettings::default()),
        audio: Mutex::new(AudioMode::Server),
        audio_method: Mutex::new(None),
        stop: StopFlag::new(),
        speaking: Arc::new(AtomicBool::new(false)),
        tx: Mutex::new(Some(tx)),
        current_player: Arc::new(Mutex::new(None)),
    });
    let worker = {
        let module = module.clone();
        std::thread::Builder::new()
            .name("xfyun-synth".to_string())
            .spawn(move || synth_worker(module, engine, rx))
            .expect("无法创建合成线程")
    };

    // 6) 命令循环
    let code = command_loop(&module, &mut reader);

    // 7) 收尾：停朗读、关队列、等合成线程退出（Engine 的 Drop 里会 AIKIT_UnInit）
    module.speaking.store(false, Ordering::SeqCst);
    module.stop.request(STOP_STOP);
    if let Some(player) = module.current_player.lock().unwrap_or_else(|e| e.into_inner()).take() {
        player.kill();
    }
    drop(module.tx.lock().unwrap_or_else(|e| e.into_inner()).take());
    let _ = worker.join();
    code
}

fn fail_init(writer: &Arc<ProtoWriter>, message: &str) -> i32 {
    let one_line = message.replace(['\n', '\r'], " ");
    log_error!("模块初始化失败：{one_line}");
    writer.multiline(&[format!("399-{one_line}"), "399 ERR CANT INIT MODULE".to_string()]);
    1
}

fn command_loop(module: &Arc<Module>, reader: &mut ProtoReader) -> i32 {
    loop {
        let line = match reader.read_line() {
            Ok(Some(l)) => l,
            Ok(None) => {
                log_debug!("stdin 关闭，模块退出");
                return 0;
            }
            Err(e) => {
                log_error!("读取命令失败：{e}");
                return 1;
            }
        };

        match line.as_str() {
            "SPEAK" | "SOUND_ICON" | "CHAR" | "KEY" => handle_speak(module, reader, &line),
            "STOP" => request_stop(module, STOP_STOP),
            "PAUSE" => request_stop(module, STOP_PAUSE),
            "SET" => handle_set(module, reader),
            "AUDIO" => handle_audio(module, reader),
            "LOGLEVEL" => handle_loglevel(module, reader),
            "QUIT" => {
                module.writer.event("210 OK QUIT");
                return 0;
            }
            "" => {}
            other if other.starts_with("LIST VOICES") => handle_list_voices(module, other),
            other if other.starts_with("DEBUG") => handle_debug(module, other),
            other => {
                log_debug!("未知命令：{other:?}");
                module.writer.event("300 ERR UNKNOWN COMMAND");
            }
        }
    }
}

/// 停止请求：置标志并立刻打断正在播放的音频。
fn request_stop(module: &Arc<Module>, kind: i32) {
    if !module.speaking.load(Ordering::SeqCst) {
        log_debug!("当前没有在朗读，忽略停止请求");
        return;
    }
    log_debug!("收到 {} 请求", if kind == STOP_PAUSE { "PAUSE" } else { "STOP" });
    module.stop.request(kind);
    if let Some(player) = module.current_player.lock().unwrap_or_else(|e| e.into_inner()).take() {
        player.kill();
    }
}

fn handle_speak(module: &Arc<Module>, reader: &mut ProtoReader, kind: &str) {
    module.writer.event("202 OK RECEIVING MESSAGE");
    let mut text = match reader.read_message() {
        Ok(Some(t)) => t,
        Ok(None) => {
            log_warn!("读取说话数据时对端关闭");
            return;
        }
        Err(e) => {
            log_error!("读取说话数据失败：{e}");
            return;
        }
    };
    if (kind == "CHAR" || kind == "KEY") && text == "space" {
        text = " ".to_string();
    }
    let text = strip_ssml_if_needed(&text);
    if text.trim().is_empty() {
        module.writer.event("301 ERROR CANT SPEAK");
        return;
    }

    let settings = module
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .resolve(&module.cfg, &module.available);
    module.speaking.store(true, Ordering::SeqCst);
    module.stop.clear();
    module.writer.event("200 OK SPEAKING");
    log_info!(
        "开始朗读 {} 字（发音人 {}，语速 {}，音调 {}，音量 {}）",
        text.chars().count(),
        settings.vcn,
        settings.speed,
        settings.pitch,
        settings.volume
    );

    let sent = {
        let guard = module.tx.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(tx) => tx.send(Job { text, settings, stop: module.stop.clone() }).is_ok(),
            None => false,
        }
    };
    if !sent {
        log_error!("合成线程已退出，无法朗读");
        module.speaking.store(false, Ordering::SeqCst);
        module.writer.event("702 END");
    }
}

fn handle_list_voices(module: &Arc<Module>, line: &str) {
    let mut parts = line.split_whitespace();
    let _ = parts.next(); // LIST
    let _ = parts.next(); // VOICES
    let want_lang = parts.next();
    let want_variant = parts.next();

    let matches = |voice: &voices::Voice| -> bool {
        let Some(lang) = want_lang else { return true };
        let ok = voice.language.eq_ignore_ascii_case(lang)
            || voice
                .language
                .split('-')
                .next()
                .map(|l| l.eq_ignore_ascii_case(lang))
                .unwrap_or(false);
        if !ok {
            return false;
        }
        match want_variant {
            Some(variant) => voice.variant == Some(variant),
            None => true,
        }
    };

    let mut lines: Vec<String> = module
        .available
        .iter()
        .filter(|v| matches(v))
        .map(|v| format!("200-{}\t{}\t{}", v.name, v.language, v.variant.unwrap_or("none")))
        .collect();
    if lines.is_empty() {
        module.writer.event("304 CANT LIST VOICES");
    } else {
        log_debug!("上报 {} 个发音人", lines.len());
        lines.push("200 OK VOICE LIST SENT".to_string());
        module.writer.multiline(&lines);
    }
}

fn handle_set(module: &Arc<Module>, reader: &mut ProtoReader) {
    module.writer.event("203 OK RECEIVING SETTINGS");
    let items = match reader.read_settings() {
        Ok(i) => i,
        Err(e) => {
            log_error!("读取设置失败：{e}");
            module.writer.event("302 ERROR BAD SYNTAX");
            return;
        }
    };
    let mut error = None;
    {
        let mut settings = module.settings.lock().unwrap_or_else(|e| e.into_inner());
        for (key, value) in &items {
            if !settings.apply(key, value) {
                log_debug!("未支持的设置项 {key}={value}");
                error = Some("303 ERROR INVALID PARAMETER OR VALUE");
            }
        }
    }
    log_debug!("设置已更新：{items:?}");
    module.writer.event(error.unwrap_or("203 OK SETTINGS RECEIVED"));
}

fn handle_audio(module: &Arc<Module>, reader: &mut ProtoReader) {
    module.writer.event("207 OK RECEIVING AUDIO SETTINGS");
    let items = match reader.read_settings() {
        Ok(i) => i,
        Err(e) => {
            log_error!("读取音频设置失败：{e}");
            module.writer.event("302 ERROR BAD SYNTAX");
            return;
        }
    };

    let method = items
        .iter()
        .find(|(k, _)| k == "audio_output_method")
        .map(|(_, v)| v.clone());
    let wants_server = method.as_deref() == Some("server");

    if wants_server {
        if module.cfg.server_audio {
            *module.audio.lock().unwrap_or_else(|e| e.into_inner()) = AudioMode::Server;
            log_info!("音频出口：由 speech-dispatcher 服务端播放（705 协议）");
            module.writer.event("203 OK AUDIO INITIALIZED");
        } else {
            // 拒绝后服务端会用 audio_* 设置再协商一次，那时改由模块自己播放
            log_info!("配置要求模块自播放，已拒绝服务端音频");
            module.writer.event("303 ERROR INVALID PARAMETER OR VALUE");
        }
        return;
    }

    if let Some(m) = &method {
        *module.audio_method.lock().unwrap_or_else(|e| e.into_inner()) = Some(m.clone());
    }
    let method_now = module.audio_method.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let player = sink::resolve_player(
        module.cfg.player.as_deref(),
        method_now.as_deref(),
        PcmFormat::default(),
    );
    match &player {
        Some(p) => log_info!("音频出口：本地播放器 {} {}", p.program, p.args.join(" ")),
        None => log_warn!(
            "没有可用播放器（pw-play/paplay/aplay 都没找到），将没有声音，可用 XfyunPlayer 指定"
        ),
    }
    *module.audio.lock().unwrap_or_else(|e| e.into_inner()) =
        if player.is_some() { AudioMode::Player(player) } else { AudioMode::None };
    module.writer.event("203 OK AUDIO INITIALIZED");
}

fn handle_loglevel(module: &Arc<Module>, reader: &mut ProtoReader) {
    module.writer.event("207 OK RECEIVING LOGLEVEL SETTINGS");
    let items = match reader.read_settings() {
        Ok(i) => i,
        Err(e) => {
            log_error!("读取日志级别失败：{e}");
            module.writer.event("302 ERROR BAD SYNTAX");
            return;
        }
    };
    if let Some((_, value)) = items.iter().find(|(k, _)| k == "log_level") {
        if let Ok(level) = value.trim().parse::<i32>() {
            // speech-dispatcher 用 0..5；0 表示未设置，此时保留配置里的级别
            let mapped = match level {
                1 => Some(crate::log::ERROR),
                2 => Some(crate::log::WARN),
                3 => Some(crate::log::INFO),
                4 => Some(crate::log::DEBUG),
                5 => Some(crate::log::TRACE),
                _ => None,
            };
            if let Some(mapped) = mapped {
                crate::log::set_level(mapped);
            }
        }
    }
    module.writer.event("203 OK LOGLEVEL SET");
}

fn handle_debug(module: &Arc<Module>, line: &str) {
    let mut parts = line.split_whitespace();
    let _ = parts.next(); // DEBUG
    match parts.next() {
        Some("ON") => {
            let file = parts.next().map(PathBuf::from);
            crate::log::set_level(crate::log::DEBUG);
            crate::log::set_file(file.as_deref());
            module.writer.event("200 OK DEBUGGING ON");
        }
        Some("OFF") => {
            crate::log::set_file(None);
            module.writer.event("200 OK DEBUGGING OFF");
        }
        _ => module.writer.event("302 ERROR BAD SYNTAX"),
    }
}

/// 合成线程：串行处理朗读任务。
fn synth_worker(module: Arc<Module>, engine: Engine, rx: Receiver<Job>) {
    while let Ok(job) = rx.recv() {
        let outcome = speak_job(&module, &engine, &job);
        module.speaking.store(false, Ordering::SeqCst);
        match &outcome {
            SpeakOutcome::Stopped { kind, bytes } if *kind == STOP_PAUSE => {
                log_debug!("朗读被暂停，已输出 {bytes} 字节");
                module.writer.event("704 PAUSE");
            }
            SpeakOutcome::Stopped { bytes, .. } => {
                log_debug!("朗读被停止，已输出 {bytes} 字节");
                module.writer.event("703 STOP");
            }
            SpeakOutcome::Spoken { bytes } => {
                log_debug!("朗读完成，共 {bytes} 字节 PCM");
                module.writer.event("702 END");
            }
            SpeakOutcome::Error { code, message } => {
                log_error!("朗读失败：{code} {message}，向服务端上报结束");
                module.writer.event("702 END");
            }
        }
    }
    log_debug!("合成线程退出");
}

fn speak_job(module: &Arc<Module>, engine: &Engine, job: &Job) -> SpeakOutcome {
    let format = PcmFormat::default();
    let mode = module.audio.lock().unwrap_or_else(|e| e.into_inner()).clone();

    let sink: Box<dyn AudioSink> = match mode {
        AudioMode::Server => Box::new(ServerAudioSink::new(module.writer.clone(), format)),
        AudioMode::Player(Some(cmd)) => match PlayerProcess::spawn(&cmd) {
            Ok(proc) => {
                *module.current_player.lock().unwrap_or_else(|e| e.into_inner()) = Some(proc.clone());
                Box::new(sink::PlayerSink::new(proc, module.writer.clone(), cmd.wav, format))
            }
            Err(e) => {
                log_warn!("启动播放器 {} 失败：{e}，本条没有声音", cmd.program);
                Box::new(NullSink)
            }
        },
        AudioMode::Player(None) | AudioMode::None => Box::new(NullSink),
    };

    module.writer.event("701 BEGIN");
    let outcome = engine.speak_all(
        &job.text,
        &job.settings,
        sink,
        job.stop.clone(),
        module.cfg.max_chunk,
    );
    *module.current_player.lock().unwrap_or_else(|e| e.into_inner()) = None;
    outcome
}

/// 去掉 SSML 标签（客户端开了 SSML 模式时服务端会把原文直接转给模块）。
/// 只在文本确实像 SSML 时才处理，避免误吃正文里的 `<`。
pub fn strip_ssml_if_needed(text: &str) -> String {
    let trimmed = text.trim_start();
    if trimmed.starts_with("<speak") || trimmed.starts_with("<?xml") {
        strip_ssml(text)
    } else {
        text.to_string()
    }
}

fn strip_ssml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut omit = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => omit = true,
            '>' => omit = false,
            '&' => {
                let rest: String = chars.clone().take(6).collect();
                let (entity, len) = if rest.starts_with("lt;") {
                    ("<", 3)
                } else if rest.starts_with("gt;") {
                    (">", 3)
                } else if rest.starts_with("amp;") {
                    ("&", 4)
                } else if rest.starts_with("quot;") {
                    ("\"", 5)
                } else if rest.starts_with("apos;") {
                    ("'", 5)
                } else {
                    ("", 0)
                };
                if len > 0 {
                    out.push_str(entity);
                    for _ in 0..len {
                        chars.next();
                    }
                } else {
                    out.push('&');
                }
            }
            other if !omit => out.push(other),
            _ => {}
        }
    }
    out
}

/// WAV 出口（CLI 用）：给了路径就写文件（结束回填长度），否则写 stdout（长度未知）。
pub fn wav_sink(
    path: Option<&std::path::Path>,
    format: PcmFormat,
) -> std::io::Result<Box<dyn AudioSink>> {
    match path {
        Some(p) => Ok(Box::new(FileWavSink::create(p, format)?)),
        None => Ok(Box::new(StreamWavSink::new(Box::new(std::io::stdout()), format)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_table() -> Vec<&'static voices::Voice> {
        voices::VOICES.iter().collect()
    }

    #[test]
    fn settings_default_to_config_voice() {
        let cfg = Config::default();
        let resolved = SdSettings::default().resolve(&cfg, &full_table());
        assert_eq!(resolved.vcn, "xiaoyan");
        assert_eq!(resolved.language_id, 1);
        assert_eq!(resolved.speed, 50);
    }

    #[test]
    fn settings_honour_synthesis_voice_and_rate() {
        let cfg = Config::default();
        let mut s = SdSettings::default();
        assert!(s.apply("synthesis_voice", "john"));
        assert!(s.apply("rate", "80"));
        let resolved = s.resolve(&cfg, &full_table());
        assert_eq!(resolved.vcn, "john");
        assert_eq!(resolved.language_id, 2);
        assert_eq!(resolved.speed, 90);
    }

    #[test]
    fn settings_fall_back_to_language_default() {
        let cfg = Config::default();
        let mut s = SdSettings::default();
        assert!(s.apply("voice", "male1"));
        assert!(s.apply("language", "en-US"));
        assert_eq!(s.resolve(&cfg, &full_table()).vcn, "john");
    }

    #[test]
    fn known_settings_accepted_unknown_rejected() {
        let mut s = SdSettings::default();
        for key in [
            "voice", "synthesis_voice", "language", "rate", "pitch", "volume",
            "pitch_range", "punctuation_mode", "spelling_mode", "cap_let_recogn",
        ] {
            assert!(s.apply(key, "1"), "{key} 应被接受");
        }
        assert!(!s.apply("nonsense", "1"));
    }

    #[test]
    fn null_values_are_unset() {
        let mut s = SdSettings::default();
        assert!(s.apply("language", "NULL"));
        assert!(s.apply("synthesis_voice", "NULL"));
        assert!(s.language.is_none());
        assert!(s.synthesis_voice.is_none());
    }

    #[test]
    fn unavailable_voice_falls_back_to_default() {
        let cfg = Config::default();
        let mut s = SdSettings::default();
        assert!(s.apply("synthesis_voice", "john"));
        // 只装了 xiaoyan 时，john 应退回 xiaoyan
        let only_xiaoyan: Vec<&'static voices::Voice> = vec![&voices::VOICES[0]];
        let resolved = s.resolve(&cfg, &only_xiaoyan);
        assert_eq!(resolved.vcn, "xiaoyan");
        assert_eq!(resolved.language_id, 1);
    }

    #[test]
    fn strips_ssml_only_when_it_looks_like_ssml() {
        assert_eq!(strip_ssml_if_needed("普通文本 1 < 2"), "普通文本 1 < 2");
        assert_eq!(
            strip_ssml_if_needed("<speak>你好<break time=\"1s\"/>世界 &amp; 朋友</speak>"),
            "你好世界 & 朋友"
        );
    }
}
