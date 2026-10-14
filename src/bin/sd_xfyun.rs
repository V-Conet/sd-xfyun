//! speech-dispatcher 输出模块可执行文件。
//!
//! speech-dispatcher 会以 `<模块> <配置文件路径>` 的方式拉起它，配置文件就是
//! `AddModule` 里指定的那个（如 /etc/speech-dispatcher/modules/xfyun-tts.conf）。

use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let config = args.next().map(PathBuf::from);
    if config.is_none() {
        eprintln!("[xfyun] 提示：未收到配置文件路径参数（speech-dispatcher 会传）");
    }
    std::process::exit(sd_xfyun::module::run(config));
}
