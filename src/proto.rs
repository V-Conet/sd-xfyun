//! speech-dispatcher 输出模块协议（模块 ↔ 服务端，行式文本，走 stdin/stdout）。
//!
//! 协议要点（与上游 `src/modules/module_process.c` 对齐）：
//! - 启动后服务端先发 `INIT`，模块回 `299-<说明>` + `299 OK LOADED SUCCESSFULLY`
//!   （失败回 `399-…` + `399 ERR CANT INIT MODULE` 并退出）；
//! - 多行消息：除最后一行外，行内第 4 个字符是 `-`（如 `705-bits=16`）；
//!   最后一行第 4 个字符是空格（如 `705 AUDIO`）；
//! - 说话：`SPEAK` → 模块回 `202 OK RECEIVING MESSAGE` → 服务端发文本行 + `.`
//!   → 模块回 `200 OK SPEAKING` → 事件 `701 BEGIN` → 音频 → `702 END`；
//! - 打断：`STOP` / `PAUSE` 无回复，模块随后用 `703 STOP` / `704 PAUSE` 事件收尾。

use std::io::{self, BufRead, BufReader, Write};
use std::sync::Mutex;

use crate::sink::PcmFormat;

/// 协议 stdout 写端。所有多行消息都在同一把锁里写完，避免与服务端的事件读取交叉。
pub struct ProtoWriter {
    out: Mutex<io::Stdout>,
}

impl Default for ProtoWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProtoWriter {
    pub fn new() -> Self {
        ProtoWriter { out: Mutex::new(io::stdout()) }
    }

    /// 单行事件或回复（`701 BEGIN`、`203 OK SETTINGS RECEIVED` 等）。
    pub fn event(&self, line: &str) {
        self.raw(line.as_bytes());
        self.raw(b"\n");
    }

    /// 多行回复（如 `LIST VOICES` 的结果）。
    pub fn multiline(&self, lines: &[String]) {
        let mut guard = self.out.lock().unwrap_or_else(|e| e.into_inner());
        for line in lines {
            let _ = guard.write_all(line.as_bytes());
            let _ = guard.write_all(b"\n");
        }
        let _ = guard.flush();
    }

    /// 一块音频（705 协议），整块原子写出。
    pub fn audio_track(&self, format: &PcmFormat, data: &[u8]) {
        let samples = data.len() as u64 * 8 / (format.bits.max(1) as u64 * format.channels.max(1) as u64);
        let mut head = String::with_capacity(160);
        head.push_str(&format!("705-bits={}\n", format.bits));
        head.push_str(&format!("705-num_channels={}\n", format.channels));
        head.push_str(&format!("705-sample_rate={}\n", format.sample_rate));
        head.push_str(&format!("705-num_samples={}\n", samples));
        head.push_str("705-big_endian=0\n"); // 0 = little endian（SPD_AUDIO_LE）
        head.push_str("705-AUDIO");

        let mut guard = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = guard.write_all(head.as_bytes());
        let _ = guard.write_all(&[0u8]);
        let _ = guard.write_all(&escape_hdlc(data));
        let _ = guard.write_all(b"\n705 AUDIO\n");
        let _ = guard.flush();
    }

    fn raw(&self, bytes: &[u8]) {
        let mut guard = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = guard.write_all(bytes);
        let _ = guard.flush();
    }
}

/// HDLC 转义：0x7d 与换行前插 0x7d，并把原字节第 5 位取反（服务端按同规则还原）。
pub fn escape_hdlc(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 32);
    for &b in data {
        if b == 0x7d || b == b'\n' {
            out.push(0x7d);
            out.push(b ^ 0x20);
        } else {
            out.push(b);
        }
    }
    out
}

/// 协议 stdin 读端。
pub struct ProtoReader {
    input: BufReader<io::Stdin>,
}

impl Default for ProtoReader {
    fn default() -> Self {
        Self::new()
    }
}

impl ProtoReader {
    pub fn new() -> Self {
        ProtoReader { input: BufReader::new(io::stdin()) }
    }

    /// 读一行（去掉行尾 CR/LF）。返回 `None` 表示对端关闭。
    pub fn read_line(&mut self) -> io::Result<Option<String>> {
        let mut buf = Vec::new();
        let n = self.input.read_until(b'\n', &mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
            buf.pop();
        }
        Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
    }

    /// 读取 `KEY=VALUE` 块，直到单独一行的 `.`。
    pub fn read_settings(&mut self) -> io::Result<Vec<(String, String)>> {
        let mut items = Vec::new();
        while let Some(line) = self.read_line()? {
            if line == "." {
                break;
            }
            if line.is_empty() {
                continue;
            }
            match line.split_once('=') {
                Some((k, v)) => items.push((k.to_string(), v.to_string())),
                None => crate::log_debug!("忽略无 = 的设置行：{line}"),
            }
        }
        Ok(items)
    }

    /// 读取说话文本块（`SPEAK` 之后的数据），处理点填充，直到单独一行的 `.`。
    pub fn read_message(&mut self) -> io::Result<Option<String>> {
        let mut text = String::new();
        while let Some(line) = self.read_line()? {
            if line == "." {
                // 去掉行尾多余的换行（上游实现同样处理）
                if text.ends_with('\n') {
                    text.pop();
                }
                return Ok(Some(text));
            }
            let line = line.strip_prefix('.').unwrap_or(&line);
            text.push_str(line);
            text.push('\n');
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_newline_and_escape_byte() {
        let out = escape_hdlc(&[0x00, b'\n', 0x7d, 0xff]);
        assert_eq!(out, vec![0x00, 0x7d, 0x0a ^ 0x20, 0x7d, 0x7d ^ 0x20, 0xff]);
    }

    #[test]
    fn escaping_removes_raw_newlines() {
        let escaped = escape_hdlc(b"line1\nline2\n");
        assert!(!escaped.contains(&b'\n'));
    }

    #[test]
    fn audio_samples_counted_from_bits_and_channels() {
        let fmt = PcmFormat::default();
        let data = vec![0u8; 960];
        let samples = data.len() as u64 * 8 / (fmt.bits as u64 * fmt.channels as u64);
        assert_eq!(samples, 480);
    }
}
