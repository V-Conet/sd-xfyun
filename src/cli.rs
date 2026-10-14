//! 命令行工具 `xfyun-say`：既能独立合成（写 WAV/RAW 或直接播放），
//! 也能作为 sd_generic 模块的合成命令使用。

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use crate::engine::{AudioSink, Engine, SpeakOutcome, StopFlag};
use crate::proto::ProtoWriter;
use crate::sink::{self, PcmFormat, PlayerSink, PlayerProcess};
use crate::voices::{self, SpeakSettings};
use crate::{keys, license, log_error, log_info, log_warn, Config};

const USAGE: &str = "\
xfyun-say —— 讯飞 AIkit 离线合成命令行

用法:
  xfyun-say [选项] [文本...]
  xfyun-say --text-file 文本.txt --out 输出.wav
  xfyun-say --list-voices

文本来源:
  --text <文本>          直接给出文本
  --text-file <文件>     从文件读（UTF-8）
  （都没有时读 stdin）

声音:
  --voice <名字>         发音人（xiaoyan/xiaofeng/catherine/john…，默认取配置）
  --language <代码>      语种（zh-CN/en-US…，用于挑选默认发音人）
  --speed/--pitch/--volume <0-100>
  --rate < -100..100 >   speech-dispatcher 风格语速，等价 speed = 50 + rate/2

路径:
  --config <模块配置>    复用 speech-dispatcher 模块的配置项
  --sdk-root/--libs-dir/--res-dir/--work-dir/--license <路径>

输出:
  --out <文件.wav>       写 WAV 文件（默认写 WAV 到 stdout）
  --raw                  写裸 PCM 到 stdout
  --play                 直接播放（也可配合 sd_generic：xfyun-say --play …）
  --quiet                不打印统计信息

其它:
  --list-voices          列出可用发音人
  --device-id            打印设备指纹（排查授权问题时用）
  --version / --help
";

#[derive(Debug, Default)]
struct Args {
    text: Option<String>,
    text_file: Option<PathBuf>,
    positional: Vec<String>,
    voice: Option<String>,
    language: Option<String>,
    speed: Option<u8>,
    pitch: Option<u8>,
    volume: Option<u8>,
    rate: Option<i32>,
    config: Option<PathBuf>,
    sdk_root: Option<PathBuf>,
    libs_dir: Option<PathBuf>,
    res_dir: Option<PathBuf>,
    work_dir: Option<PathBuf>,
    license_file: Option<PathBuf>,
    out: Option<PathBuf>,
    raw: bool,
    play: bool,
    quiet: bool,
    list_voices: bool,
    device_id: bool,
    version: bool,
    help: bool,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut args = Args::default();
    let mut i = 0;
    let take = |i: &mut usize, argv: &[String], name: &str| -> Result<String, String> {
        *i += 1;
        argv.get(*i).cloned().ok_or_else(|| format!("{name} 缺少参数"))
    };
    while i < argv.len() {
        let a = argv[i].as_str();
        match a {
            "--text" => args.text = Some(take(&mut i, argv, a)?),
            "--text-file" => args.text_file = Some(PathBuf::from(take(&mut i, argv, a)?)),
            "--voice" => args.voice = Some(take(&mut i, argv, a)?),
            "--language" => args.language = Some(take(&mut i, argv, a)?),
            "--speed" => args.speed = Some(parse_u8(&take(&mut i, argv, a)?)?),
            "--pitch" => args.pitch = Some(parse_u8(&take(&mut i, argv, a)?)?),
            "--volume" => args.volume = Some(parse_u8(&take(&mut i, argv, a)?)?),
            "--rate" => {
                let v: i32 = take(&mut i, argv, a)?.parse().map_err(|_| "--rate 需要整数".to_string())?;
                args.rate = Some(v.clamp(-100, 100));
            }
            "--config" => args.config = Some(PathBuf::from(take(&mut i, argv, a)?)),
            "--sdk-root" => args.sdk_root = Some(PathBuf::from(take(&mut i, argv, a)?)),
            "--libs-dir" => args.libs_dir = Some(PathBuf::from(take(&mut i, argv, a)?)),
            "--res-dir" => args.res_dir = Some(PathBuf::from(take(&mut i, argv, a)?)),
            "--work-dir" => args.work_dir = Some(PathBuf::from(take(&mut i, argv, a)?)),
            "--license" | "--license-file" => args.license_file = Some(PathBuf::from(take(&mut i, argv, a)?)),
            "--out" | "-o" => args.out = Some(PathBuf::from(take(&mut i, argv, a)?)),
            "--raw" => args.raw = true,
            "--play" => args.play = true,
            "--quiet" | "-q" => args.quiet = true,
            "--list-voices" => args.list_voices = true,
            "--device-id" => args.device_id = true,
            "--version" | "-V" => args.version = true,
            "--help" | "-h" => args.help = true,
            other if other.starts_with("--") => return Err(format!("未知选项 {other}")),
            other => args.positional.push(other.to_string()),
        }
        i += 1;
    }
    Ok(args)
}

fn parse_u8(v: &str) -> Result<u8, String> {
    v.trim()
        .parse::<i32>()
        .map(|n| n.clamp(0, 100) as u8)
        .map_err(|_| format!("{v} 不是 0-100 的整数"))
}

/// 命令行入口。返回退出码。
pub fn run(argv: Vec<String>) -> i32 {
    let args = match parse_args(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("参数错误：{e}\n\n{USAGE}");
            return 2;
        }
    };
    if args.help {
        print!("{USAGE}");
        return 0;
    }

    // 配置：默认值 ← 模块配置文件 ← 命令行
    let mut cfg = Config::load(args.config.as_deref());
    if let Some(p) = args.sdk_root.clone() {
        cfg.sdk_root = p;
        cfg.libs_dir = cfg.sdk_root.join("libs");
        cfg.res_dir = cfg.sdk_root.clone();
    }
    if let Some(p) = args.libs_dir.clone() {
        cfg.libs_dir = p;
    }
    if let Some(p) = args.res_dir.clone() {
        cfg.res_dir = p;
    }
    if let Some(p) = args.work_dir.clone() {
        cfg.work_dir = p;
    }
    if args.license_file.is_some() {
        cfg.license_file = args.license_file.clone();
    }
    if args.quiet && cfg.log_level > crate::log::WARN {
        cfg.log_level = crate::log::WARN;
    }
    cfg.normalize();
    crate::bootstrap_logging(&cfg, None);

    if let Err(e) = crate::loader::ensure_engine_loadable(&cfg.libs_dir, cfg.reexec, &argv) {
        log_error!("{e}");
    }

    if args.list_voices {
        for v in voices::VOICES {
            println!(
                "{}\t{}\t{}\t语言 id={}",
                v.name,
                v.language,
                v.variant.unwrap_or("none"),
                v.language_id
            );
        }
        return 0;
    }

    let keys = match keys::Keys::load() {
        Ok(k) => k,
        Err(e) => {
            log_error!("{e}");
            return 2;
        }
    };
    let app_id = keys.app_id.to_string_lossy().into_owned();
    let lic = match license::prepare(&cfg, &app_id) {
        Ok(l) => l,
        Err(e) => {
            log_error!("{e}");
            return 2;
        }
    };
    log_info!("授权状态：{}", lic.describe());

    let sdk_log = cfg.log_file.clone();
    let engine = match Engine::init(&cfg, &keys, &lic, sdk_log.as_deref()) {
        Ok(e) => e,
        Err(e) => {
            log_error!("{e}");
            return 2;
        }
    };

    if args.version || args.device_id {
        println!("{}", crate::version_line(Some(&engine)));
        if args.version && !args.device_id {
            return 0;
        }
        if args.device_id {
            println!("设备指纹: {}", engine.device_id().unwrap_or("(取不到)"));
            return 0;
        }
    }

    // 文本
    let text = match read_text(&args) {
        Ok(t) if !t.trim().is_empty() => t,
        Ok(_) => {
            log_error!("没有要合成的文本（--text / --text-file / 位置参数 / stdin）");
            return 2;
        }
        Err(e) => {
            log_error!("读取文本失败：{e}");
            return 2;
        }
    };

    let available = voices::available_voices(&[cfg.res_dir.clone(), cfg.work_dir.clone()]);
    let settings = resolve_settings(&args, &cfg, &available);

    // 出口
    let format = PcmFormat::default();
    let writer = Arc::new(ProtoWriter::new());
    let sink: Box<dyn AudioSink> = if args.play {
        match sink::resolve_player(cfg.player.as_deref(), None, format) {
            Some(cmd) => match PlayerProcess::spawn(&cmd) {
                Ok(proc) => Box::new(PlayerSink::new(proc, writer.clone(), cmd.wav, format)),
                Err(e) => {
                    log_error!("启动播放器 {} 失败：{e}", cmd.program);
                    return 2;
                }
            },
            None => {
                log_error!("没找到可用播放器（pw-play/paplay/aplay）");
                return 2;
            }
        }
    } else if args.raw {
        Box::new(RawSink::new(Box::new(io::stdout())))
    } else {
        match crate::module::wav_sink(args.out.as_deref(), format) {
            Ok(s) => s,
            Err(e) => {
                log_error!("无法创建输出：{e}");
                return 2;
            }
        }
    };

    let outcome = engine.speak_all(&text, &settings, sink, StopFlag::new(), cfg.max_chunk);
    match outcome {
        SpeakOutcome::Spoken { bytes } => {
            if !args.quiet {
                let secs = bytes as f64 / (format.sample_rate as f64 * (format.bits as f64 / 8.0));
                eprintln!(
                    "已合成 {} 字，{} 字节 PCM（{:.2} 秒），发音人 {}",
                    text.chars().count(),
                    bytes,
                    secs,
                    settings.vcn
                );
            }
            0
        }
        SpeakOutcome::Stopped { bytes, .. } => {
            log_warn!("合成被中断，已输出 {bytes} 字节");
            0
        }
        SpeakOutcome::Error { code, message } => {
            log_error!("合成失败：{code} {}", message);
            1
        }
    }
}

fn read_text(args: &Args) -> io::Result<String> {
    if let Some(t) = &args.text {
        return Ok(t.clone());
    }
    if let Some(p) = &args.text_file {
        return std::fs::read_to_string(p);
    }
    if !args.positional.is_empty() {
        return Ok(args.positional.join(" "));
    }
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}

fn resolve_settings(
    args: &Args,
    cfg: &Config,
    available: &[&'static voices::Voice],
) -> SpeakSettings {
    let fallback = available
        .iter()
        .find(|v| v.name == cfg.default_voice)
        .or_else(|| available.first())
        .copied()
        .unwrap_or(&voices::VOICES[0]);
    let wanted = args
        .voice
        .as_deref()
        .map(|v| v.to_string())
        .or_else(|| args.language.as_deref().map(|l| voices::default_for_language(l).name.to_string()));
    let voice = voices::pick(wanted.as_deref(), available, fallback);

    let from_rate = |rate: Option<i32>, fallback: u8| match rate {
        Some(r) => SpeakSettings::sd_to_percent(r, fallback),
        None => fallback,
    };

    SpeakSettings {
        vcn: voice.vcn,
        language_id: voice.language_id,
        speed: args.speed.unwrap_or_else(|| from_rate(args.rate, cfg.speed)),
        pitch: args.pitch.unwrap_or_else(|| from_rate(args.rate, cfg.pitch)),
        volume: args.volume.unwrap_or(cfg.volume),
    }
}

/// 裸 PCM 输出。
struct RawSink {
    out: Box<dyn Write + Send>,
}

impl RawSink {
    fn new(out: Box<dyn Write + Send>) -> Self {
        RawSink { out }
    }
}

impl AudioSink for RawSink {
    fn pcm(&mut self, data: &[u8]) -> io::Result<()> {
        self.out.write_all(data)
    }
    fn finish(&mut self) {
        let _ = self.out.flush();
    }
    fn abort(&mut self) {
        let _ = self.out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_basic_options() {
        let a = parse_args(&argv(&["--voice", "xiaofeng", "--rate", "60", "你好", "世界"])).unwrap();
        assert_eq!(a.voice.as_deref(), Some("xiaofeng"));
        assert_eq!(a.rate, Some(60));
        assert_eq!(a.positional, vec!["你好", "世界"]);
    }

    #[test]
    fn rejects_unknown_option() {
        assert!(parse_args(&argv(&["--nope"])).is_err());
    }

    #[test]
    fn rate_is_clamped() {
        let a = parse_args(&argv(&["--rate", "999"])).unwrap();
        assert_eq!(a.rate, Some(100));
    }

    #[test]
    fn clamps_out_of_range_levels() {
        let a = parse_args(&argv(&["--speed", "500"])).unwrap();
        assert_eq!(a.speed, Some(100));
    }

    #[test]
    fn settings_follow_voice_and_rate() {
        let cfg = Config::default();
        let a = parse_args(&argv(&["--voice", "catherine", "--rate", "100"])).unwrap();
        let all: Vec<&'static voices::Voice> = voices::VOICES.iter().collect();
        let s = resolve_settings(&a, &cfg, &all);
        assert_eq!(s.vcn, "catherine");
        assert_eq!(s.language_id, 2);
        assert_eq!(s.speed, 100);
    }

    #[test]
    fn settings_fall_back_when_voice_not_installed() {
        let cfg = Config::default();
        let a = parse_args(&argv(&["--voice", "catherine"])).unwrap();
        let only_zh: Vec<&'static voices::Voice> =
            vec![&voices::VOICES[0], &voices::VOICES[1]];
        let s = resolve_settings(&a, &cfg, &only_zh);
        assert_eq!(s.vcn, "xiaoyan");
    }

    #[test]
    fn missing_option_value_is_error() {
        assert!(parse_args(&argv(&["--voice"])).is_err());
    }
}
