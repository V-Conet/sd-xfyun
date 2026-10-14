//! 发音人表与 speech-dispatcher 设置 → SDK 参数的映射。

/// 一个可选发音人。`name` 是 speech-dispatcher 侧看到的语种名，`vcn` 是引擎参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Voice {
    pub name: &'static str,
    pub vcn: &'static str,
    pub language: &'static str,
    pub language_id: i32,
    pub variant: Option<&'static str>,
}

/// 取自官方文档 6.6.1 的发音人列表（本包只带了小燕/小峰的中文资源，其余需要相应资源授权）。
pub const VOICES: &[Voice] = &[
    Voice { name: "xiaoyan", vcn: "xiaoyan", language: "zh-CN", language_id: 1, variant: Some("female") },
    Voice { name: "xiaofeng", vcn: "xiaofeng", language: "zh-CN", language_id: 1, variant: Some("male") },
    Voice { name: "xiaoyuan", vcn: "xiaoyuan", language: "zh-CN", language_id: 1, variant: Some("female") },
    Voice { name: "qianqian", vcn: "qianqian", language: "zh-CN", language_id: 1, variant: Some("female") },
    Voice { name: "xiaoguan", vcn: "xiaoguan", language: "zh-CN", language_id: 1, variant: Some("female") },
    Voice { name: "chongchong", vcn: "chongchong", language: "zh-CN", language_id: 1, variant: Some("female") },
    Voice { name: "xiaofang", vcn: "xiaofang", language: "zh-CN", language_id: 1, variant: Some("child_female") },
    Voice { name: "xiaomei", vcn: "xiaomei", language: "yue-HK", language_id: 12, variant: Some("female") },
    Voice { name: "catherine", vcn: "catherine", language: "en-US", language_id: 2, variant: Some("female") },
    Voice { name: "john", vcn: "john", language: "en-US", language_id: 2, variant: Some("male") },
    Voice { name: "mariane", vcn: "mariane", language: "fr-FR", language_id: 3, variant: Some("female") },
    Voice { name: "xiaolin", vcn: "xiaolin", language: "ja-JP", language_id: 5, variant: Some("female") },
    Voice { name: "zhongcun", vcn: "zhongcun", language: "ja-JP", language_id: 5, variant: Some("male") },
    Voice { name: "keshu", vcn: "keshu", language: "ru-RU", language_id: 6, variant: Some("female") },
    Voice { name: "abha", vcn: "abha", language: "hi-IN", language_id: 8, variant: Some("female") },
    Voice { name: "christiance", vcn: "christiance", language: "de-DE", language_id: 9, variant: Some("female") },
    Voice { name: "felisa", vcn: "felisa", language: "es-ES", language_id: 23, variant: Some("female") },
    Voice { name: "kim", vcn: "kim", language: "ko-KR", language_id: 16, variant: Some("female") },
    Voice { name: "anna", vcn: "anna", language: "it-IT", language_id: 15, variant: Some("female") },
    Voice { name: "suparut", vcn: "suparut", language: "th-TH", language_id: 27, variant: Some("female") },
];

/// 语种代码（可带地区）→ 引擎 language 参数。
pub fn language_id(language: &str) -> Option<i32> {
    let lang = language.split(['-', '_']).next().unwrap_or("").to_ascii_lowercase();
    Some(match lang.as_str() {
        "zh" | "cmn" => 1,
        "en" => 2,
        "fr" => 3,
        "ja" => 5,
        "ru" => 6,
        "hi" => 8,
        "de" => 9,
        "yue" => 12,
        "it" => 15,
        "ko" => 16,
        "es" => 23,
        "th" => 27,
        _ => return None,
    })
}

/// 按名字（发音人名或 vcn）查表。
pub fn find(name: &str) -> Option<&'static Voice> {
    let n = name.trim();
    if n.is_empty() || n.eq_ignore_ascii_case("NULL") {
        return None;
    }
    VOICES
        .iter()
        .find(|v| v.name.eq_ignore_ascii_case(n) || v.vcn.eq_ignore_ascii_case(n))
}

/// 某语种下的默认发音人。
pub fn default_for_language(language: &str) -> &'static Voice {
    let want = language.split(['-', '_']).next().unwrap_or("").to_ascii_lowercase();
    VOICES
        .iter()
        .find(|v| v.language.split('-').next().unwrap_or("").eq_ignore_ascii_case(&want))
        .unwrap_or(&VOICES[0])
}

/// speech-dispatcher 的 voice_type（male1/female1/child_female...）→ 本模块发音人。
pub fn for_voice_type(voice_type: &str, language: &str) -> &'static Voice {
    let want = language.split(['-', '_']).next().unwrap_or("").to_ascii_lowercase();
    if want.is_empty() || want == "null" {
        return default_for_language("zh-CN");
    }
    let variant_wanted = match voice_type.to_ascii_lowercase().as_str() {
        "male1" | "male2" | "male3" => Some("male"),
        "female1" | "female2" | "female3" => Some("female"),
        "child_male" => Some("child_male"),
        "child_female" => Some("child_female"),
        _ => None,
    };
    variant_wanted
        .and_then(|v| {
            VOICES.iter().find(|voice| {
                voice
                    .language
                    .split('-')
                    .next()
                    .unwrap_or("")
                    .eq_ignore_ascii_case(&want)
                    && voice.variant == Some(v)
            })
        })
        .unwrap_or_else(|| default_for_language(language))
}

/// 一次朗读实际下发给引擎的参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakSettings {
    pub vcn: &'static str,
    pub language_id: i32,
    pub speed: u8,
    pub pitch: u8,
    pub volume: u8,
}

impl SpeakSettings {
    pub fn from_voice(v: &'static Voice, speed: u8, pitch: u8, volume: u8) -> Self {
        SpeakSettings { vcn: v.vcn, language_id: v.language_id, speed, pitch, volume }
    }

    /// speech-dispatcher 的 SSC 取值范围是 -100..100，本模块内部是 0..100（默认 50）。
    pub fn sd_to_percent(sd: i32, default: u8) -> u8 {
        if sd == 0 {
            default
        } else {
            (50 + sd / 2).clamp(0, 100) as u8
        }
    }
}


/// 资源文件名里解析出发音人：`<hash>_<ver>_xTTS_<LangLang>_<voice>_<year>_<arch>.{irf,dat}`。
/// `front`/`Emb` 是前端/嵌入资源，不是发音人。
pub fn parse_resource_voice(filename: &str) -> Option<String> {
    let parts: Vec<&str> = filename.split('_').collect();
    let idx = parts.iter().position(|p| *p == "xTTS")?;
    let voice = parts.get(idx + 2)?.trim();
    if voice.is_empty()
        || voice.eq_ignore_ascii_case("front")
        || voice.eq_ignore_ascii_case("emb")
        || voice.eq_ignore_ascii_case("front_emb")
    {
        return None;
    }
    Some(voice.to_string())
}

/// 扫描资源目录（`<dir>/xtts/`）里真实存在的发音人。
///
/// 装了小燕/小峰的中文资源就只能用小燕小峰 —— 请求别的发音人时引擎直接返回
/// 18501（参数范围溢出）。所以这里以资源为准，而不是照抄文档里的全量列表。
pub fn available_voices(res_dirs: &[std::path::PathBuf]) -> Vec<&'static Voice> {
    let mut names: Vec<String> = Vec::new();
    for dir in res_dirs {
        let Ok(entries) = std::fs::read_dir(dir.join("xtts")) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(voice) = parse_resource_voice(&name) {
                if !names.iter().any(|n| n.eq_ignore_ascii_case(&voice)) {
                    names.push(voice);
                }
            }
        }
    }

    let mut found: Vec<&'static Voice> = Vec::new();
    for name in &names {
        match find(name) {
            Some(v) => found.push(v),
            None => crate::log_warn!(
                "资源目录里发现未登记的发音人 {name}（可用性未知），如需支持请补进 src/voices.rs"
            ),
        }
    }

    if found.is_empty() {
        crate::log_warn!("资源目录里没找到任何发音人资源，退回完整发音人表（合成可能失败）");
        return VOICES.iter().collect();
    }
    // 保持表内顺序，便于 LIST VOICES 输出稳定
    VOICES.iter().filter(|v| found.iter().any(|f| f.name == v.name)).collect()
}

/// 在可选列表里按名字找；找不到就退回默认（避免把不可用的发音人下发给引擎）。
pub fn pick<'a>(requested: Option<&str>, available: &'a [&'static Voice], fallback: &'a Voice) -> &'a Voice {
    match requested.and_then(find) {
        Some(v) => match available.iter().find(|a| a.name == v.name) {
            Some(a) => a,
            None => {
                crate::log_warn!(
                    "发音人 {} 在本机资源里不可用（只装了 {}），改用 {}",
                    v.name,
                    available.iter().map(|a| a.name).collect::<Vec<_>>().join("/"),
                    fallback.name
                );
                fallback
            }
        },
        None => fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_ids_match_docs() {
        assert_eq!(language_id("zh-CN"), Some(1));
        assert_eq!(language_id("en_US"), Some(2));
        assert_eq!(language_id("yue"), Some(12));
        assert_eq!(language_id("xx-YY"), None);
    }

    #[test]
    fn voice_lookup_by_name_and_vcn() {
        assert_eq!(find("xiaofeng").unwrap().vcn, "xiaofeng");
        assert_eq!(find("XIAOYAN").unwrap().language_id, 1);
        assert!(find("").is_none());
        assert!(find("NULL").is_none());
        assert!(find("no_such_voice").is_none());
    }

    #[test]
    fn voice_type_mapping_respects_language() {
        assert_eq!(for_voice_type("male1", "zh-CN").vcn, "xiaofeng");
        assert_eq!(for_voice_type("female1", "zh-CN").vcn, "xiaoyan");
        assert_eq!(for_voice_type("child_female", "zh-CN").vcn, "xiaofang");
        assert_eq!(for_voice_type("male1", "en-US").vcn, "john");
        assert_eq!(for_voice_type("female1", "en-US").vcn, "catherine");
        // 未知语种回落到中文默认发音人
        assert_eq!(for_voice_type("male1", "xx-XX").vcn, "xiaoyan");
    }

    #[test]
    fn parses_voice_from_resource_filename() {
        assert_eq!(
            parse_resource_voice("e3fe94474_1.0.0_xTTS_CnCn_xiaoyan_2018_arm.irf").as_deref(),
            Some("xiaoyan")
        );
        assert_eq!(
            parse_resource_voice("e4b08c6f3_1.0.0_xTTS_CnCn_xiaofeng_2018_fix_arm.dat").as_deref(),
            Some("xiaofeng")
        );
        assert_eq!(parse_resource_voice("e4caee636_1.0.2_xTTS_CnCn_front_Emb_arm_2017.irf"), None);
        assert_eq!(parse_resource_voice("libaikit.so"), None);
    }

    #[test]
    fn available_voices_follows_resources() {
        let dir = std::env::temp_dir().join(format!("xfyun-res-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(dir.join("xtts")).unwrap();
        std::fs::write(dir.join("xtts/e3fe94474_1.0.0_xTTS_CnCn_xiaoyan_2018_arm.irf"), "x").unwrap();
        std::fs::write(dir.join("xtts/e4caee636_1.0.2_xTTS_CnCn_front_Emb_arm_2017.irf"), "x").unwrap();
        let av = available_voices(std::slice::from_ref(&dir));
        assert_eq!(av.len(), 1);
        assert_eq!(av[0].name, "xiaoyan");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pick_falls_back_when_voice_unavailable() {
        let available: Vec<&'static Voice> = vec![&VOICES[0]];
        assert_eq!(pick(Some("xiaofeng"), &available, &VOICES[0]).name, "xiaoyan");
        assert_eq!(pick(Some("xiaoyan"), &available, &VOICES[0]).name, "xiaoyan");
        assert_eq!(pick(None, &available, &VOICES[0]).name, "xiaoyan");
    }

    #[test]
    fn sd_rate_conversion() {
        assert_eq!(SpeakSettings::sd_to_percent(0, 50), 50);
        assert_eq!(SpeakSettings::sd_to_percent(100, 50), 100);
        assert_eq!(SpeakSettings::sd_to_percent(-100, 50), 0);
        assert_eq!(SpeakSettings::sd_to_percent(50, 50), 75);
    }
}
