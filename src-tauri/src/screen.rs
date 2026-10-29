//! 屏幕上下文：开始录音时截取前台窗口，和转写结果一起发给 LLM（见 llm.rs），让它对照屏幕上的内容
//! 认出人名、专有名词，理解用户在说什么。
//!
//! 只截前台窗口自己的内容（各平台的 screen::capture_window），不截整个屏幕：盖在上面的其他窗口、
//! Aoide 自己的悬浮条和小窗都不会进入截图。截图在录音期间于后台完成，不增加等待。
//! 截图会发给用户配置的 LLM 服务，所以默认关闭（设置里的「屏幕上下文」）

use crate::config::Config;
use base64::Engine;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// 发给 LLM 的截图长边上限（像素）：再大只多花 token，文字已经够清楚
const MAX_SIDE: u32 = 1600;
const JPEG_QUALITY: u8 = 80;
/// 录完音后最多再等截图多久（目标程序卡住时不至于拖住输出）
const WAIT: Duration = Duration::from_millis(1500);

pub struct Shot {
    jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl Shot {
    /// 窗口的 RGB 像素（逐行，每像素 3 字节）：缩放到长边不超过 MAX_SIDE 后编码成 JPEG
    pub(crate) fn from_rgb(width: u32, height: u32, rgb: Vec<u8>) -> Option<Shot> {
        let img = image::RgbImage::from_raw(width, height, rgb)?;
        let scale = (MAX_SIDE as f32 / width.max(height) as f32).min(1.0);
        let img = if scale < 1.0 {
            let (w, h) = (((width as f32 * scale).round() as u32).max(1), ((height as f32 * scale).round() as u32).max(1));
            image::imageops::resize(&img, w, h, image::imageops::FilterType::Triangle)
        } else {
            img
        };
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY).encode_image(&img).ok()?;
        Some(Shot { jpeg, width: img.width(), height: img.height() })
    }

    pub fn data_url(&self) -> String {
        format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(&self.jpeg))
    }
}

/// 后台进行中的截图
pub struct Pending(Option<mpsc::Receiver<Option<Shot>>>);

/// 开启了屏幕上下文、配置了 LLM 时，在后台截取前台窗口
pub fn begin(cfg: &Config) -> Pending {
    if !cfg.screen_context || cfg.polish_api.trim().is_empty() {
        return Pending(None);
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let start = Instant::now();
        let shot = crate::platform::screen::capture_window();
        match &shot {
            Some(s) => log::info!("Screen context: {}x{}, {} KB, in {:?}", s.width, s.height, s.jpeg.len() / 1024, start.elapsed()),
            None => log::info!("Screen context: no screenshot of the foreground window"),
        }
        let _ = tx.send(shot);
    });
    Pending(Some(rx))
}

impl Pending {
    /// 截图结果；还没截完的话最多再等 WAIT
    pub fn take(self) -> Option<Shot> {
        self.0?.recv_timeout(WAIT).ok().flatten()
    }
}
