//! 录音期间静音其他程序的声音，结束后恢复（macOS）
//!
//! macOS 没有 Windows 音量合成器那样按程序静音的接口，这里静音的是默认输出设备：
//! 只在它原本没被静音时静音，恢复时只取消我们静音的设备，用户原本就静音的保持原样。
//! 提示音也走这个设备，所以开始音播完才静音；结束时立即恢复，和结束音一起出来（见 controller.rs 的 start_cue）。
//! 被静音的设备同时记在文件里：程序被强退来不及恢复时，下次启动按文件恢复。
//!
//! 蓝牙耳机的麦克风一打开，耳机就从音乐模式切到通话模式（输出的采样率随之降低），麦克风关掉后再切回来。
//! 切换要一两秒，两种模式各记各的静音状态：
//! - 录音期间一直盯着：静音被切换冲掉了就再静音，默认输出换了设备也一并静音；
//! - 结束时立即取消静音（这时还在通话模式），然后紧盯着：切回音乐模式时，那边记着的静音
//!   会重新生效，一出现就马上再取消，直到采样率回到原值后再稳一会儿。

use super::cf::{CFTypeRef, CF};
use crate::sound::Cue;
use serde::{Deserialize, Serialize};
use std::ffi::c_void;
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
/// 每次开始或结束加一。录音期间、恢复后盯着设备的线程，发现它变了就退出
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// 录音期间检查设备静音状态的间隔
const WATCH_EVERY: Duration = Duration::from_millis(250);
/// 一次录音里，同一设备的静音被冲掉后最多再静音几次：用户自己按了取消静音时不和他一直较劲
const MAX_REMUTES: usize = 4;
/// 恢复后检查的间隔：切回音乐模式时带回的静音要尽快取消
const RECHECK_EVERY: Duration = Duration::from_millis(25);
/// 恢复后至少盯多久（不是蓝牙、或者没切换过模式的设备只盯这么久）
const RECHECK_MIN: Duration = Duration::from_millis(500);
/// 蓝牙耳机采样率回到原值后再盯多久：切换收尾时才把那边记着的静音应用上
const RECHECK_AFTER_SWITCH: Duration = Duration::from_millis(800);
/// 最多盯多久
const RECHECK_MAX: Duration = Duration::from_secs(5);

type AudioObjectId = u32;

#[repr(C)]
struct Address {
    selector: u32,
    scope: u32,
    element: u32,
}

const fn code(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const SYSTEM_OBJECT: AudioObjectId = 1;
const DEFAULT_OUTPUT: u32 = code(b"dOut");
const DEVICES: u32 = code(b"dev#");
const DEVICE_UID: u32 = code(b"uid ");
const NAME: u32 = code(b"lnam");
const TRANSPORT: u32 = code(b"tran");
const SAMPLE_RATE: u32 = code(b"nsrt");
const MUTE: u32 = code(b"mute");
const SCOPE_GLOBAL: u32 = code(b"glob");
const SCOPE_OUTPUT: u32 = code(b"outp");
/// 蓝牙、低功耗蓝牙的连接方式
const BLUETOOTH: [u32; 2] = [code(b"blue"), code(b"blea")];
/// 主通道；没有主通道开关的设备逐个声道设置
const ELEMENTS: [u32; 3] = [0, 1, 2];

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyDataSize(id: AudioObjectId, addr: *const Address, qsize: u32, qdata: *const c_void, size: *mut u32) -> i32;
    fn AudioObjectGetPropertyData(id: AudioObjectId, addr: *const Address, qsize: u32, qdata: *const c_void, size: *mut u32, data: *mut c_void) -> i32;
    fn AudioObjectSetPropertyData(id: AudioObjectId, addr: *const Address, qsize: u32, qdata: *const c_void, size: u32, data: *const c_void) -> i32;
    fn AudioObjectHasProperty(id: AudioObjectId, addr: *const Address) -> u8;
    fn AudioObjectIsPropertySettable(id: AudioObjectId, addr: *const Address, settable: *mut u8) -> i32;
}

/// 读取定长的属性值
fn get<T: Default>(id: AudioObjectId, addr: &Address) -> Option<T> {
    let mut value = T::default();
    let mut size = std::mem::size_of::<T>() as u32;
    let status = unsafe { AudioObjectGetPropertyData(id, addr, 0, std::ptr::null(), &mut size, &mut value as *mut T as *mut c_void) };
    (status == 0).then_some(value)
}

/// 写入定长的属性值
fn set<T>(id: AudioObjectId, addr: &Address, value: &T) -> bool {
    let size = std::mem::size_of::<T>() as u32;
    unsafe { AudioObjectSetPropertyData(id, addr, 0, std::ptr::null(), size, value as *const T as *const c_void) == 0 }
}

fn default_output() -> Option<AudioObjectId> {
    let addr = Address { selector: DEFAULT_OUTPUT, scope: SCOPE_GLOBAL, element: 0 };
    get::<AudioObjectId>(SYSTEM_OBJECT, &addr).filter(|&id| id != 0)
}

fn all_devices() -> Vec<AudioObjectId> {
    let addr = Address { selector: DEVICES, scope: SCOPE_GLOBAL, element: 0 };
    let mut size = 0u32;
    unsafe {
        if AudioObjectGetPropertyDataSize(SYSTEM_OBJECT, &addr, 0, std::ptr::null(), &mut size) != 0 {
            return Vec::new();
        }
        let mut ids = vec![0 as AudioObjectId; size as usize / std::mem::size_of::<AudioObjectId>()];
        if AudioObjectGetPropertyData(SYSTEM_OBJECT, &addr, 0, std::ptr::null(), &mut size, ids.as_mut_ptr() as *mut c_void) != 0 {
            return Vec::new();
        }
        ids.truncate(size as usize / std::mem::size_of::<AudioObjectId>());
        ids
    }
}

/// 取字符串属性（UID、名称）
fn string_property(id: AudioObjectId, selector: u32) -> Option<String> {
    let addr = Address { selector, scope: SCOPE_GLOBAL, element: 0 };
    let s: CFTypeRef = get::<usize>(id, &addr)? as CFTypeRef;
    CF::owned(s)?.to_string()
}

fn device_uid(id: AudioObjectId) -> Option<String> {
    string_property(id, DEVICE_UID)
}

/// 按 UID 找当前连着的设备
fn find(uid: &str) -> Option<AudioObjectId> {
    all_devices().into_iter().find(|&id| device_uid(id).as_deref() == Some(uid))
}

fn transport(id: AudioObjectId) -> Option<u32> {
    get::<u32>(id, &Address { selector: TRANSPORT, scope: SCOPE_GLOBAL, element: 0 })
}

fn is_bluetooth(id: AudioObjectId) -> bool {
    transport(id).is_some_and(|t| BLUETOOTH.contains(&t))
}

fn sample_rate(id: AudioObjectId) -> f64 {
    get::<f64>(id, &Address { selector: SAMPLE_RATE, scope: SCOPE_GLOBAL, element: 0 }).unwrap_or(0.0)
}

/// 日志里的设备描述：名称、连接方式（blue = 蓝牙，bltn = 内置，usb = USB）和采样率
fn describe(id: AudioObjectId) -> String {
    let name = string_property(id, NAME).unwrap_or_else(|| format!("device {id}"));
    let transport = transport(id)
        .map(|t| String::from_utf8_lossy(&t.to_be_bytes()).trim().to_string())
        .unwrap_or_else(|| "?".into());
    format!("\"{name}\" ({transport}, {} Hz)", sample_rate(id))
}

/// 设备上可以设置某个属性的通道：有主通道就只用它
fn settable_elements(id: AudioObjectId, selector: u32) -> Vec<u32> {
    let settable = |element| {
        let addr = Address { selector, scope: SCOPE_OUTPUT, element };
        let mut ok = 0u8;
        unsafe { AudioObjectHasProperty(id, &addr) != 0 && AudioObjectIsPropertySettable(id, &addr, &mut ok) == 0 && ok != 0 }
    };
    if settable(ELEMENTS[0]) {
        return vec![ELEMENTS[0]];
    }
    ELEMENTS[1..].iter().copied().filter(|&e| settable(e)).collect()
}

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

/// 录音结束：立即恢复声音、播放结束音，然后在后台盯一会儿（蓝牙耳机切回音乐模式时会带回静音）
pub fn end_with(cue: Cue) {
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    WANTED.store(false, Ordering::SeqCst);
    let restored = restore();
    crate::sound::play(cue);
    if restored.is_empty() {
        return;
    }
    std::thread::spawn(move || keep_unmuted(&restored, generation));
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

/// 静音一个还没被我们静音的设备
fn mute_device(id: AudioObjectId, muted: &mut Vec<Muted>) {
    let Some(uid) = device_uid(id) else { return };
    if muted.iter().any(|m| m.uid == uid) {
        return;
    }
    let elements = mute_elements(id);
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

/// 恢复后紧盯一会儿：恢复过的设备又变成静音（蓝牙耳机切回音乐模式时带回了那边记着的静音），马上再取消。
/// 蓝牙耳机还没切回来（采样率不是静音时的值）就一直盯到它切回来后再稳一会儿，否则只盯 RECHECK_MIN
fn keep_unmuted(restored: &[Muted], generation: u64) {
    let start = Instant::now();
    let mut back_at: Option<Instant> = None;
    let mut switched = false;
    loop {
        std::thread::sleep(RECHECK_EVERY);
        if GENERATION.load(Ordering::SeqCst) != generation {
            return;
        }
        let mut switching = false;
        {
            let _guard = MUTED.lock().unwrap();
            if WANTED.load(Ordering::SeqCst) {
                return;
            }
            for entry in restored {
                let Some(id) = find(&entry.uid) else {
                    switching = true; // 切换途中设备可能暂时找不到
                    continue;
                };
                if entry.rate > 0.0 && is_bluetooth(id) && sample_rate(id) != entry.rate {
                    switching = true;
                }
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
        let elapsed = start.elapsed();
        if switching {
            switched = true;
            back_at = None;
        } else if back_at.is_none() {
            back_at = Some(Instant::now());
        }
        // 没切换过模式的只盯 RECHECK_MIN；切换过的盯到切回来后再过 RECHECK_AFTER_SWITCH
        let settled = match back_at {
            Some(t) if switched => t.elapsed() >= RECHECK_AFTER_SWITCH,
            Some(_) => elapsed >= RECHECK_MIN,
            None => false,
        };
        if settled && switched {
            log::info!("Bluetooth output switched back; stopped watching {} ms after restoring", elapsed.as_millis());
        }
        if settled || elapsed >= RECHECK_MAX {
            if switching {
                log::warn!("Bluetooth output had not switched back {} ms after restoring", elapsed.as_millis());
            }
            return;
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
