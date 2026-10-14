//! 编译期写入的密钥（build.rs 生成）+ 运行时覆盖。
//!
//! 优先使用环境变量（方便开发/临时切换），否则用构建时混淆进二进制的值。
//! 编译期值由 obfstr 在编译时混淆，运行时按需解密，不会以明文出现在
//! `.rodata` 里，也不参与 `--version` 等输出。

use std::ffi::CString;

include!(concat!(env!("OUT_DIR"), "/keys_gen.rs"));

/// 编译期写入的能力 ID（与授权文件绑定）。
pub fn ability() -> String {
    ability_compiled()
}

/// SDK 源码树/包根目录（编译期探测，作为运行时默认值）。
pub fn compiled_sdk_root() -> String {
    sdk_root_compiled()
}

/// 密钥三元组，附带 NUL 结尾的 C 字符串形式（供 FFI 直接使用）。
pub struct Keys {
    pub app_id: CString,
    pub api_key: CString,
    pub api_secret: CString,
    /// 密钥指纹（SHA-256 前 8 字节 hex），用于诊断而不泄露密钥本体。
    pub fingerprint: String,
}

impl Keys {
    pub fn load() -> Result<Keys, String> {
        let app_id = pick("XFYUN_APP_ID", app_id_compiled);
        let api_key = pick("XFYUN_API_KEY", api_key_compiled);
        let api_secret = pick("XFYUN_API_SECRET", api_secret_compiled);

        if app_id.is_empty() || api_key.is_empty() || api_secret.is_empty() {
            return Err(
                "缺少讯飞密钥：请在构建时提供 XFYUN_APP_ID / XFYUN_API_KEY / XFYUN_API_SECRET\
                 （环境变量或 keys.toml），参见 README"
                    .to_string(),
            );
        }
        let fingerprint = fingerprint_of(&app_id, &api_key);
        let to_c = |s: String, what: &str| {
            CString::new(s).map_err(|_| format!("{what} 含 NUL 字符"))
        };
        Ok(Keys {
            app_id: to_c(app_id, "appID")?,
            api_key: to_c(api_key, "apiKey")?,
            api_secret: to_c(api_secret, "apiSecret")?,
            fingerprint,
        })
    }
}

fn pick(env: &str, compiled: fn() -> String) -> String {
    match std::env::var(env) {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => compiled(),
    }
}

/// 只用 appID + apiKey 做短指纹，不泄露 apiSecret。
pub fn fingerprint_of(app_id: &str, api_key: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in app_id.bytes().chain(b"|".iter().copied()).chain(api_key.bytes()) {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_and_hides_secret() {
        let a = fingerprint_of("aa2bd560", "key123");
        assert_eq!(a, fingerprint_of("aa2bd560", "key123"));
        assert_ne!(a, fingerprint_of("aa2bd560", "key124"));
        assert_eq!(a.len(), 16);
        assert!(!a.contains("key123"));
    }
}
