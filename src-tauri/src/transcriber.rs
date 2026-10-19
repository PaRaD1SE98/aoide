//! 语音识别：whisper.cpp / SenseVoice（sherpa-onnx）模型加载与转录

use crate::audio::{self, SAMPLE_RATE};
use crate::config::Config;
use crate::models::{self, Engine};
use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, OfflineSenseVoiceModelConfig};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperVadContext,
    WhisperVadContextParams, WhisperVadParams,
};

enum Model {
    Whisper(WhisperContext),
    SenseVoice(OfflineRecognizer),
}

/// 加载时的关键配置；任何一项变化都要重新加载
#[derive(PartialEq)]
struct Key {
    path: PathBuf,
    gpu: bool,
    /// SenseVoice 的识别语言在创建识别器时固定
    language: String,
}

impl Key {
    fn of(cfg: &Config) -> Self {
        let engine = models::engine_of(&cfg.model);
        Key {
            path: models::model_path(&cfg.model_dir(), &cfg.model),
            gpu: engine == Engine::Whisper && cfg.use_gpu(),
            language: if engine == Engine::SenseVoice { cfg.language.clone() } else { String::new() },
        }
    }
}

struct Loaded {
    key: Key,
    model: Model,
    /// Silero VAD；模型文件缺失时为 None（不做静音切除）
    vad: Option<WhisperVadContext>,
}

/// 懒加载模型；配置变更时 reset() 后按新配置重新加载
#[derive(Default)]
pub struct Transcriber {
    inner: Mutex<Option<Loaded>>,
}

impl Transcriber {
    fn ensure<'a>(slot: &'a mut Option<Loaded>, cfg: &Config) -> Result<&'a mut Loaded, String> {
        let key = Key::of(cfg);
        if slot.as_ref().is_some_and(|l| l.key == key) {
            return Ok(slot.as_mut().unwrap());
        }
        *slot = None;
        if !models::is_downloaded(&cfg.model_dir(), &cfg.model) {
            return Err(crate::trf!("模型 {} 未下载（在设置中下载）", "Model {} is not downloaded (download it in Settings)", cfg.model));
        }
        log::info!("Loading model '{}' (gpu={})...", cfg.model, key.gpu);
        let start = Instant::now();
        let model = match models::engine_of(&cfg.model) {
            Engine::Whisper => {
                let mut params = WhisperContextParameters::default();
                params.use_gpu(key.gpu).flash_attn(key.gpu);
                let ctx = WhisperContext::new_with_params(&key.path, params)
                    .map_err(|e| crate::trf!("模型加载失败：{e}", "Failed to load the model: {e}"))?;
                warm_up(&ctx);
                Model::Whisper(ctx)
            }
            Engine::SenseVoice => Model::SenseVoice(load_sense_voice(&key.path, &cfg.language)?),
        };
        let vad = load_vad(cfg);
        log::info!("Model loaded! ({:?})", start.elapsed());
        Ok(slot.insert(Loaded { key, model, vad }))
    }

    pub fn preload(&self, cfg: &Config) -> Result<(), String> {
        let mut slot = self.inner.lock().unwrap();
        Self::ensure(&mut slot, cfg).map(|_| ())
    }

    pub fn reset(&self) {
        *self.inner.lock().unwrap() = None;
    }

    /// audio: 16 kHz 单声道；返回拼接后的文本
    pub fn transcribe(&self, audio: &[f32], cfg: &Config) -> Result<String, String> {
        let mut slot = self.inner.lock().unwrap();
        let loaded = Self::ensure(&mut slot, cfg)?;

        // 解码前先用 VAD 切掉静音段：没有音频进解码器就编不出东西，是抗幻觉的第一道防线。
        // （whisper.cpp 只在 whisper_full() 里做 VAD，whisper_full_with_state() 会跳过，所以这里自己做）
        let speech = match loaded.vad.as_mut() {
            Some(vad) => match speech_only(vad, audio) {
                Some(s) => s,
                None => {
                    log::info!("VAD: no speech detected");
                    return Ok(String::new());
                }
            },
            None => audio.to_vec(),
        };
        match &loaded.model {
            Model::Whisper(ctx) => {
                // 过短的音频用静音补齐（whisper.cpp 对不足 1 秒的输入直接返回空）
                let speech = audio::pad_to_min(speech, cfg.min_duration.max(1), "Speech");
                decode(ctx, &speech, cfg)
            }
            Model::SenseVoice(rec) => {
                let stream = rec.create_stream();
                stream.accept_waveform(SAMPLE_RATE as i32, &speech);
                rec.decode(&stream);
                Ok(stream.get_result().map(|r| r.text.trim().to_string()).unwrap_or_default())
            }
        }
    }
}

fn load_sense_voice(dir: &std::path::Path, language: &str) -> Result<OfflineRecognizer, String> {
    let mut config = OfflineRecognizerConfig::default();
    config.model_config.sense_voice = OfflineSenseVoiceModelConfig {
        model: Some(dir.join("model.int8.onnx").to_string_lossy().into()),
        // SenseVoice 支持 zh / en / ja / ko / yue / auto
        language: Some(language.to_string()),
        // 逆文本正则化：输出标点，数字写成阿拉伯数字
        use_itn: true,
    };
    config.model_config.tokens = Some(dir.join("tokens.txt").to_string_lossy().into());
    config.model_config.num_threads =
        std::thread::available_parallelism().map_or(2, |n| n.get().min(4)) as i32;
    config.model_config.provider = Some("cpu".into());
    OfflineRecognizer::create(&config).ok_or_else(|| crate::i18n::tr("SenseVoice 模型加载失败", "Failed to load the SenseVoice model").to_string())
}

fn load_vad(cfg: &Config) -> Option<WhisperVadContext> {
    let path = models::vad_path(&cfg.model_dir());
    if !path.exists() {
        log::warn!("VAD model not found: {}", path.display());
        return None;
    }
    let mut params = WhisperVadContextParams::default();
    params.set_n_threads(4);
    params.set_use_gpu(false); // 模型只有 1 MB，CPU 足够快
    WhisperVadContext::new(&path.to_string_lossy(), params)
        .map_err(|e| log::warn!("VAD load failed: {e}"))
        .ok()
}

/// 只保留语音段（段间留 0.1 秒静音）；完全没有语音返回 None，VAD 出错时原样返回
fn speech_only(vad: &mut WhisperVadContext, audio: &[f32]) -> Option<Vec<f32>> {
    let segments = match vad.segments_from_samples(WhisperVadParams::default(), audio) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("VAD failed: {e}");
            return Some(audio.to_vec());
        }
    };
    let gap = vec![0.0f32; SAMPLE_RATE as usize / 10];
    let mut out = Vec::with_capacity(audio.len());
    for seg in segments {
        // 时间戳单位是 10 毫秒
        let to_sample = |cs: f32| ((cs * SAMPLE_RATE as f32 / 100.0) as usize).min(audio.len());
        let (start, end) = (to_sample(seg.start), to_sample(seg.end));
        if end > start {
            if !out.is_empty() {
                out.extend_from_slice(&gap);
            }
            out.extend_from_slice(&audio[start..end]);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// 预热：首次推理时 Vulkan 要编译着色器管线（新机器上可能要十几秒），加载完先跑一次。
/// 只为触发编译，用贪心解码、限制 token 数、关闭温度回退，避免在静音上反复解码
fn warm_up(ctx: &WhisperContext) {
    let Ok(mut state) = ctx.create_state() else { return };
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_max_tokens(4);
    params.set_temperature_inc(0.0);
    params.set_single_segment(true);
    params.set_print_progress(false);
    let _ = state.full(params, &vec![0.0; SAMPLE_RATE as usize * 2]);
}

fn decode(ctx: &WhisperContext, audio: &[f32], cfg: &Config) -> Result<String, String> {
    let mut state = ctx.create_state().map_err(|e| e.to_string())?;

    let mut params = FullParams::new(SamplingStrategy::BeamSearch { beam_size: 5, patience: -1.0 });
    params.set_language(Some(cfg.language.as_str()));
    params.set_n_threads(std::thread::available_parallelism().map_or(4, |n| n.get().min(8)) as i32);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);

    // === 抗幻觉 ===
    // 短音频上 whisper 会凭空生成训练数据里的字幕文本（"优优独播剧场"之类）：
    // 音频证据不足时解码器会退回语言模型先验硬凑。
    // 每次都是独立的一句话，不需要上文；带上文时幻觉会被带进后续片段并滚雪球
    params.set_no_context(true);
    // 平均对数概率低于此值的片段判为不可信（默认 -1.0，收紧到 -0.7）
    params.set_logprob_thold(-0.7);
    // 抑制非语音 token（音乐符号、[掌声] 之类）
    params.set_suppress_nst(true);

    state.full(params, audio).map_err(|e| crate::trf!("识别失败：{e}", "Transcription failed: {e}"))?;

    let parts: Vec<String> = state
        .as_iter()
        .filter_map(|seg| seg.to_str_lossy().ok().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect();
    // 中日韩文本片段之间不需要空格
    let sep = if matches!(cfg.language.as_str(), "zh" | "ja" | "ko" | "yue") { "" } else { " " };
    Ok(parts.join(sep))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 读取 16 kHz 单声道 16 位 PCM wav
    fn read_wav(path: &str) -> Vec<f32> {
        let bytes = std::fs::read(path).unwrap();
        let mut i = 12;
        while &bytes[i..i + 4] != b"data" {
            i += 8 + u32::from_le_bytes(bytes[i + 4..i + 8].try_into().unwrap()) as usize;
        }
        bytes[i + 8..]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
            .collect()
    }

    /// 手动运行：VT_MODEL_DIR=… VT_WAV=… [VT_MODEL=tiny] [VT_DEVICE=cpu]
    /// cargo test --lib transcribe_wav -- --ignored --nocapture
    #[test]
    #[ignore]
    fn transcribe_wav() {
        let cfg = Config {
            model: std::env::var("VT_MODEL").unwrap_or_else(|_| "tiny".into()),
            model_dir: std::env::var("VT_MODEL_DIR").unwrap(),
            device: std::env::var("VT_DEVICE").unwrap_or_else(|_| "gpu".into()),
            ..Config::default()
        };
        let t = Transcriber::default();
        let start = std::time::Instant::now();
        t.preload(&cfg).unwrap();
        println!("load: {:?}", start.elapsed());

        let audio = read_wav(&std::env::var("VT_WAV").unwrap());
        for _ in 0..3 {
            let start = std::time::Instant::now();
            println!("speech: {:?} ({:?})", t.transcribe(&audio, &cfg).unwrap(), start.elapsed());
        }

        let silence = vec![0.0f32; 16000 * 3];
        println!("silence: {:?}", t.transcribe(&silence, &cfg).unwrap());

        let padded: Vec<f32> = [vec![0.0; 16000 * 3], audio.clone(), vec![0.0; 16000 * 4]].concat();
        println!("silence+speech+silence: {:?}", t.transcribe(&padded, &cfg).unwrap());

        let short = &audio[..(16000 * 1).min(audio.len())];
        println!("1s clip: {:?}", t.transcribe(short, &cfg).unwrap());
    }
}
