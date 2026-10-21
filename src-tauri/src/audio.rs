//! 录音、提示音、重采样、静音补齐

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::Duration;

/// whisper 要求 16 kHz 单声道
pub const SAMPLE_RATE: u32 = 16_000;

/// 最近一段录音的音量（RMS，f32 位模式），供悬浮条画波形
static INPUT_LEVEL: AtomicU32 = AtomicU32::new(0);

pub fn input_level() -> f32 {
    f32::from_bits(INPUT_LEVEL.load(Ordering::Relaxed))
}

#[derive(Default)]
struct Shared {
    /// 设备原始采样率下的单声道样本
    samples: Mutex<Vec<f32>>,
    rate: AtomicU32,
    stopped: AtomicBool,
    done: Mutex<bool>,
    done_cv: Condvar,
}

/// 一次录音会话：start() 后台采集默认麦克风，直到 stop()
///
/// 每次录音新建一个实例，转录线程持有自己那次的实例，
/// 快速连按时新录音不会覆盖还没处理完的上一段音频。
pub struct Recorder {
    shared: Arc<Shared>,
    stop_tx: Mutex<Option<mpsc::Sender<()>>>,
}

impl Recorder {
    pub fn start() -> Arc<Self> {
        let shared = Arc::new(Shared::default());
        let (tx, rx) = mpsc::channel::<()>();
        let s = shared.clone();
        std::thread::spawn(move || {
            // cpal 的 Stream 不能跨线程，建流、等停止信号、销毁都在这个线程里
            if let Err(e) = run_stream(&s, rx) {
                log::error!("Recording failed: {e}");
            }
            *s.done.lock().unwrap() = true;
            s.done_cv.notify_all();
        });
        Arc::new(Self { shared, stop_tx: Mutex::new(Some(tx)) })
    }

    pub fn stop(&self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        INPUT_LEVEL.store(0, Ordering::Relaxed);
        self.stop_tx.lock().unwrap().take(); // 丢弃发送端，录音线程的 recv() 随即返回
    }

    /// 等待录音线程结束；返回是否已结束
    pub fn wait(&self, timeout: Duration) -> bool {
        let guard = self.shared.done.lock().unwrap();
        let (guard, _) = self
            .shared
            .done_cv
            .wait_timeout_while(guard, timeout, |done| !*done)
            .unwrap();
        *guard
    }

    /// 录到的音频（16 kHz 单声道）；什么都没录到返回 None
    pub fn audio(&self) -> Option<Vec<f32>> {
        let samples = self.shared.samples.lock().unwrap();
        if samples.is_empty() {
            return None;
        }
        Some(resample(&samples, self.shared.rate.load(Ordering::SeqCst)))
    }
}

fn run_stream(s: &Arc<Shared>, rx: mpsc::Receiver<()>) -> Result<(), String> {
    let device = cpal::default_host()
        .default_input_device()
        .ok_or(crate::i18n::tr("没有找到麦克风", "No microphone found", "マイクが見つかりません"))?;
    let supported = device.default_input_config().map_err(|e| e.to_string())?;
    let channels = supported.channels() as usize;
    s.rate.store(supported.sample_rate().0, Ordering::SeqCst);
    let config = supported.config();

    let stream = match supported.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, &config, channels, s.clone()),
        SampleFormat::I16 => build::<i16>(&device, &config, channels, s.clone()),
        SampleFormat::U16 => build::<u16>(&device, &config, channels, s.clone()),
        SampleFormat::I32 => build::<i32>(&device, &config, channels, s.clone()),
        other => return Err(crate::trf!("不支持的采样格式 {other:?}", "Unsupported sample format {other:?}", "サポートされていないサンプル形式です: {other:?}")),
    }?;
    stream.play().map_err(|e| e.to_string())?;
    let _ = rx.recv(); // 阻塞直到 stop()
    drop(stream);
    Ok(())
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    s: Arc<Shared>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                if s.stopped.load(Ordering::Relaxed) {
                    return;
                }
                let mut buf = s.samples.lock().unwrap();
                let start = buf.len();
                for frame in data.chunks(channels) {
                    let sum: f32 = frame.iter().map(|&x| x.to_sample::<f32>()).sum();
                    buf.push(sum / channels as f32);
                }
                let chunk = &buf[start..];
                if !chunk.is_empty() {
                    let rms = (chunk.iter().map(|x| x * x).sum::<f32>() / chunk.len() as f32).sqrt();
                    INPUT_LEVEL.store(rms.to_bits(), Ordering::Relaxed);
                }
            },
            |e| log::error!("Audio stream error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())
}

/// 重采样到 16 kHz：降采样用区间平均（兼作抗混叠低通），升采样用线性插值
fn resample(input: &[f32], from: u32) -> Vec<f32> {
    if from == SAMPLE_RATE || from == 0 {
        return input.to_vec();
    }
    let ratio = from as f64 / SAMPLE_RATE as f64;
    let out_len = (input.len() as f64 / ratio) as usize;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * ratio;
            if ratio > 1.0 {
                let start = pos as usize;
                let end = (((i + 1) as f64 * ratio) as usize).clamp(start + 1, input.len());
                input[start..end].iter().sum::<f32>() / (end - start) as f32
            } else {
                let j = pos as usize;
                let frac = (pos - j as f64) as f32;
                let next = input.get(j + 1).copied().unwrap_or(input[j]);
                input[j] * (1.0 - frac) + next * frac
            }
        })
        .collect()
}

/// 过短的录音用静音补齐到最短时长再送去转录
pub fn pad_to_min(mut audio: Vec<f32>, min_secs: u32, tag: &str) -> Vec<f32> {
    let need = (min_secs * SAMPLE_RATE) as usize;
    if audio.len() < need {
        log::info!(
            "{tag}: padded {:.1}s -> {min_secs}s with silence",
            audio.len() as f32 / SAMPLE_RATE as f32
        );
        audio.resize(need, 0.0);
    }
    audio
}
