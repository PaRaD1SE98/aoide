//! 语音识别 API：OpenAI 格式的 /audio/transcriptions 接口
//!
//! 这是语音识别 API 事实上的通用格式：OpenAI、Groq、硅基流动、Mistral 等云服务，
//! 以及自建的 vLLM、LocalAI 等都提供。其他格式的服务可以经 LiteLLM 等代理转成这个格式。
//!
//! 请求：multipart 表单，file（音频）+ model，可选 language（ISO-639-1）、prompt（提示词，用来放词库）；
//! 响应：{"text": "..."}

use crate::config::Config;
use crate::i18n::tr;
use crate::trf;
use reqwest::blocking::multipart::{Form, Part};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(60);

/// 识别 16 kHz 单声道音频；prompt 为词库里的词（可空）
pub fn transcribe(audio: &[f32], sample_rate: u32, prompt: &str, cfg: &Config) -> Result<String, String> {
    let base = cfg.speech_api.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err(tr("未配置语音识别 API（在设置中填写 API 地址）", "Speech recognition API is not configured (set the API base in Settings)", "音声認識 API が設定されていません（設定で API のアドレスを入力してください）").into());
    }
    let model = cfg.speech_api_model.trim();
    if model.is_empty() {
        return Err(tr("未填写语音识别 API 的模型名", "Set the model name of the speech recognition API in Settings", "音声認識 API のモデル名を設定してください").into());
    }
    let wav = encode_wav(audio, sample_rate);
    // 识别语言：自动检测时不传；粤语不在 ISO-639-1 里，交给服务端自动检测
    let language = Some(cfg.language.as_str()).filter(|l| !matches!(*l, "auto" | "yue"));
    let url = format!("{base}/audio/transcriptions");
    let start = Instant::now();
    // 可选参数（language、prompt）个别服务端不认，被拒绝（400 / 422）时去掉再试一次
    let result = match post(&url, cfg, &wav, model, language, prompt) {
        Err((Some(400 | 422), e)) if language.is_some() || !prompt.is_empty() => {
            log::info!("Speech API rejected optional fields, retrying without them: {e}");
            post(&url, cfg, &wav, model, None, "")
        }
        r => r,
    };
    let text = result.map_err(|(_, e)| trf!("语音识别 API 调用失败：{e}", "Speech recognition API failed: {e}", "音声認識 API の呼び出しに失敗しました: {e}"))?;
    log::info!("Speech API: {model} answered in {:?}", start.elapsed());
    Ok(text)
}

fn post(url: &str, cfg: &Config, wav: &[u8], model: &str, language: Option<&str>, prompt: &str) -> Result<String, (Option<u16>, String)> {
    let client = reqwest::blocking::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| (None, e.to_string()))?;
    let file = Part::bytes(wav.to_vec()).file_name("audio.wav").mime_str("audio/wav").map_err(|e| (None, e.to_string()))?;
    let mut form = Form::new().part("file", file).text("model", model.to_string());
    if let Some(language) = language {
        form = form.text("language", language.to_string());
    }
    if !prompt.is_empty() {
        form = form.text("prompt", prompt.to_string());
    }
    let mut req = client.post(url).multipart(form);
    let key = cfg.speech_api_key.trim();
    if !key.is_empty() {
        req = req.bearer_auth(key);
    }
    let resp = req.send().map_err(|e| (None, e.to_string()))?;
    let status = resp.status();
    let body = resp.text().map_err(|e| (None, e.to_string()))?;
    if !status.is_success() {
        let body: String = body.chars().take(300).collect();
        return Err((Some(status.as_u16()), format!("HTTP {status}: {body}")));
    }
    parse_text(&body).ok_or_else(|| (None, format!("unexpected response: {}", body.chars().take(300).collect::<String>())))
}

/// 响应一般是 {"text": "..."}；少数服务端按 response_format=text 的习惯直接返回纯文本
fn parse_text(body: &str) -> Option<String> {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) => v["text"].as_str().map(|t| t.trim().to_string()),
        Err(_) => Some(body.trim().to_string()),
    }
}

/// 编码成 16 位 PCM 的 WAV 文件
fn encode_wav(audio: &[f32], sample_rate: u32) -> Vec<u8> {
    let data_len = (audio.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt 块长度
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // 单声道
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // 每秒字节数
    out.extend_from_slice(&2u16.to_le_bytes()); // 每帧字节数
    out.extend_from_slice(&16u16.to_le_bytes()); // 位深
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for &s in audio {
        out.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_wav_header() {
        let wav = encode_wav(&[0.0, 1.0, -1.0], 16000);
        assert_eq!(wav.len(), 44 + 6);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 36 + 6);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16000);
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 6);
        assert_eq!(i16::from_le_bytes([wav[46], wav[47]]), 32767);
        assert_eq!(i16::from_le_bytes([wav[48], wav[49]]), -32767);
    }

    #[test]
    fn parses_json_or_plain_text() {
        assert_eq!(parse_text(r#"{"text":" 你好 "}"#).as_deref(), Some("你好"));
        assert_eq!(parse_text("hello world\n").as_deref(), Some("hello world"));
        assert_eq!(parse_text(r#"{"error":"x"}"#), None);
    }

    /// 手动运行（会用本机配置里的语音识别 API）：VT_WAV=… cargo test --lib speech_api_with_wav -- --ignored --nocapture
    #[test]
    #[ignore]
    fn speech_api_with_wav() {
        let cfg = Config::load();
        let bytes = std::fs::read(std::env::var("VT_WAV").unwrap()).unwrap();
        let audio: Vec<f32> = bytes[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect();
        println!("{:?}", transcribe(&audio, 16000, "", &cfg));
    }
}
