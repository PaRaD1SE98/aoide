//! 提示音（Kenney Interface Sounds，CC0，见 sounds/LICENSE.txt）。播放见各平台的 sound.rs

use std::sync::OnceLock;

/// 相对原始音效文件的音量
const VOLUME: f32 = 0.5;

#[derive(Clone, Copy, Debug)]
pub enum Cue {
    /// 开始录音
    Start,
    /// 开始语音修订录音
    ReviseStart,
    /// 结束录音
    Stop,
    /// 取消 / 没有录到声音
    Cancel,
    /// 出错（和提示小窗里的错误消息一起播放）
    Error,
}

static START: &[u8] = include_bytes!("../sounds/drop_004.wav");
static REVISE_START: &[u8] = include_bytes!("../sounds/drop_003.wav");
static STOP: &[u8] = include_bytes!("../sounds/confirmation_001.wav");
static CANCEL: &[u8] = include_bytes!("../sounds/minimize_008.wav");
static ERROR: &[u8] = include_bytes!("../sounds/error_008.wav");

/// 按 VOLUME 缩放后的 WAV 数据（首次使用时生成，之后常驻，异步播放时一直有效）
static SCALED: OnceLock<[Vec<u8>; 5]> = OnceLock::new();

/// 缩放 16 位 PCM WAV 的采样；格式不符时原样返回
fn scale_wav(wav: &[u8], volume: f32) -> Vec<u8> {
    let mut out = wav.to_vec();
    if wav.len() < 12 || &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return out;
    }
    let mut bits = 0u16;
    let mut i = 12;
    while i + 8 <= wav.len() {
        let id = &wav[i..i + 4];
        let len = u32::from_le_bytes(wav[i + 4..i + 8].try_into().unwrap()) as usize;
        let body = i + 8..(i + 8 + len).min(wav.len());
        if id == b"fmt " && len >= 16 {
            let format = u16::from_le_bytes([wav[i + 8], wav[i + 9]]);
            bits = if format == 1 { u16::from_le_bytes([wav[i + 22], wav[i + 23]]) } else { 0 };
        } else if id == b"data" && bits == 16 {
            for s in out[body].chunks_exact_mut(2) {
                let v = (i16::from_le_bytes([s[0], s[1]]) as f32 * volume) as i16;
                s.copy_from_slice(&v.to_le_bytes());
            }
        }
        i += 8 + len + (len & 1); // 块按偶数字节对齐
    }
    out
}

fn bytes(cue: Cue) -> &'static [u8] {
    let scaled = SCALED.get_or_init(|| {
        [START, REVISE_START, STOP, CANCEL, ERROR].map(|wav| scale_wav(wav, VOLUME))
    });
    match cue {
        Cue::Start => &scaled[0],
        Cue::ReviseStart => &scaled[1],
        Cue::Stop => &scaled[2],
        Cue::Cancel => &scaled[3],
        Cue::Error => &scaled[4],
    }
}

/// 异步播放，立即返回
pub fn play(cue: Cue) {
    crate::platform::sound::play(bytes(cue), false);
}

/// 提示音的时长（按 WAV 头计算），另加一点余量：系统开始播放有少许延迟
pub fn length(cue: Cue) -> std::time::Duration {
    const MARGIN: std::time::Duration = std::time::Duration::from_millis(60);
    wav_length(bytes(cue)).unwrap_or(std::time::Duration::from_millis(400)) + MARGIN
}

/// WAV 的播放时长：data 块的字节数 ÷ 每秒字节数
fn wav_length(wav: &[u8]) -> Option<std::time::Duration> {
    if wav.len() < 12 || &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return None;
    }
    let mut byte_rate = 0u32;
    let mut i = 12;
    while i + 8 <= wav.len() {
        let id = &wav[i..i + 4];
        let len = u32::from_le_bytes(wav[i + 4..i + 8].try_into().ok()?) as usize;
        if id == b"fmt " && len >= 16 && i + 20 <= wav.len() {
            byte_rate = u32::from_le_bytes(wav[i + 16..i + 20].try_into().ok()?);
        } else if id == b"data" && byte_rate > 0 {
            let len = len.min(wav.len() - (i + 8));
            return Some(std::time::Duration::from_secs_f64(len as f64 / byte_rate as f64));
        }
        i += 8 + len + (len & 1);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_cue_length() {
        for cue in [Cue::Start, Cue::ReviseStart, Cue::Stop, Cue::Cancel, Cue::Error] {
            let length = wav_length(bytes(cue)).unwrap();
            println!("{cue:?}: {length:?}");
            assert!(length > std::time::Duration::from_millis(50) && length < std::time::Duration::from_secs(2));
        }
    }

    #[test]
    fn halves_pcm_samples_only() {
        let scaled = scale_wav(START, 0.5);
        assert_eq!(scaled.len(), START.len());
        // 头部（RIFF / fmt）不变
        assert_eq!(&scaled[..36], &START[..36]);
        let peak = |w: &[u8]| {
            let data = w.windows(4).position(|c| c == b"data").unwrap() + 8;
            w[data..].chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]]).unsigned_abs()).max().unwrap()
        };
        let (before, after) = (peak(START), peak(&scaled));
        assert!(before > 1000);
        assert!((after as i32 - before as i32 / 2).abs() <= 1, "{before} -> {after}");
    }
}
