//! 语音识别模型列表与下载：whisper.cpp（ggml 格式）和 SenseVoice（sherpa-onnx）

use serde::Serialize;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    /// 阿里 SenseVoice（sherpa-onnx 运行，CPU）：非自回归，中文更准、极快、不易幻觉
    SenseVoice,
    /// whisper.cpp（GPU 走 Vulkan）
    Whisper,
}

pub struct ModelInfo {
    pub id: &'static str,
    pub engine: Engine,
    pub size_mb: u32,
    /// [中文, English]
    pub note: [&'static str; 2],
    /// SenseVoice 模型所在的 Hugging Face 仓库
    repo: &'static str,
}

pub const MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "sense-voice",
        engine: Engine::SenseVoice,
        size_mb: 239,
        note: ["推荐：中文更准、速度极快、不易幻觉（中英日韩粤）", "Recommended: accurate for Chinese, very fast, rarely hallucinates (zh/en/ja/ko/yue)"],
        repo: "csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17",
    },
    ModelInfo {
        id: "sense-voice-yue",
        engine: Engine::SenseVoice,
        size_mb: 237,
        note: ["粤语增强版", "Cantonese-tuned"],
        repo: "csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09",
    },
    ModelInfo { id: "tiny", engine: Engine::Whisper, size_mb: 78, note: ["最快，精度最低", "Fastest, least accurate"], repo: "" },
    ModelInfo { id: "base", engine: Engine::Whisper, size_mb: 148, note: ["", ""], repo: "" },
    ModelInfo { id: "small", engine: Engine::Whisper, size_mb: 488, note: ["", ""], repo: "" },
    ModelInfo { id: "medium", engine: Engine::Whisper, size_mb: 1530, note: ["", ""], repo: "" },
    ModelInfo { id: "large-v3-turbo-q5_0", engine: Engine::Whisper, size_mb: 574, note: ["量化版，体积小、速度快", "Quantized: smaller and faster"], repo: "" },
    ModelInfo { id: "large-v3-turbo", engine: Engine::Whisper, size_mb: 1620, note: ["又快又准", "Fast and accurate"], repo: "" },
    ModelInfo { id: "large-v2", engine: Engine::Whisper, size_mb: 3090, note: ["短音频更稳", "More stable on short clips"], repo: "" },
    ModelInfo { id: "large-v3", engine: Engine::Whisper, size_mb: 3100, note: ["最准，但短音频较易幻觉", "Most accurate, but hallucinates more on short clips"], repo: "" },
];

/// Silero VAD：识别前先切掉静音段，抗幻觉的第一道防线
const VAD_FILE: &str = "ggml-silero-v5.1.2.bin";
const SENSE_VOICE_FILES: [&str; 2] = ["tokens.txt", "model.int8.onnx"];

pub fn find(id: &str) -> Option<&'static ModelInfo> {
    MODELS.iter().find(|m| m.id == id)
}

pub fn engine_of(id: &str) -> Engine {
    find(id).map_or(Engine::Whisper, |m| m.engine)
}

/// whisper：ggml 模型文件；SenseVoice：模型目录
pub fn model_path(dir: &Path, id: &str) -> PathBuf {
    match engine_of(id) {
        Engine::Whisper => dir.join(format!("ggml-{id}.bin")),
        Engine::SenseVoice => dir.join(format!("sherpa-onnx-{id}")),
    }
}

pub fn is_downloaded(dir: &Path, id: &str) -> bool {
    let path = model_path(dir, id);
    match engine_of(id) {
        Engine::Whisper => path.is_file(),
        Engine::SenseVoice => SENSE_VOICE_FILES.iter().all(|f| path.join(f).is_file()),
    }
}

pub fn vad_path(dir: &Path) -> PathBuf {
    dir.join(VAD_FILE)
}

/// 可用 HF_ENDPOINT 环境变量指定镜像站
fn hf_base() -> String {
    std::env::var("HF_ENDPOINT")
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|_| "https://huggingface.co".into())
}

/// 下载模型（连同 VAD 模型）；progress(已下载字节, 总字节) 只报告主模型文件
pub fn download(
    id: &str,
    dir: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<(), String> {
    let info = find(id).ok_or_else(|| crate::trf!("未知模型：{id}", "Unknown model: {id}"))?;
    std::fs::create_dir_all(dir).map_err(|e| crate::trf!("无法创建模型目录：{e}", "Cannot create the model folder: {e}"))?;

    let base = hf_base();
    let vad = vad_path(dir);
    if !vad.exists() {
        let url = format!("{base}/ggml-org/whisper-vad/resolve/main/{VAD_FILE}");
        download_file(&url, &vad, cancel, |_, _| {})?;
    }
    match info.engine {
        Engine::Whisper => {
            let url = format!("{base}/ggerganov/whisper.cpp/resolve/main/ggml-{id}.bin");
            download_file(&url, &model_path(dir, id), cancel, &mut progress)
        }
        Engine::SenseVoice => {
            let target = model_path(dir, id);
            std::fs::create_dir_all(&target).map_err(|e| crate::trf!("无法创建模型目录：{e}", "Cannot create the model folder: {e}"))?;
            for (i, file) in SENSE_VOICE_FILES.iter().enumerate() {
                let url = format!("{base}/{}/resolve/main/{file}", info.repo);
                let is_main = i == SENSE_VOICE_FILES.len() - 1;
                download_file(&url, &target.join(file), cancel, |d, t| {
                    if is_main {
                        progress(d, t)
                    }
                })?;
            }
            Ok(())
        }
    }
}

fn download_file(
    url: &str,
    dest: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<(), String> {
    log::info!("Downloading {url}");
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(None)
        .build()
        .map_err(|e| e.to_string())?;
    let mut resp = client
        .get(url)
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| crate::trf!("下载失败：{e}", "Download failed: {e}"))?;
    let total = resp.content_length().unwrap_or(0);

    let part = dest.with_extension("part");
    let mut file = File::create(&part).map_err(|e| crate::trf!("无法写入文件：{e}", "Cannot write file: {e}"))?;
    let mut buf = vec![0u8; 1 << 16];
    let mut done = 0u64;
    let mut last_report = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(file);
            let _ = std::fs::remove_file(&part);
            return Err(crate::i18n::tr("已取消", "Canceled").into());
        }
        let n = resp.read(&mut buf).map_err(|e| crate::trf!("下载中断：{e}", "Download interrupted: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| crate::trf!("无法写入文件：{e}", "Cannot write file: {e}"))?;
        done += n as u64;
        if done - last_report >= 1 << 20 {
            last_report = done;
            progress(done, total);
        }
    }
    file.flush().map_err(|e| e.to_string())?;
    drop(file);
    std::fs::rename(&part, dest).map_err(|e| crate::trf!("无法保存文件：{e}", "Cannot save file: {e}"))?;
    progress(done, total.max(done));
    log::info!("Downloaded {}", dest.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手动运行：cargo test --lib download_models -- --ignored --nocapture
    #[test]
    #[ignore]
    fn download_models() {
        let dir = std::env::temp_dir().join("aoide-dl-test");
        let _ = std::fs::remove_dir_all(&dir);
        let mut last = (0, 0);
        download("tiny", &dir, &AtomicBool::new(false), |d, t| last = (d, t)).unwrap();
        println!("progress: {last:?}");
        assert!(model_path(&dir, "tiny").metadata().unwrap().len() > 70_000_000);
        assert!(vad_path(&dir).exists());

        download("sense-voice", &dir, &AtomicBool::new(false), |d, t| last = (d, t)).unwrap();
        println!("progress: {last:?}");
        assert!(is_downloaded(&dir, "sense-voice"));

        // 取消：立即中止并清理 .part 文件
        let cancel = AtomicBool::new(true);
        let err = download("base", &dir, &cancel, |_, _| {}).unwrap_err();
        assert_eq!(err, "已取消");
        assert!(!model_path(&dir, "base").exists());
        assert!(!model_path(&dir, "base").with_extension("part").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
