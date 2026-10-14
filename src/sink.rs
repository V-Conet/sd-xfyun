//! PCM 出口：交给 speech-dispatcher 服务端播放（705 协议），或喂给本地播放器，
//! 或由 CLI 自己写成 WAV 文件/管道。

use std::io::{self, Seek, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::engine::AudioSink;
use crate::proto::ProtoWriter;
use crate::wav;

/// 引擎输出的 PCM 格式（官方文档 6.6.3：pcm / 16000Hz / 单声道 / 16bit）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
}

impl Default for PcmFormat {
    fn default() -> Self {
        PcmFormat {
            sample_rate: crate::engine::PCM_SAMPLE_RATE,
            channels: crate::engine::PCM_CHANNELS,
            bits: crate::engine::PCM_BITS,
        }
    }
}

/// 705 协议：把音频交给 speech-dispatcher 服务端播放。
///
/// 每块音频是一条独立消息（服务端 `output.c` 逐行读取，以 `705 AUDIO` 收尾）：
/// ```text
/// 705-bits=16
/// 705-num_channels=1
/// 705-sample_rate=16000
/// 705-num_samples=N
/// 705-big_endian=0
/// 705-AUDIO\0<经 HDLC 转义的 PCM>\n705 AUDIO\n
/// ```
/// 转义规则（与服务端实现一致）：0x7d 与 `\n` 前插入 0x7d，并把原字节第 5 位取反。
pub struct ServerAudioSink {
    writer: Arc<ProtoWriter>,
    format: PcmFormat,
}

impl ServerAudioSink {
    pub fn new(writer: Arc<ProtoWriter>, format: PcmFormat) -> Self {
        ServerAudioSink { writer, format }
    }
}

impl AudioSink for ServerAudioSink {
    fn pcm(&mut self, data: &[u8]) -> io::Result<()> {
        self.writer.audio_track(&self.format, data);
        Ok(())
    }

    fn finish(&mut self) {}

    fn abort(&mut self) {}
}

/// 播放器进程句柄：合成线程负责写入，主线程可以在 STOP/PAUSE 时直接 kill。
#[derive(Debug)]
pub struct PlayerProcess {
    child: Mutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
}

impl PlayerProcess {
    pub fn spawn(player: &PlayerCommand) -> io::Result<Arc<Self>> {
        // 调试模式下把播放器的 stderr 透出来，便于排查“播放器启动即退出”之类的问题
        let stderr = if crate::log::enabled(crate::log::DEBUG) {
            Stdio::inherit()
        } else {
            Stdio::null()
        };
        let mut child = Command::new(&player.program)
            .args(&player.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "无法获取播放器 stdin"))?;
        Ok(Arc::new(PlayerProcess {
            child: Mutex::new(Some(child)),
            stdin: Mutex::new(Some(stdin)),
        }))
    }

    fn write(&self, data: &[u8]) -> io::Result<()> {
        let mut guard = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(stdin) => stdin.write_all(data),
            None => Err(io::Error::new(io::ErrorKind::NotConnected, "播放器 stdin 已关闭")),
        }
    }

    /// 关闭 stdin（播完自然退出）
    fn close_stdin(&self) {
        let _ = self.stdin.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    /// 播放器若已退出，返回其退出状态描述（用于日志排查）。
    pub fn exit_status(&self) -> Option<String> {
        let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
        let child = guard.as_mut()?;
        match child.try_wait() {
            Ok(Some(status)) => Some(format!("{status}")),
            Ok(None) => None,
            Err(e) => Some(format!("查询失败: {e}")),
        }
    }

    /// 立即打断播放
    pub fn kill(&self) {
        self.close_stdin();
        let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn wait(&self) {
        self.close_stdin();
        let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(child) = guard.as_mut() {
            let _ = child.wait();
        }
        let _ = guard.take();
    }
}

impl Drop for PlayerProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

/// 本地播放 sink：把原始 PCM 写进播放器 stdin。
pub struct PlayerSink {
    proc: Arc<PlayerProcess>,
    writer: Arc<ProtoWriter>,
    warned: bool,
}

impl PlayerSink {
    /// `with_wav_head`：先写一个“长度未知”的 WAV 头（pw-play 需要）。
    pub fn new(
        proc: Arc<PlayerProcess>,
        writer: Arc<ProtoWriter>,
        with_wav_head: bool,
        format: PcmFormat,
    ) -> Self {
        if with_wav_head {
            let head = wav::header(format.sample_rate, format.channels, format.bits, None);
            if let Err(e) = proc.write(&head) {
                crate::log_warn!("写入 WAV 头失败：{e}");
            }
        }
        PlayerSink { proc, writer, warned: false }
    }
}

impl AudioSink for PlayerSink {
    fn pcm(&mut self, data: &[u8]) -> io::Result<()> {
        match self.proc.write(data) {
            Ok(()) => Ok(()),
            Err(e) => {
                if !self.warned {
                    match self.proc.exit_status() {
                        Some(status) => crate::log_warn!(
                            "播放器已退出（{status}），本条剩余音频将被丢弃：{e}"
                        ),
                        None => crate::log_warn!("写入播放器失败（{e}），本条剩余音频将被丢弃"),
                    }
                    self.warned = true;
                }
                Err(e)
            }
        }
    }

    fn finish(&mut self) {
        let _ = &self.writer;
        self.proc.wait();
    }

    fn abort(&mut self) {
        self.proc.kill();
    }
}

/// 播放器命令行。
///
/// `wav`：该播放器需要标准 WAV 头（实测 pw-play 从 stdin 读裸 PCM 会报
/// "Format not recognised"，但喂 WAV 流可以；paplay/aplay 则直接吃裸 PCM）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerCommand {
    pub program: String,
    pub args: Vec<String>,
    pub wav: bool,
}

impl PlayerCommand {
    pub fn new(program: &str, args: Vec<String>, wav: bool) -> Self {
        PlayerCommand { program: program.to_string(), args, wav }
    }
}

/// 各播放器「从 stdin 读原始 PCM」的参数（16000Hz / 16bit LE / 单声道）。
pub fn player_args(program: &str, format: PcmFormat) -> Vec<String> {
    let rate = format.sample_rate.to_string();
    let channels = format.channels.to_string();
    let s = |v: &str| v.to_string();
    match program {
        // pw-play 由调用方补 WAV 头（见 PlayerCommand::wav），这里只要文件名参数
        "pw-play" => vec![s("-")],
        "paplay" => vec![
            s("--raw"), s("--format=s16le"), format!("--rate={rate}"), format!("--channels={channels}"),
        ],
        "aplay" => vec![
            s("-q"), s("-t"), s("raw"), s("-f"), s("S16_LE"), s("-r"), rate, s("-c"), channels, s("-"),
        ],
        "play" => vec![
            s("-q"), s("-t"), s("raw"), s("-e"), s("signed"), s("-b"), s("16"),
            s("-r"), rate, s("-c"), channels, s("-"),
        ],
        "ffplay" => vec![
            s("-nodisp"), s("-autoexit"), s("-loglevel"), s("quiet"),
            s("-f"), s("s16le"), s("-ar"), rate, s("-ac"), channels, s("-i"), s("-"),
        ],
        _ => vec![
            s("-q"), s("-t"), s("raw"), s("-f"), s("S16_LE"), s("-r"), rate, s("-c"), channels, s("-"),
        ],
    }
}

/// 探测可用播放器。`configured` 可以是完整命令（含参数），也可以只是程序名。
pub fn resolve_player(
    configured: Option<&str>,
    audio_method: Option<&str>,
    format: PcmFormat,
) -> Option<PlayerCommand> {
    if let Some(cmd) = configured {
        let mut parts = cmd.split_whitespace();
        let program = parts.next()?.to_string();
        let args: Vec<String> = parts.map(|s| s.to_string()).collect();
        if !args.is_empty() {
            // 用户自己给了参数，按裸 PCM 处理（命令自由度优先）
            return Some(PlayerCommand::new(&program, args, false));
        }
        return Some(PlayerCommand::new(&program, player_args(&program, format), program == "pw-play"));
    }

    let candidates: &[&str] = match audio_method.map(|m| m.to_ascii_lowercase()).as_deref() {
        Some(m) if m.contains("pipewire") => &["pw-play"],
        Some(m) if m.contains("pulse") => &["paplay"],
        Some(m) if m.contains("alsa") || m.contains("oss") => &["aplay"],
        _ => &["pw-play", "paplay", "aplay"],
    };
    candidates
        .iter()
        .find(|p| which(p).is_some())
        .map(|p| PlayerCommand::new(p, player_args(p, format), *p == "pw-play"))
}

fn which(program: &str) -> Option<std::path::PathBuf> {
    if program.contains('/') {
        let p = Path::new(program);
        return p.is_file().then(|| p.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|dir| dir.join(program)).find(|p| p.is_file())
    })
}

/// 输出到管道/标准输出的 WAV：长度未知，写 0xFFFFFFFF，播放器按流处理。
pub struct StreamWavSink {
    out: Box<dyn Write + Send>,
}

impl StreamWavSink {
    pub fn new(mut out: Box<dyn Write + Send>, format: PcmFormat) -> io::Result<Self> {
        out.write_all(&wav::header(format.sample_rate, format.channels, format.bits, None))?;
        Ok(StreamWavSink { out })
    }
}

impl AudioSink for StreamWavSink {
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

/// 写入文件的 WAV：结束时回填 RIFF/data 长度，得到标准 WAV。
pub struct FileWavSink {
    file: std::fs::File,
    pcm_len: u64,
}

impl FileWavSink {
    pub fn create(path: &Path, format: PcmFormat) -> io::Result<Self> {
        let mut file = std::fs::File::create(path)?;
        file.write_all(&wav::header(format.sample_rate, format.channels, format.bits, Some(0)))?;
        Ok(FileWavSink { file, pcm_len: 0 })
    }

    pub fn pcm_len(&self) -> u64 {
        self.pcm_len
    }
}

impl AudioSink for FileWavSink {
    fn pcm(&mut self, data: &[u8]) -> io::Result<()> {
        self.file.write_all(data)?;
        self.pcm_len += data.len() as u64;
        Ok(())
    }

    fn finish(&mut self) {
        let f = &mut self.file;
        let _ = f.flush();
        let len = self.pcm_len as u32;
        let _ = f.seek(io::SeekFrom::Start(4));
        let _ = f.write_all(&(36u32.saturating_add(len)).to_le_bytes());
        let _ = f.seek(io::SeekFrom::Start(40));
        let _ = f.write_all(&len.to_le_bytes());
        let _ = f.seek(io::SeekFrom::End(0));
    }

    fn abort(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aplay_args_carry_raw_format() {
        let args = player_args("aplay", PcmFormat::default());
        assert_eq!(args, vec!["-q", "-t", "raw", "-f", "S16_LE", "-r", "16000", "-c", "1", "-"]);
    }

    #[test]
    fn paplay_args_use_equals_style() {
        let args = player_args("paplay", PcmFormat::default());
        assert_eq!(args, vec!["--raw", "--format=s16le", "--rate=16000", "--channels=1"]);
    }

    #[test]
    fn pw_play_reads_wav_from_stdin() {
        let p = resolve_player(Some("pw-play"), None, PcmFormat::default()).unwrap();
        assert_eq!(p.args, vec!["-"]);
        assert!(p.wav, "pw-play 需要 WAV 头");
        let raw = resolve_player(Some("aplay"), None, PcmFormat::default()).unwrap();
        assert!(!raw.wav);
    }

    #[test]
    fn respects_configured_player_with_args() {
        let p = resolve_player(Some("aplay -D hw:0"), None, PcmFormat::default()).unwrap();
        assert_eq!(p.program, "aplay");
        assert_eq!(p.args, vec!["-D", "hw:0"]);
        assert!(!p.wav);
    }

    #[test]
    fn configured_program_only_gets_default_args() {
        let p = resolve_player(Some("aplay"), None, PcmFormat::default()).unwrap();
        assert!(p.args.contains(&"-t".to_string()));
        assert!(p.args.contains(&"16000".to_string()));
    }

    #[test]
    fn player_follows_audio_method() {
        assert_eq!(resolve_player(None, Some("pipewire"), PcmFormat::default()).unwrap().program, "pw-play");
        assert_eq!(resolve_player(None, Some("alsa"), PcmFormat::default()).unwrap().program, "aplay");
        assert_eq!(resolve_player(None, Some("pulse"), PcmFormat::default()).unwrap().program, "paplay");
    }

    #[test]
    fn file_wav_sink_writes_valid_header() {
        let path = std::env::temp_dir().join(format!("xfyun-wav-{}.wav", std::process::id()));
        {
            let mut sink = FileWavSink::create(&path, PcmFormat::default()).unwrap();
            sink.pcm(&[0u8; 3200]).unwrap();
            sink.finish();
        }
        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len(), 44 + 3200);
        assert_eq!(u32::from_le_bytes(data[4..8].try_into().unwrap()), 36 + 3200);
        assert_eq!(u32::from_le_bytes(data[40..44].try_into().unwrap()), 3200);
        std::fs::remove_file(&path).ok();
    }
}
