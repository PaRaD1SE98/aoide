//! 播放提示音（Windows：PlaySound）。走本进程自己的音频会话，录音期间静音其他程序时不受影响

use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT, SND_SYNC};

/// 播放内存中的 WAV；sync 为 true 时播完才返回。数据在异步播放期间必须一直有效
pub fn play(wav: &'static [u8], sync: bool) {
    let flags = SND_MEMORY | SND_NODEFAULT | if sync { SND_SYNC } else { SND_ASYNC };
    unsafe {
        // SND_MEMORY：第一个参数实际是指向 WAV 数据的指针
        let _ = PlaySoundW(PCWSTR(wav.as_ptr() as *const u16), None, flags);
    }
}
