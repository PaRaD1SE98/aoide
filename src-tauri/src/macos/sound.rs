//! 播放提示音（macOS：NSSound）

use super::cf::{CFDataCreate, CF};
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 每段 WAV 数据（按地址区分）对应的 NSSound，第一次播放时创建，之后常驻
static SOUNDS: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

fn sound_for(wav: &'static [u8]) -> Option<*mut AnyObject> {
    let mut sounds = SOUNDS.lock().unwrap();
    let key = wav.as_ptr() as usize;
    if let Some(&(_, sound)) = sounds.iter().find(|s| s.0 == key) {
        return Some(sound as *mut AnyObject);
    }
    // CFData 与 NSData 可以互换使用
    let data = CF::owned(unsafe { CFDataCreate(std::ptr::null(), wav.as_ptr(), wav.len() as isize) })?;
    let sound = super::on_main(move || unsafe {
        let sound: *mut AnyObject = msg_send![class!(NSSound), alloc];
        let sound: *mut AnyObject = msg_send![sound, initWithData: data.as_ptr() as *mut AnyObject];
        sound as usize
    });
    if sound == 0 {
        log::warn!("NSSound could not load a cue");
        return None;
    }
    sounds.push((key, sound));
    Some(sound as *mut AnyObject)
}

/// 播放内存中的 WAV；sync 为 true 时播完才返回
pub fn play(wav: &'static [u8], sync: bool) {
    let Some(sound) = sound_for(wav) else { return };
    let ptr = sound as usize;
    // AppKit 对象在主线程上操作
    super::on_main(move || unsafe {
        let sound = ptr as *mut AnyObject;
        let _: bool = msg_send![sound, stop];
        let _: bool = msg_send![sound, play];
    });
    unsafe {
        if sync {
            let duration: f64 = msg_send![sound, duration];
            let deadline = Instant::now() + Duration::from_secs_f64(duration + 0.5);
            loop {
                let playing: bool = msg_send![sound, isPlaying];
                if !playing || Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}
