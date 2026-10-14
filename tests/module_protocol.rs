//! 端到端（不依赖 speech-dispatcher 守护进程）：
//! 直接按模块协议驱动 `sd_xfyun`，检查握手、音频输出、事件序列与退出。
//!
//! 需要讯飞 SDK 包与可用的授权文件；缺 SDK 时自动跳过（打印提示）。

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

fn sdk_root() -> Option<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Ok(dir) = std::env::var("XFYUN_SDK_ROOT") {
        let p = PathBuf::from(dir);
        if is_sdk(&p) {
            return p.canonicalize().ok();
        }
    }
    let third_party = manifest.join("3rd_party");
    if is_sdk(&third_party) {
        return third_party.canonicalize().ok();
    }
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&third_party)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_sdk(p))
        .collect();
    candidates.sort();
    candidates.into_iter().next()?.canonicalize().ok()
}

/// SDK 包根目录的标志
fn is_sdk(dir: &Path) -> bool {
    dir.join("libs/libaikit.so").is_file()
}

struct Module {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<Vec<u8>>,
}

impl Module {
    fn start(sdk: &Path, work_dir: &Path, conf: &Path) -> Module {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sd_xfyun"))
            .arg(conf)
            .env("XFYUN_SDK_ROOT", sdk)
            .env("XFYUN_WORK_DIR", work_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("无法启动 sd_xfyun");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // 必须按字节读：705 的音频载荷是二进制，lines() 遇到非法 UTF-8 会直接结束
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
                            buf.pop();
                        }
                        if tx.send(buf.clone()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Module { child, stdin, lines: rx }
    }

    fn send(&mut self, line: &str) {
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.write_all(b"\n").unwrap();
        self.stdin.flush().unwrap();
    }

    /// 读一行（原始字节），超时返回 None。
    fn recv(&mut self, timeout: Duration) -> Option<Vec<u8>> {
        self.lines.recv_timeout(timeout).ok()
    }

    /// 读到指定前缀的行为止，返回该行的文本形式。
    fn wait_for(&mut self, prefix: &str, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match self.recv(remaining) {
                Some(line) if starts_with(&line, prefix) => {
                    return Some(String::from_utf8_lossy(&line).into_owned())
                }
                Some(_) => continue,
                None => return None,
            }
        }
        None
    }
}

fn write_conf(path: &Path, sdk: &Path, work: &Path) {
    let conf = format!(
        "Debug 0\nXfyunSdkRoot \"{}\"\nXfyunWorkDir \"{}\"\nXfyunLogLevel 2\nXfyunServerAudio 1\n",
        sdk.display(),
        work.display()
    );
    std::fs::write(path, conf).unwrap();
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sd-xfyun-test-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn module_reports_voices_and_synthesizes_audio() {
    let Some(sdk) = sdk_root() else {
        eprintln!("跳过：在 3rd_party/ 或 XFYUN_SDK_ROOT 下找不到 SDK（需要 libs/libaikit.so）");
        return;
    };
    let has_license = std::fs::read_dir(&sdk)
        .map(|it| {
            it.flatten().any(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.len() == 40 && n.starts_with(|c: char| c.is_ascii_hexdigit())
            })
        })
        .unwrap_or(false);
    if !has_license {
        eprintln!("跳过：SDK 包里没有授权文件，模块无法初始化");
        return;
    }

    let dir = temp_dir("proto");
    let conf = dir.join("xfyun-tts.conf");
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    write_conf(&conf, &sdk, &work);

    let mut m = Module::start(&sdk, &work, &conf);
    m.send("INIT");
    let hello = m.wait_for("299", Duration::from_secs(60)).expect("未收到 299 初始化回复");
    assert!(hello.starts_with("299-"), "初始化回复格式不对：{hello}");
    assert!(m.wait_for("299 OK", Duration::from_secs(5)).is_some(), "缺少 299 OK LOADED SUCCESSFULLY");

    // 服务端音频协商：先收到 207 应答，接受 server 后回 203
    m.send("AUDIO");
    m.wait_for("207", Duration::from_secs(5)).expect("缺少 AUDIO 应答");
    m.send("audio_output_method=server");
    m.send(".");
    m.wait_for("203 OK AUDIO INITIALIZED", Duration::from_secs(5)).expect("未接受服务端音频");

    // 语音列表
    m.send("LIST VOICES");
    let mut names = Vec::new();
    while let Some(line) = m.recv(Duration::from_secs(5)) {
        let text = String::from_utf8_lossy(&line).into_owned();
        if let Some(rest) = text.strip_prefix("200-") {
            names.push(rest.split('\t').next().unwrap_or("").to_string());
        } else if text.starts_with("200 OK") {
            break;
        }
    }
    assert!(!names.is_empty(), "语音列表为空");
    assert!(names.iter().any(|n| n == "xiaoyan"), "列表里应有 xiaoyan：{names:?}");

    // 设置 + 朗读
    m.send("SET");
    m.wait_for("203", Duration::from_secs(5)).expect("缺少 SET 应答");
    m.send("rate=0");
    m.send("pitch=0");
    m.send("volume=0");
    m.send("synthesis_voice=NULL");
    m.send("language=NULL");
    m.send(".");
    m.wait_for("203 OK SETTINGS RECEIVED", Duration::from_secs(5)).expect("设置未被接受");

    m.send("SPEAK");
    m.wait_for("202", Duration::from_secs(5)).expect("缺少 202 应答");
    m.send("测试一句话。");
    m.send(".");
    m.wait_for("200 OK SPEAKING", Duration::from_secs(10)).expect("未开始朗读");
    assert!(m.wait_for("701 BEGIN", Duration::from_secs(10)).is_some(), "缺少 701 BEGIN");

    // 收集 705 音频块，直到 702 END
    let mut pcm_bytes = 0usize;
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut finished = false;
    while Instant::now() < deadline {
        let Some(line) = m.recv(Duration::from_secs(30)) else { break };
        if starts_with(&line, "705-AUDIO") {
            // 705-AUDIO\0<HDLC 转义后的 PCM>
            pcm_bytes += unescape_hdlc(&line["705-AUDIO".len() + 1..]).len();
        } else if starts_with(&line, "702 END") {
            finished = true;
            break;
        } else if starts_with(&line, "301") || starts_with(&line, "300") {
            panic!("模块报错：{}", String::from_utf8_lossy(&line));
        }
    }
    assert!(finished, "没有收到 702 END");
    assert!(pcm_bytes > 8000, "音频数据太少：{pcm_bytes} 字节");

    m.send("QUIT");
    assert!(m.wait_for("210 OK QUIT", Duration::from_secs(5)).is_some(), "缺少 QUIT 应答");
    let status = m.child.wait().unwrap();
    assert!(status.success(), "模块退出码异常：{status}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn module_rejects_unknown_command() {
    let Some(sdk) = sdk_root() else {
        eprintln!("跳过：找不到 SDK 包");
        return;
    };
    let dir = temp_dir("unknown");
    let conf = dir.join("xfyun-tts.conf");
    write_conf(&conf, &sdk, &dir.join("work"));
    std::fs::create_dir_all(dir.join("work")).unwrap();

    let mut m = Module::start(&sdk, &dir.join("work"), &conf);
    m.send("INIT");
    if m.wait_for("299", Duration::from_secs(60)).is_none() {
        eprintln!("跳过：模块未能初始化（通常是授权/资源问题）");
        let _ = m.child.kill();
        return;
    }
    m.wait_for("299 OK", Duration::from_secs(5)).unwrap();
    m.send("BLABLA");
    let reply = m.wait_for("300", Duration::from_secs(5)).expect("未知命令应有 300 回复");
    assert!(reply.contains("UNKNOWN COMMAND"), "{reply}");
    let _ = m.child.kill();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 字节前缀判断（避免为二进制行构造 String）。
fn starts_with(line: &[u8], prefix: &str) -> bool {
    line.starts_with(prefix.as_bytes())
}

/// 与服务端 output.c 相同的还原规则：0x7d 后一字节取反第 5 位。
fn unescape_hdlc(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0x7d && i + 1 < data.len() {
            out.push(data[i + 1] ^ 0x20);
            i += 2;
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}
