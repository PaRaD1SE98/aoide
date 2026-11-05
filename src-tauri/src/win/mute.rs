//! 录音期间静音其他程序的声音，结束后恢复
//!
//! 不动系统总音量，而是逐个处理 Windows 的音频会话（音量合成器里的每一项）：
//! 只静音其他程序当前正在出声、且原本没被静音的会话。本程序自己的提示音和
//! Windows 系统提示音不受影响，所以开始时先静音再播开始音，结束时先播结束音、播完再恢复。
//! 恢复时音量从低到高渐强，原本音量很大时不会突然响起来。
//! 恢复时只取消我们静音的那些会话，用户原本就静音的保持原样。
//! 被静音的会话（和原来的音量）同时记在文件里：程序被强退来不及恢复时，下次启动按文件恢复。

use super::com::ComGuard;
use crate::sound::Cue;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use windows::core::Interface;
use windows::Win32::Foundation::S_OK;
use windows::Win32::Media::Audio::{
    eRender, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2,
    IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};

/// 提示音在静音期间也听得到（只静音其他程序）
pub const CUE_HEARD_WHILE_MUTED: bool = true;

/// 一个被我们静音的会话
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Muted {
    /// 会话实例 ID，恢复时按它找回
    id: String,
    /// 静音前的音量（0–1）：渐强恢复的目标，强退后也按它调回来
    #[serde(default)]
    volume: Option<f32>,
}

/// 被我们静音的会话，恢复（含渐强）完成后才清空
static MUTED: Mutex<Vec<Muted>> = Mutex::new(Vec::new());
/// 当前是否应处于静音状态。快速开始又停止时，恢复可能抢在静音之前执行；
/// 静音在锁内检查这个标志，避免录音已结束却留下静音
static WANTED: AtomicBool = AtomicBool::new(false);
/// 每次开始或结束加一。结束后播提示音、渐强恢复的线程，发现它变了（又开始了录音）就停下
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// 渐强时长与步数
const FADE: Duration = Duration::from_millis(600);
const FADE_STEPS: u32 = 20;

/// 录音开始：静音其他程序正在播放的声音
pub fn begin() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    WANTED.store(true, Ordering::SeqCst);
    mute_others();
}

/// 录音结束：先播结束音，播完再渐强恢复其他程序的声音。在后台进行，立即返回
pub fn end_with(cue: Cue) {
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    WANTED.store(false, Ordering::SeqCst);
    if MUTED.lock().unwrap().is_empty() {
        let _ = std::fs::remove_file(pending_file());
        crate::sound::play(cue);
        return;
    }
    std::thread::spawn(move || {
        crate::sound::play(cue);
        std::thread::sleep(crate::sound::length(cue));
        restore(&|| GENERATION.load(Ordering::SeqCst) == generation, FADE);
    });
}

/// 退出、更新前调用：立即恢复，不播提示音、不渐强
pub fn end() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    WANTED.store(false, Ordering::SeqCst);
    restore(&|| true, Duration::ZERO);
}

/// 启动时调用：上次运行留下了未恢复的静音（被强退、崩溃），现在恢复
pub fn recover() {
    let Ok(text) = std::fs::read_to_string(pending_file()) else { return };
    // 旧版本只记了会话 ID
    let entries = serde_json::from_str::<Vec<Muted>>(&text).ok().or_else(|| {
        serde_json::from_str::<Vec<String>>(&text)
            .ok()
            .map(|ids| ids.into_iter().map(|id| Muted { id, volume: None }).collect())
    });
    let Some(entries) = entries else {
        let _ = std::fs::remove_file(pending_file());
        return;
    };
    log::info!("Recovering {} audio session(s) left muted by the last run", entries.len());
    *MUTED.lock().unwrap() = entries;
    restore(&|| true, Duration::ZERO);
}

fn pending_file() -> PathBuf {
    crate::config::data_dir().join("muted-sessions.json")
}

fn save_pending(entries: &[Muted]) {
    let _ = std::fs::create_dir_all(crate::config::data_dir());
    let json = serde_json::to_string(entries).expect("muted sessions are serializable");
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

fn set_volume(s: &Session, volume: f32) {
    let _ = unsafe { s.volume.SetMasterVolume(volume, std::ptr::null()) };
}

fn set_mute(s: &Session, muted: bool) -> bool {
    unsafe { s.volume.SetMute(muted, std::ptr::null()) }.is_ok()
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
    let mut targets = Vec::new();
    for s in sessions {
        if let Some(entry) = muted.iter().find(|m| m.id == s.id) {
            // 上次结束后还没恢复完（结束音还在播、正在渐强）：音量调回原值，重新静音
            if let Some(v) = entry.volume {
                set_volume(&s, v);
            }
            set_mute(&s, true);
            continue;
        }
        let already = unsafe { s.volume.GetMute() }.is_ok_and(|m| m.as_bool());
        if s.active && !already {
            targets.push(s);
        }
    }
    if targets.is_empty() {
        return;
    }
    // 先落盘再静音：静音之后、写文件之前被强退也不会漏掉。
    // 静音失败的会话多记了也无妨，恢复时对它取消静音不改变什么
    let entries: Vec<Muted> = targets
        .iter()
        .map(|s| Muted { id: s.id.clone(), volume: unsafe { s.volume.GetMasterVolume() }.ok() })
        .collect();
    let mut pending = muted.clone();
    pending.extend(entries.iter().cloned());
    save_pending(&pending);
    for (s, entry) in targets.iter().zip(entries) {
        if set_mute(s, true) {
            muted.push(entry);
        }
    }
    if !muted.is_empty() {
        log::info!("Muted {} audio session(s) while recording", muted.len());
    }
}

/// 取消我们静音的会话，音量在 fade 内从低到高渐强。
/// 渐强途中又开始录音（current() 变成 false）：音量调回原值，静音交给 begin（这些会话还记着）
fn restore(current: &dyn Fn() -> bool, fade: Duration) {
    let _com = ComGuard::new();
    let steps = if fade.is_zero() { 0 } else { FADE_STEPS };
    let gain = |s: u32| (s as f32 / FADE_STEPS as f32).powi(2);
    let targets: Vec<(Session, Option<f32>)> = {
        let mut muted = MUTED.lock().unwrap();
        if !current() {
            return;
        }
        if muted.is_empty() {
            // 记了文件但一个都没静音成功时，文件也要清掉
            let _ = std::fs::remove_file(pending_file());
            return;
        }
        let sessions = match other_sessions() {
            Ok(s) => s,
            // 记录和文件都留着：下次结束录音或下次启动时重试
            Err(e) => {
                log::warn!("Restore audio failed: {e}");
                return;
            }
        };
        let targets: Vec<(Session, Option<f32>)> = sessions
            .into_iter()
            .filter_map(|s| {
                let volume = muted.iter().find(|m| m.id == s.id)?.volume;
                Some((s, volume))
            })
            .collect();
        for (s, volume) in &targets {
            // 渐强从很小的音量开始；不知道原来音量的（旧版本留下的记录）直接取消静音
            match volume {
                Some(v) if steps > 0 => set_volume(s, v * gain(1)),
                Some(v) => set_volume(s, *v),
                None => {}
            }
            set_mute(s, false);
        }
        if steps == 0 {
            log::info!("Restored {} audio session(s)", muted.len());
            muted.clear();
            let _ = std::fs::remove_file(pending_file());
            return;
        }
        targets
    };
    for step in 2..=steps {
        std::thread::sleep(fade / steps);
        let _muted = MUTED.lock().unwrap();
        if !current() {
            // 又开始了录音：音量调回原值、重新静音（这些会话还记着，begin 也会处理）。
            // 被下一次结束（如取消）打断时什么都不做，由它从小音量重新渐强
            if WANTED.load(Ordering::SeqCst) {
                for (s, volume) in &targets {
                    if let Some(v) = volume {
                        set_volume(s, *v);
                    }
                    set_mute(s, true);
                }
            }
            return;
        }
        for (s, volume) in &targets {
            if let Some(v) = volume {
                set_volume(s, v * gain(step));
            }
        }
    }
    let mut muted = MUTED.lock().unwrap();
    if !current() {
        return;
    }
    for (s, volume) in &targets {
        if let Some(v) = volume {
            set_volume(s, *v);
        }
    }
    log::info!("Restored {} audio session(s)", muted.len());
    muted.clear();
    let _ = std::fs::remove_file(pending_file());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_of(pid: u32) -> Option<(bool, f32)> {
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
                        let volume = control.cast::<ISimpleAudioVolume>().ok()?;
                        return Some((volume.GetMute().ok()?.as_bool(), volume.GetMasterVolume().ok()?));
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
        let original = session_of(child.id()).map(|s| s.1);
        println!("child before: {:?}", session_of(child.id()));

        begin();
        let muted_count = MUTED.lock().unwrap().len();
        println!("muted sessions: {muted_count}, child: {:?}", session_of(child.id()));
        assert!(muted_count >= 1);
        assert_eq!(session_of(child.id()).map(|s| s.0), Some(true));
        assert!(pending_file().is_file());

        // 结束：提示音播完后渐强恢复；渐强途中音量低于原值，结束后回到原值
        end_with(Cue::Stop);
        std::thread::sleep(Duration::from_millis(900));
        let mid = session_of(child.id());
        println!("fading: {mid:?}");
        std::thread::sleep(Duration::from_millis(1500));
        println!("after restore: {:?}", session_of(child.id()));
        assert!(MUTED.lock().unwrap().is_empty());
        assert_eq!(session_of(child.id()).map(|s| s.0), Some(false));
        assert_eq!(session_of(child.id()).map(|s| s.1), original);
        assert!(!pending_file().is_file());

        // 渐强途中又开始录音：音量回到原值并重新静音，之后照常恢复
        begin();
        end_with(Cue::Stop);
        std::thread::sleep(Duration::from_millis(700));
        begin();
        println!("restarted while fading: {:?}", session_of(child.id()));
        assert_eq!(session_of(child.id()), original.map(|v| (true, v)));
        end_with(Cue::Stop);
        std::thread::sleep(Duration::from_millis(2500));
        assert_eq!(session_of(child.id()), original.map(|v| (false, v)));

        // 模拟强退：静音后内存里的记录丢失，下次启动按文件恢复
        begin();
        WANTED.store(false, Ordering::SeqCst);
        MUTED.lock().unwrap().clear();
        assert_eq!(session_of(child.id()).map(|s| s.0), Some(true));
        recover();
        println!("after recover: {:?}", session_of(child.id()));
        assert_eq!(session_of(child.id()), original.map(|v| (false, v)));
        assert!(!pending_file().is_file());
        let _ = child.kill();
    }
}
