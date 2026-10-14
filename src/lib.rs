//! `sd-xfyun`：讯飞 AIkit 离线 XTTS 的 speech-dispatcher 后端。
//!
//! 两个产物：
//! - `sd_xfyun`：speech-dispatcher 输出模块（服务端直接拉起它，走模块协议）；
//! - `xfyun-say`：命令行工具，既能单独合成，也能配合 sd_generic 模块使用。
//!
//! 分层：`ffi`(SDK C 接口) → `engine`(生命周期/合成) → `sink`(音频出口) /
//! `module`(speech-dispatcher 模块协议) / `cli`(命令行)。

pub mod cli;
pub mod config;
pub mod engine;
pub mod errcode;
pub mod ffi;
pub mod keys;
pub mod license;
pub mod loader;
pub mod log;
pub mod module;
pub mod proto;
pub mod sink;
pub mod voices;
pub mod wav;

pub use config::Config;

/// 供两个 bin 共用：解析参数、初始化日志、必要时 re-exec 让引擎库可加载。
pub fn bootstrap_logging(cfg: &Config, extra_log: Option<&std::path::Path>) {
    log::set_level(cfg.log_level);
    let file = extra_log.or(cfg.log_file.as_deref());
    log::set_file(file);
}

/// 版本信息（不包含任何密钥内容，只给指纹）。
pub fn version_line(engine: Option<&engine::Engine>) -> String {
    let mut line = format!("sd-xfyun {} (sdk {})", env!("CARGO_PKG_VERSION"), keys::ability());
    if let Some(e) = engine {
        line.push_str(&format!(
            "\n  SDK 版本: {}   引擎: {}",
            e.version().unwrap_or("?"),
            e.engine_version().unwrap_or("(SDK 未上报)")
        ));
        line.push_str(&format!(
            "\n  设备指纹: {}",
            e.device_id().map(|d| &d[..d.len().min(24)]).unwrap_or("?")
        ));
        line.push_str(&format!("\n  密钥指纹: {}", e.key_fingerprint()));
    }
    line
}
