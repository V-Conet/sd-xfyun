//! PCM → WAV 封装（16bit 小端，多声道交错）。

/// 生成 44 字节的 canonical WAV 头。
pub fn header(sample_rate: u32, channels: u16, bits: u16, data_len: Option<u32>) -> [u8; 44] {
    let byte_rate = sample_rate * channels as u32 * (bits as u32 / 8);
    let block_align = channels * bits / 8;
    // 流式输出（长度未知）时两个长度字段都写 0xFFFFFFFF：libsndfile 会把 data 长度
    // 当成真实帧数，写成 0 会让播放器认为“文件里没有音频”而直接正常退出。
    let (riff_size, data_size) = match data_len {
        Some(len) => (36u32.saturating_add(len), len),
        None => (0xFFFF_FFFF, 0xFFFF_FFFF),
    };
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&riff_size.to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    h[32..34].copy_from_slice(&block_align.to_le_bytes());
    h[34..36].copy_from_slice(&bits.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_size.to_le_bytes());
    h
}

/// 用已收集的 PCM 生成完整 WAV。
pub fn wrap(sample_rate: u32, channels: u16, bits: u16, pcm: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pcm.len() + 44);
    out.extend_from_slice(&header(sample_rate, channels, bits, Some(pcm.len() as u32)));
    out.extend_from_slice(pcm);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_fields() {
        let h = header(16000, 1, 16, Some(1000));
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(&h[8..12], b"WAVE");
        assert_eq!(u32::from_le_bytes(h[24..28].try_into().unwrap()), 16000);
        assert_eq!(u16::from_le_bytes(h[22..24].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(h[34..36].try_into().unwrap()), 16);
        assert_eq!(u32::from_le_bytes(h[4..8].try_into().unwrap()), 1036);
        assert_eq!(u32::from_le_bytes(h[40..44].try_into().unwrap()), 1000);
    }

    #[test]
    fn streaming_header_uses_unknown_length() {
        let h = header(16000, 1, 16, None);
        assert_eq!(u32::from_le_bytes(h[4..8].try_into().unwrap()), 0xFFFF_FFFF);
        assert_eq!(u32::from_le_bytes(h[40..44].try_into().unwrap()), 0xFFFF_FFFF);
    }

    #[test]
    fn wrap_appends_pcm() {
        let w = wrap(16000, 1, 16, &[1, 2, 3, 4]);
        assert_eq!(w.len(), 48);
        assert_eq!(&w[44..], &[1, 2, 3, 4]);
    }
}
