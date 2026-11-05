//! 录音期间静音其他程序的声音，结束后恢复（macOS）
//!
//! macOS 没有 Windows 音量合成器那样按程序静音的接口，这里静音的是默认输出设备：
//! 只在它原本没被静音时静音，恢复时只取消我们静音的设备，用户原本就静音的保持原样。
//! 提示音也走这个设备，所以开始音播完才静音；结束时恢复声音，和结束音一起出来（见 controller.rs 的 start_cue）。
//! 被静音的设备同时记在文件里：程序被强退来不及恢复时，下次启动按文件恢复。
//!
//! 蓝牙耳机的麦克风一打开，耳机就从音乐模式切到通话模式（输出的采样率随之降低），麦克风关掉后要一两秒才切回来。
//! 通话模式有自己的音量（往往比音乐模式响），两种模式各记各的静音状态，切回来时还会断一下音。
//! 所以默认改用 Mac 内置麦克风录音，耳机一直留在音乐模式（见 coreaudio.rs 的 builtin_mic_instead_of_bluetooth），
//! 静音、恢复都是立即的。没有内置麦克风、只能用耳机麦克风时：
//! - 录音期间一直盯着：静音被切换冲掉了就再静音，默认输出换了设备也一并静音；
//! - 结束时耳机还在通话模式，先保持静音，等它切回音乐模式（采样率回到静音时的值）再恢复、播结束音。
//!   在通话模式下就恢复的话，其他声音会先按通话音量突然响起来，接着切换断音，然后才正常

use super::coreaudio::{
    default_output, describe, device_uid, find, get, is_bluetooth, sample_rate, set, settable_elements, Address,
    AudioObjectId, MUTE, SCOPE_OUTPUT,
};
use crate::sound::Cue;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 提示音在静音期间听不到（静音的是整个输出设备）
pub const CUE_HEARD_WHILE_MUTED: bool = false;

/// 一个被我们静音的设备
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Muted {
    /// 设备 UID：重新插拔后也不变，恢复时按它找回设备
    uid: String,
    /// 静音时（麦克风打开前）的采样率；蓝牙耳机切回音乐模式后会回到这个值
    #[serde(default)]
    rate: f64,
}

/// 被我们静音的设备
static MUTED: Mutex<Vec<Muted>> = Mutex::new(Vec::new());
/// 当前是否应处于静音状态。快速开始又停止时，恢复可能抢在静音之前执行；
/// 静音在锁内检查这个标志，避免录音已结束却留下静音
static WANTED: AtomicBool = AtomicBool::new(false);
/// 每次开始或结束加一。录音期间盯着设备、结束后等耳机切回来的线程，发现它变了就退出
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// 录音期间检查设备静音状态的间隔
const WATCH_EVERY: Duration = Duration::from_millis(250);
/// 一次录音里，同一设备的静音被冲掉后最多再静音几次：用户自己按了取消静音时不和他一直较劲
const MAX_REMUTES: usize = 4;
/// 等蓝牙耳机切回音乐模式：检查间隔、最多等多久
const SETTLE_EVERY: Duration = Duration::from_millis(25);
const SETTLE_MAX: Duration = Duration::from_secs(4);
/// 采样率回到原值后再等一会儿：切换收尾时才把音乐模式记着的静音状态应用上，在那之前恢复会被盖掉
const SETTLE_GRACE: Duration = Duration::from_millis(300);
/// 恢复后再盯多久：还是被切换重新静音了的，马上再取消
const RECHECK_FOR: Duration = Duration::from_millis(1500);

fn mute_elements(id: AudioObjectId) -> Vec<u32> {
    settable_elements(id, MUTE)
}

fn is_muted(id: AudioObjectId, element: u32) -> bool {
    get::<u32>(id, &Address { selector: MUTE, scope: SCOPE_OUTPUT, element }).is_some_and(|m| m != 0)
}

/// 设备的所有静音通道都设为 muted；返回是否至少设成功一个
fn set_device(id: AudioObjectId, elements: &[u32], muted: bool) -> bool {
    let value = u32::from(muted);
    elements.iter().fold(false, |any, &e| set(id, &Address { selector: MUTE, scope: SCOPE_OUTPUT, element: e }, &value) || any)
}

fn all_muted(id: AudioObjectId, elements: &[u32]) -> bool {
    !elements.is_empty() && elements.iter().all(|&e| is_muted(id, e))
}

fn any_muted(id: AudioObjectId, elements: &[u32]) -> bool {
    elements.iter().any(|&e| is_muted(id, e))
}

/// 蓝牙耳机还在通话模式（采样率不是静音时的值；切换途中设备可能暂时找不到）
fn switching(entry: &Muted) -> bool {
    match find(&entry.uid) {
        Some(id) => entry.rate > 0.0 && is_bluetooth(id) && sample_rate(id) != entry.rate,
        None => entry.rate > 0.0,
    }
}

/// 录音开始：静音默认输出设备，并在录音期间盯着它
pub fn begin() {
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    WANTED.store(true, Ordering::SeqCst);
    mute_output();
    std::thread::spawn(move || {
        let mut remutes = 0;
        loop {
            std::thread::sleep(WATCH_EVERY);
            if GENERATION.load(Ordering::SeqCst) != generation {
                return;
            }
            if remutes < MAX_REMUTES && keep_muted() {
                remutes += 1;
            }
        }
    });
}

/// 录音结束：恢复声音并播放结束音。一般立即进行；蓝牙耳机还在通话模式时，等它切回音乐模式再一起恢复。
/// 在后台进行，立即返回
pub fn end_with(cue: Cue) {
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    WANTED.store(false, Ordering::SeqCst);
    let waiting = MUTED.lock().unwrap().iter().any(switching);
    if !waiting {
        let restored = restore();
        crate::sound::play(cue);
        if !restored.is_empty() {
            std::thread::spawn(move || keep_unmuted(&restored, generation));
        }
        return;
    }
    let current = move || GENERATION.load(Ordering::SeqCst) == generation;
    std::thread::spawn(move || {
        settle(&current);
        if !current() {
            return; // 期间又开始了录音：开始音已经播过，结束音不再播
        }
        let restored = restore();
        crate::sound::play(cue);
        keep_unmuted(&restored, generation);
    });
}

/// 退出、更新前调用：立即恢复
pub fn end() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    WANTED.store(false, Ordering::SeqCst);
    restore();
}

/// 启动时调用：上次运行留下了未恢复的静音（被强退、崩溃），现在恢复
pub fn recover() {
    let Ok(text) = std::fs::read_to_string(pending_file()) else { return };
    // 旧版本只记了设备 UID
    let entries = serde_json::from_str::<Vec<Muted>>(&text).ok().or_else(|| {
        serde_json::from_str::<Vec<String>>(&text)
            .ok()
            .map(|uids| uids.into_iter().map(|uid| Muted { uid, rate: 0.0 }).collect())
    });
    let Some(entries) = entries else {
        let _ = std::fs::remove_file(pending_file());
        return;
    };
    log::info!("Recovering {} audio device(s) left muted by the last run", entries.len());
    *MUTED.lock().unwrap() = entries;
    restore();
}

fn pending_file() -> PathBuf {
    crate::config::data_dir().join("muted-devices.json")
}

fn save_pending(entries: &[Muted]) {
    let _ = std::fs::create_dir_all(crate::config::data_dir());
    let json = serde_json::to_string(entries).expect("muted devices are serializable");
    if let Err(e) = std::fs::write(pending_file(), json) {
        log::warn!("Save muted devices failed: {e}");
    }
}

fn mute_output() {
    let mut muted = MUTED.lock().unwrap();
    if !WANTED.load(Ordering::SeqCst) {
        return;
    }
    let Some(id) = default_output() else {
        log::warn!("Mute: no default output device");
        return;
    };
    mute_device(id, &mut muted);
}

/// 静音一个设备。已经记着的（上次结束后还在等耳机切回来）保持静音
fn mute_device(id: AudioObjectId, muted: &mut Vec<Muted>) {
    let Some(uid) = device_uid(id) else { return };
    let elements = mute_elements(id);
    if muted.iter().any(|m| m.uid == uid) {
        set_device(id, &elements, true);
        return;
    }
    if elements.is_empty() {
        log::info!("Mute: output {} has no mute control", describe(id));
        return;
    }
    if all_muted(id, &elements) {
        log::info!("Mute: output {} is already muted", describe(id));
        return;
    }
    // 先落盘再静音：静音之后、写文件之前被强退也不会漏掉
    let entry = Muted { uid, rate: sample_rate(id) };
    let mut pending = muted.clone();
    pending.push(entry.clone());
    save_pending(&pending);
    if set_device(id, &elements, true) {
        log::info!("Muted output {} while recording (reads back {})", describe(id), state(id, &elements));
        muted.push(entry);
    } else {
        log::warn!("Mute: setting mute on output {} failed", describe(id));
    }
}

/// 录音期间每隔一会儿调用：静音被冲掉的设备再静音，默认输出换成的新设备也静音。返回是否重新静音了
fn keep_muted() -> bool {
    let mut muted = MUTED.lock().unwrap();
    if !WANTED.load(Ordering::SeqCst) {
        return false;
    }
    let mut remuted = false;
    for entry in muted.iter() {
        let Some(id) = find(&entry.uid) else { continue };
        let elements = mute_elements(id);
        if !elements.is_empty() && !all_muted(id, &elements) && set_device(id, &elements, true) {
            log::info!("Output {} was unmuted while recording (Bluetooth mode switch?), muted it again", describe(id));
            remuted = true;
        }
    }
    if let Some(id) = default_output() {
        if device_uid(id).is_some_and(|uid| !muted.iter().any(|m| m.uid == uid)) {
            log::info!("Default output changed to {} while recording", describe(id));
            mute_device(id, &mut muted);
        }
    }
    remuted
}

/// 等蓝牙耳机切回音乐模式：采样率回到静音时的值，再稍等一会儿
fn settle(current: &dyn Fn() -> bool) {
    let entries = MUTED.lock().unwrap().clone();
    let start = Instant::now();
    loop {
        if !current() {
            return;
        }
        if !entries.iter().any(switching) {
            std::thread::sleep(SETTLE_GRACE);
            log::info!("Bluetooth output switched back to music mode after {} ms", start.elapsed().as_millis());
            return;
        }
        if start.elapsed() >= SETTLE_MAX {
            log::warn!("Bluetooth output did not switch back in {} ms, restoring anyway", start.elapsed().as_millis());
            return;
        }
        std::thread::sleep(SETTLE_EVERY);
    }
}

/// 取消我们静音的设备；返回它们（恢复后还要再盯一会儿）
fn restore() -> Vec<Muted> {
    let mut muted = MUTED.lock().unwrap();
    if muted.is_empty() {
        // 记了文件但没静音成功时，文件也要清掉
        let _ = std::fs::remove_file(pending_file());
        return Vec::new();
    }
    for entry in muted.iter() {
        let Some(id) = find(&entry.uid) else { continue };
        let elements = mute_elements(id);
        if !set_device(id, &elements, false) {
            log::warn!("Restore: unmuting output {} failed", describe(id));
        }
        log::info!("Restored output {} (reads back {})", describe(id), state(id, &elements));
    }
    // 找不到的设备（已拔掉）也不再记着：重新接上时它的静音状态由系统保存，用户能看到
    let _ = std::fs::remove_file(pending_file());
    std::mem::take(&mut *muted)
}

/// 恢复后盯一会儿：恢复过的设备又变成静音（蓝牙耳机切换收尾时带回了静音），马上再取消
fn keep_unmuted(restored: &[Muted], generation: u64) {
    let start = Instant::now();
    while start.elapsed() < RECHECK_FOR {
        std::thread::sleep(SETTLE_EVERY);
        if GENERATION.load(Ordering::SeqCst) != generation {
            return;
        }
        let _guard = MUTED.lock().unwrap();
        if WANTED.load(Ordering::SeqCst) {
            return;
        }
        for entry in restored {
            let Some(id) = find(&entry.uid) else { continue };
            let elements = mute_elements(id);
            if any_muted(id, &elements) {
                let ok = set_device(id, &elements, false);
                log::info!(
                    "Output {} was muted again {} ms after restoring, unmuted it {}",
                    describe(id),
                    start.elapsed().as_millis(),
                    if ok { "again" } else { "- failed" }
                );
            }
        }
    }
}

fn state(id: AudioObjectId, elements: &[u32]) -> &'static str {
    if all_muted(id, elements) {
        "muted"
    } else if any_muted(id, elements) {
        "partly muted"
    } else {
        "unmuted"
    }
}
