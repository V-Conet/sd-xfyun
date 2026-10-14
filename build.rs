//! 构建脚本：
//! 1. 定位 SDK（libs/libaikit.so、引擎库、xtts 资源），配置链接与 rpath；
//! 2. 把密钥（appID / apiKey / apiSecret）在**编译期**写入并用 obfstr 混淆，
//!    运行时按需解密，明文不会出现在 .rodata，也不会被 `--version` 等输出。

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // ---------- 1. SDK 位置 ----------
    let sdk_root = resolve_sdk_root(&manifest_dir);
    let libs_dir = env::var("XFYUN_LIBS_DIR").map(PathBuf::from).unwrap_or_else(|_| sdk_root.join("libs"));

    println!("cargo:rerun-if-env-changed=XFYUN_SDK_ROOT");
    println!("cargo:rerun-if-env-changed=XFYUN_LIBS_DIR");
    println!("cargo:rerun-if-env-changed=XFYUN_SKIP_DEV_RPATH");
    println!("cargo:rerun-if-env-changed=XFYUN_DEFAULT_SDK_ROOT");
    println!("cargo:rerun-if-changed=build.rs");

    let libaikit = libs_dir.join("libaikit.so");
    if !libaikit.is_file() {
        panic!(
            "找不到 {}：请把讯飞 AIkit Linux SDK 包放到 {}/3rd_party/ 下\n\
             （该目录里应有 libs/libaikit.so、libs/*_aee.so、xtts/），或用 XFYUN_SDK_ROOT / XFYUN_LIBS_DIR 指定。",
            libaikit.display(),
            manifest_dir.display()
        );
    }
    let engine = fs::read_dir(&libs_dir)
        .ok()
        .and_then(|it| {
            it.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .find(|n| n.ends_with(".so") && n.contains("_aee"))
        });
    match &engine {
        Some(name) => println!("cargo:warning=使用引擎库 {name}（运行时需在动态库搜索路径中）"),
        None => println!(
            "cargo:warning={} 下没有找到 *_aee.so 引擎库，SDK 初始化会返回 18201",
            libs_dir.display()
        ),
    }
    if !sdk_root.join("xtts").is_dir() {
        println!(
            "cargo:warning={} 下没有 xtts/ 资源目录（可运行时用 XfyunResDir 指定）",
            sdk_root.display()
        );
    }

    // 链接 libaikit.so。除开发布局（SDK 原目录的绝对路径）外，还写入一批 $ORIGIN 相对路径，
    // 覆盖两种安装布局（模块在 /usr/lib/speech-dispatcher/speech-dispatcher-modules/、
    // 命令行在 /usr/bin/，SDK 库在 /usr/lib/sd-xfyun/；用户级安装同理在 ~/.local 下）。
    // 注意：引擎库是 libaikit 自己 dlopen 的，RPATH 对它无效，需要 LD_LIBRARY_PATH
    // （程序启动时会自动 re-exec 补上，见 src/loader.rs）。
    println!("cargo:rustc-link-search=native={}", libs_dir.display());
    println!("cargo:rustc-link-lib=dylib=aikit");
    // 打包（makepkg/发行）时不要再把构建树里的绝对路径写进 rpath，
    // 否则包里的二进制会带着 $srcdir 引用；此时只保留下面那些 $ORIGIN 相对路径。
    if !truthy_env("XFYUN_SKIP_DEV_RPATH") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", libs_dir.display());
    }
    println!(
        "cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN:$ORIGIN/../lib/sd-xfyun:$ORIGIN/../../sd-xfyun"
    );

    // ---------- 2. 密钥与能力 ID ----------
    let keys = load_keys(&manifest_dir);
    let ability = env::var("XFYUN_ABILITY").unwrap_or_else(|_| "e2e44feff".to_string());
    // 编译期默认 SDK 根目录：打包时改成安装后的路径（如 /usr/share/sd-xfyun），
    // 免得二进制里残留构建树路径。
    let default_root = env::var("XFYUN_DEFAULT_SDK_ROOT")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| sdk_root.to_string_lossy().into_owned());

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let generated = format!(
        "// 由 build.rs 生成，请勿手工修改。\n\
         pub fn app_id_compiled() -> String {{ obfstr::obfstring!(\"{}\").to_string() }}\n\
         pub fn api_key_compiled() -> String {{ obfstr::obfstring!(\"{}\").to_string() }}\n\
         pub fn api_secret_compiled() -> String {{ obfstr::obfstring!(\"{}\").to_string() }}\n\
         pub fn ability_compiled() -> String {{ obfstr::obfstring!(\"{}\").to_string() }}\n\
         pub fn sdk_root_compiled() -> String {{ obfstr::obfstring!(\"{}\").to_string() }}\n",
        escape(&keys.app_id),
        escape(&keys.api_key),
        escape(&keys.api_secret),
        escape(&ability),
        escape(&default_root),
    );
    fs::write(out_dir.join("keys_gen.rs"), generated).expect("写入 keys_gen.rs 失败");

    if !keys.found {
        panic!(
            "没有找到讯飞密钥（构建期写入二进制，不会有运行时读取）。二选一：\n\
             \x20 1) 在源码树根目录建 keys.toml，三行：\n\
             \x20      XFYUN_APP_ID = \"…\"\n\
             \x20      XFYUN_API_KEY = \"…\"\n\
             \x20      XFYUN_API_SECRET = \"…\"\n\
             \x20 2) 或用环境变量：XFYUN_APP_ID / XFYUN_API_KEY / XFYUN_API_SECRET cargo build --release"
        );
    }
}

fn truthy_env(name: &str) -> bool {
    match env::var(name) {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "" | "0" | "off" | "no" | "false"),
        Err(_) => false,
    }
}

#[derive(Default)]
struct Keys {
    app_id: String,
    api_key: String,
    api_secret: String,
    found: bool,
}

/// 密钥来源：环境变量优先，其次源码树根目录的 `keys.toml`。
///
/// `keys.toml` 不入库 —— fork 用户把它放在自己的私有仓库里即可，`cargo build` 直接读。
/// 格式就是三行：
/// ```toml
/// XFYUN_APP_ID = "xxxxxxx"
/// XFYUN_API_KEY = "xxxxxxxxxxxxxxxx"
/// XFYUN_API_SECRET = "xxxxxxxxxxxxxxxx"
/// ```
fn load_keys(manifest_dir: &Path) -> Keys {
    let mut keys = Keys {
        app_id: env::var("XFYUN_APP_ID").unwrap_or_default(),
        api_key: env::var("XFYUN_API_KEY").unwrap_or_default(),
        api_secret: env::var("XFYUN_API_SECRET").unwrap_or_default(),
        found: false,
    };
    for var in ["XFYUN_APP_ID", "XFYUN_API_KEY", "XFYUN_API_SECRET"] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    if !keys.app_id.is_empty() && !keys.api_key.is_empty() && !keys.api_secret.is_empty() {
        keys.found = true;
        return keys;
    }

    for name in ["keys.toml"] {
        let path = manifest_dir.join(name);
        println!("cargo:rerun-if-changed={}", path.display());
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let parsed = parse_simple_toml(&text);
        if keys.app_id.is_empty() {
            keys.app_id = parsed.get("XFYUN_APP_ID").cloned().unwrap_or_default();
        }
        if keys.api_key.is_empty() {
            keys.api_key = parsed.get("XFYUN_API_KEY").cloned().unwrap_or_default();
        }
        if keys.api_secret.is_empty() {
            keys.api_secret = parsed.get("XFYUN_API_SECRET").cloned().unwrap_or_default();
        }
        if !keys.app_id.is_empty() && !keys.api_key.is_empty() && !keys.api_secret.is_empty() {
            keys.found = true;
            println!("cargo:warning=已从源码树根目录的 keys.toml 读取讯飞密钥");
            break;
        }
    }
    keys
}

/// 只支持 `KEY = "value"` / `KEY = value` 这种平铺写法，避免引入 toml 依赖。
fn parse_simple_toml(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("");
        let Some((k, v)) = line.split_once('=') else { continue };
        let key = k.trim().to_string();
        let value = v.trim().trim_matches('"').trim_matches('\'').to_string();
        if !key.is_empty() && !value.is_empty() {
            map.insert(key, value);
        }
    }
    map
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn resolve_sdk_root(manifest_dir: &Path) -> PathBuf {
    // 1) 环境变量优先（分发/特殊布局时用）
    if let Ok(dir) = env::var("XFYUN_SDK_ROOT") {
        let p = PathBuf::from(&dir);
        if is_sdk_root(&p) {
            return p;
        }
        println!("cargo:warning=XFYUN_SDK_ROOT={dir} 下没有 libs/libaikit.so，忽略该设置");
    }

    // 2) 3rd_party/：本身是 SDK 目录，或下面某个子目录是（取字典序第一个）
    let third_party = manifest_dir.join("3rd_party");
    if is_sdk_root(&third_party) {
        return third_party;
    }
    if let Ok(entries) = fs::read_dir(&third_party) {
        let mut candidates: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| is_sdk_root(p))
            .collect();
        candidates.sort();
        if let Some(found) = candidates.into_iter().next() {
            return found;
        }
    }

    // 3) 兜底：报错信息里告诉调用者该放哪儿
    manifest_dir.join("3rd_party")
}

/// SDK 包根目录的标志：libs/libaikit.so
fn is_sdk_root(dir: &Path) -> bool {
    dir.join("libs").join("libaikit.so").is_file()
}
