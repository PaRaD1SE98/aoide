//! 录音期间静音其他程序的声音，结束后恢复
//!
//! 不动系统总音量，而是逐个处理 Windows 的音频会话（音量合成器里的每一项）：
//! 只静音其他程序当前正在出声、且原本没被静音的会话。本程序自己的提示音和
//! Windows 系统提示音不受影响，所以提示音何时播放都听得到。
//! 恢复时只取消我们静音的那些会话，用户原本就静音的保持原样。
//! 被静音的会话同时记在文件里：程序被强退来不及恢复时，下次启动按文件恢复。

use super::com::ComGuard;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use windows::core::Interface;
use windows::Win32::Foundation::S_OK;
use windows::Win32::Media::Audio::{
    eRender, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2,
    IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};

/// 被我们静音的会话（会话实例 ID），恢复时按 ID 找回
static MUTED: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// 当前是否应处于静音状态。快速开始又停止时，恢复可能抢在静音之前执行；
/// 静音在锁内检查这个标志，避免录音已结束却留下静音
static WANTED: AtomicBool = AtomicBool::new(false);

/// 录音开始：静音其他程序正在播放的声音
pub fn begin() {
    WANTED.store(true, Ordering::SeqCst);
    mute_others();
}

/// 录音结束：恢复（可重复调用）
pub fn end() {
    WANTED.store(false, Ordering::SeqCst);
    restore();
}

/// 启动时调用：上次运行留下了未恢复的静音（被强退、崩溃），现在恢复
pub fn recover() {
    let Some(ids) = std::fs::read_to_string(pending_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
    else {
        let _ = std::fs::remove_file(pending_file());
        return;
    };
    log::info!("Recovering {} audio session(s) left muted by the last run", ids.len());
    MUTED.lock().unwrap().extend(ids);
    restore();
}

fn pending_file() -> PathBuf {
    crate::config::data_dir().join("muted-sessions.json")
}

fn save_pending(ids: &[String]) {
    let _ = std::fs::create_dir_all(crate::config::data_dir());
    let json = serde_json::to_string(ids).expect("session ids are serializable");
    if let Err(e) = std::fs::write(pending_file(), json) {
        log::warn!("Save muted sessions failed: {e}");
    }
}

struct Session {
    id: String,
    active: bool,
    volume: ISimpleAudioVolume,
}

/// 所有输出设备上、除本程序和系统提示音以外的音频会话
fn other_sessions() -> windows::core::Result<Vec<Session>> {
    let own_pid = std::process::id();
    let mut out = Vec::new();
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let devices = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        for d in 0..devices.GetCount()? {
            let Ok(manager) = devices.Item(d)?.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                continue;
            };
            let sessions = manager.GetSessionEnumerator()?;
            for i in 0..sessions.GetCount()? {
                let Ok(control) = sessions.GetSession(i) else { continue };
                let Ok(control2) = control.cast::<IAudioSessionControl2>() else { continue };
                // IsSystemSoundsSession 对普通会话返回 S_FALSE，不能用 is_ok() 判断
                if control2.IsSystemSoundsSession() == S_OK || control2.GetProcessId().ok() == Some(own_pid) {
                    continue;
                }
                let Ok(raw_id) = control2.GetSessionInstanceIdentifier() else { continue };
                let id = raw_id.to_string().unwrap_or_default();
                CoTaskMemFree(Some(raw_id.0 as _));
                let Ok(volume) = control.cast::<ISimpleAudioVolume>() else { continue };
                let active = control.GetState().is_ok_and(|s| s == AudioSessionStateActive);
                out.push(Session { id, active, volume });
            }
        }
    }
    Ok(out)
}

fn mute_others() {
    let mut muted = MUTED.lock().unwrap();
    if !WANTED.load(Ordering::SeqCst) {
        return;
    }
    let _com = ComGuard::new();
    let sessions = match other_sessions() {
        Ok(s) => s,
        Err(e) => {
            log::warn!("Mute others failed: {e}");
            return;
        }
    };
    let targets: Vec<Session> = sessions
        .into_iter()
        .filter(|s| {
            let already = unsafe { s.volume.GetMute() }.is_ok_and(|m| m.as_bool());
            s.active && !already && !muted.contains(&s.id)
        })
        .collect();
    if targets.is_empty() {
        return;
    }
    // 先落盘再静音：静音之后、写文件之前被强退也不会漏掉。
    // 静音失败的会话多记了也无妨，恢复时对它取消静音不改变什么
    let mut pending = muted.clone();
    pending.extend(targets.iter().map(|s| s.id.clone()));
    save_pending(&pending);
    for s in targets {
        if unsafe { s.volume.SetMute(true, std::ptr::null()) }.is_ok() {
            muted.push(s.id);
        }
    }
    if !muted.is_empty() {
        log::info!("Muted {} audio session(s) while recording", muted.len());
    }
}

fn restore() {
    let mut muted = MUTED.lock().unwrap();
    if muted.is_empty() {
        // 记了文件但一个都没静音成功时，文件也要清掉
        let _ = std::fs::remove_file(pending_file());
        return;
    }
    let _com = ComGuard::new();
    match other_sessions() {
        Ok(sessions) => {
            for s in sessions.iter().filter(|s| muted.contains(&s.id)) {
                let _ = unsafe { s.volume.SetMute(false, std::ptr::null()) };
            }
            log::info!("Restored {} audio session(s)", muted.len());
            muted.clear();
            let _ = std::fs::remove_file(pending_file());
        }
        // 记录和文件都留着：下次结束录音或下次启动时重试
        Err(e) => log::warn!("Restore audio failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn muted_state_of(pid: u32) -> Option<bool> {
        let _com = ComGuard::new();
        unsafe {
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
            let devices = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE).ok()?;
            for d in 0..devices.GetCount().ok()? {
                let manager = devices.Item(d).ok()?.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None).ok()?;
                let sessions = manager.GetSessionEnumerator().ok()?;
                for i in 0..sessions.GetCount().ok()? {
                    let control = sessions.GetSession(i).ok()?;
                    let control2 = control.cast::<IAudioSessionControl2>().ok()?;
                    if control2.GetProcessId().ok() == Some(pid) {
                        return control.cast::<ISimpleAudioVolume>().ok()?.GetMute().ok().map(|b| b.as_bool());
                    }
                }
            }
        }
        None
    }

    /// 手动运行：cargo test --lib mute -- --ignored --nocapture
    /// 用 PowerShell 循环播放一段无声 wav 充当「其他程序」（会短暂静音正在播放的其他程序）
    #[test]
    #[ignore]
    fn mute_and_restore_other_process() {
        let wav = std::env::temp_dir().join("aoide-silence.wav");
        let samples = 16000 * 10;
        let mut bytes = Vec::new();
        for (tag, v) in [(b"RIFF", 36 + samples * 2), (b"WAVE", 0)] {
            bytes.extend_from_slice(tag);
            if tag == b"RIFF" {
                bytes.extend_from_slice(&(v as u32).to_le_bytes());
            }
        }
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        for v in [1u16, 1] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        bytes.extend_from_slice(&16000u32.to_le_bytes());
        bytes.extend_from_slice(&32000u32.to_le_bytes());
        for v in [2u16, 16] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&((samples * 2) as u32).to_le_bytes());
        bytes.resize(bytes.len() + samples * 2, 0);
        std::fs::write(&wav, bytes).unwrap();

        let mut child = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &format!("(New-Object Media.SoundPlayer '{}').PlaySync()", wav.display())])
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_secs(2));
        // PowerShell 可能经由子进程出声，按会话所属 PID 找不到时直接比较全局记录
        begin();
        let muted_count = MUTED.lock().unwrap().len();
        println!("muted sessions: {muted_count}, child state: {:?}", muted_state_of(child.id()));
        assert!(muted_count >= 1);
        assert_eq!(muted_state_of(child.id()), Some(true));

        assert!(pending_file().is_file());

        end();
        println!("after restore: {:?}", muted_state_of(child.id()));
        assert!(MUTED.lock().unwrap().is_empty());
        assert_eq!(muted_state_of(child.id()), Some(false));
        assert!(!pending_file().is_file());

        // 模拟强退：静音后内存里的记录丢失，下次启动按文件恢复
        begin();
        WANTED.store(false, Ordering::SeqCst);
        MUTED.lock().unwrap().clear();
        assert_eq!(muted_state_of(child.id()), Some(true));
        recover();
        println!("after recover: {:?}", muted_state_of(child.id()));
        assert_eq!(muted_state_of(child.id()), Some(false));
        assert!(!pending_file().is_file());
        let _ = child.kill();
    }
}
