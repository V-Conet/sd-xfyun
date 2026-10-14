//! 命令行合成工具（也可作为 sd_generic 模块的合成命令）。

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(sd_xfyun::cli::run(args));
}
