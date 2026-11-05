//! 录音期间静音其他程序的声音，结束后恢复（macOS）
//!
//! macOS 没有 Windows 音量合成器那样按程序静音的接口，这里静音的是默认输出设备：
//! 只在它原本没被静音时静音，恢复时只取消我们静音的设备，用户原本就静音的保持原样。
//! 提示音也走这个设备，所以开始音播完后才静音，结束时先恢复再播结束音（见 controller.rs 的 start_cue）。
//! 恢复时音量从低到高渐强，原本音量很大时不会突然响起来。
//! 被静音的设备（和渐强时的目标音量）同时记在文件里：程序被强退来不及恢复时，下次启动按文件恢复。
//!
//! 蓝牙耳机的麦克风一打开，耳机就从音乐模式切到通话模式（输出的采样率随之降低），麦克风关掉后再切回来。
//! 切换要一两秒，两种模式各记各的静音状态：
//! - 录音期间一直盯着：静音被切换冲掉了就再静音，默认输出换了设备也一并静音；
//! - 结束时等耳机切回音乐模式（采样率回到静音时的值）再恢复。在通话模式下就取消静音的话，
//!   声音会先回来一下，切回音乐模式后又被那边记着的静音盖住；
//! - 恢复后再盯几秒：还是被切换重新静音了的，再取消一次。

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
    /// 渐强恢复时的目标音量（通道, 音量）；只在渐强期间记着，强退后按它把音量调回来
    #[serde(default)]
    volume: Vec<(u32, f32)>,
}

/// 被我们静音的设备，恢复（含渐强）完成后才清空
static MUTED: Mutex<Vec<Muted>> = Mutex::new(Vec::new());
/// 当前是否应处于静音状态。快速开始又停止时，恢复可能抢在静音之前执行；
/// 静音在锁内检查这个标志，避免录音已结束却留下静音
static WANTED: AtomicBool = AtomicBool::new(false);
/// 每次开始或结束加一。录音期间盯着设备、结束后恢复的线程，发现它变了就退出
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// 录音期间检查设备静音状态的间隔
const WATCH_EVERY: Duration = Duration::from_millis(250);
/// 一次录音里，同一设备的静音被冲掉后最多再静音几次：用户自己按了取消静音时不和他一直较劲
const MAX_REMUTES: usize = 4;
/// 等蓝牙耳机切回音乐模式最多等多久
const SETTLE_MAX: Duration = Duration::from_secs(4);
/// 采样率回到原值后再等一会儿：切换收尾时还会把那边记着的静音状态应用上
const SETTLE_GRACE: Duration = Duration::from_millis(300);
/// 渐强时长与步数
const FADE: Duration = Duration::from_millis(400);
const FADE_STEPS: u32 = 12;
/// 恢复后隔多久再检查一次（累计约 3 秒）
const RECHECK_AFTER: [u64; 4] = [300, 500, 900, 1300];

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
const VOLUME: u32 = code(b"volm");
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

/// 设备上可以设置某个属性（静音、音量）的通道：有主通道就只用它
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

/// 当前各通道的音量
fn volumes(id: AudioObjectId) -> Vec<(u32, f32)> {
    settable_elements(id, VOLUME)
        .into_iter()
        .filter_map(|e| get::<f32>(id, &Address { selector: VOLUME, scope: SCOPE_OUTPUT, element: e }).map(|v| (e, v)))
        .collect()
}

/// 按目标音量的 gain 倍设置各通道
fn set_volumes(id: AudioObjectId, target: &[(u32, f32)], gain: f32) {
    for &(e, v) in target {
        set(id, &Address { selector: VOLUME, scope: SCOPE_OUTPUT, element: e }, &(v * gain));
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

/// 录音结束：恢复声音并播放结束音。输出设备静音时提示音也听不到，所以先恢复、再播；
/// 蓝牙耳机先等它切回音乐模式。都在后台进行，立即返回
pub fn end_with(cue: Cue) {
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    WANTED.store(false, Ordering::SeqCst);
    if MUTED.lock().unwrap().is_empty() {
        let _ = std::fs::remove_file(pending_file());
        crate::sound::play(cue);
        return;
    }
    let current = move || GENERATION.load(Ordering::SeqCst) == generation;
    std::thread::spawn(move || {
        settle(&current);
        let restored = restore(&current, FADE);
        if !current() {
            return; // 期间又开始了录音：开始音已经播过，结束音不再播
        }
        crate::sound::play(cue);
        for ms in RECHECK_AFTER {
            std::thread::sleep(Duration::from_millis(ms));
            if !current() {
                return;
            }
            unmute_again(&restored);
        }
    });
}

/// 退出、更新前调用：立即恢复，不等蓝牙耳机切换、不播提示音、不渐强
pub fn end() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    WANTED.store(false, Ordering::SeqCst);
    restore(&|| true, Duration::ZERO);
}

/// 启动时调用：上次运行留下了未恢复的静音（被强退、崩溃），现在恢复
pub fn recover() {
    let Ok(text) = std::fs::read_to_string(pending_file()) else { return };
    // 旧版本只记了设备 UID
    let entries = serde_json::from_str::<Vec<Muted>>(&text).ok().or_else(|| {
        serde_json::from_str::<Vec<String>>(&text)
            .ok()
            .map(|uids| uids.into_iter().map(|uid| Muted { uid, rate: 0.0, volume: Vec::new() }).collect())
    });
    let Some(entries) = entries else {
        let _ = std::fs::remove_file(pending_file());
        return;
    };
    log::info!("Recovering {} audio device(s) left muted by the last run", entries.len());
    *MUTED.lock().unwrap() = entries;
    restore(&|| true, Duration::ZERO);
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

/// 静音一个设备。已经记着的（上次结束后还没恢复完）重新静音，音量调回原值
fn mute_device(id: AudioObjectId, muted: &mut Vec<Muted>) {
    let Some(uid) = device_uid(id) else { return };
    let elements = mute_elements(id);
    if let Some(entry) = muted.iter_mut().find(|m| m.uid == uid) {
        if !entry.volume.is_empty() {
            set_volumes(id, &entry.volume, 1.0);
            entry.volume.clear();
            save_pending(muted);
        }
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
    let entry = Muted { uid, rate: sample_rate(id), volume: Vec::new() };
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
    let waiting: Vec<(String, f64)> = MUTED
        .lock()
        .unwrap()
        .iter()
        .filter(|m| m.rate > 0.0 && find(&m.uid).is_some_and(is_bluetooth))
        .map(|m| (m.uid.clone(), m.rate))
        .collect();
    if waiting.is_empty() {
        return;
    }
    let start = Instant::now();
    let mut waited = false;
    loop {
        if !current() {
            return;
        }
        // 设备暂时找不到（切换中）也算没切回来
        let pending: Vec<String> = waiting
            .iter()
            .filter(|(uid, rate)| find(uid).is_none_or(|id| sample_rate(id) != *rate))
            .map(|(uid, rate)| format!("{} (waiting for {rate} Hz)", find(uid).map_or_else(|| uid.clone(), describe)))
            .collect();
        if pending.is_empty() {
            if waited {
                std::thread::sleep(SETTLE_GRACE);
                log::info!("Bluetooth output switched back after {} ms", start.elapsed().as_millis());
            }
            return;
        }
        if start.elapsed() >= SETTLE_MAX {
            log::warn!("Bluetooth output did not switch back in time, restoring anyway: {}", pending.join(", "));
            return;
        }
        waited = true;
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 正在恢复的设备：设备、UID、渐强的目标音量
type Target = (AudioObjectId, String, Vec<(u32, f32)>);

/// 取消我们静音的设备，音量在 fade 内从低到高渐强。返回恢复了的设备 UID（之后还要再盯几秒）。
/// 渐强途中又开始录音（current() 变成 false）：音量调回原值、设备重新静音，记录留给下次恢复
fn restore(current: &dyn Fn() -> bool, fade: Duration) -> Vec<String> {
    let steps = if fade.is_zero() { 0 } else { FADE_STEPS };
    let first = |s: u32| (s as f32 / FADE_STEPS as f32).powi(2);
    let targets: Vec<Target> = {
        let mut muted = MUTED.lock().unwrap();
        if !current() {
            return Vec::new();
        }
        if muted.is_empty() {
            // 记了文件但没静音成功时，文件也要清掉
            let _ = std::fs::remove_file(pending_file());
            return Vec::new();
        }
        let mut targets = Vec::new();
        for entry in muted.iter_mut() {
            let Some(id) = find(&entry.uid) else { continue };
            // 上次渐强没做完（被打断、被强退）的，目标仍是当时记下的原音量，不能读现在被调小的音量
            let target = if entry.volume.is_empty() { volumes(id) } else { entry.volume.clone() };
            if steps == 0 {
                set_volumes(id, &entry.volume, 1.0);
                entry.volume.clear();
            } else {
                entry.volume = target;
            }
            targets.push((id, entry.uid.clone(), entry.volume.clone()));
        }
        save_pending(&muted);
        for (id, _, volume) in &targets {
            // 渐强从很小的音量开始（不设成 0：有的设备音量为 0 时会自动静音）
            if steps > 0 {
                set_volumes(*id, volume, first(1));
            }
            let elements = mute_elements(*id);
            if !set_device(*id, &elements, false) {
                log::warn!("Restore: unmuting output {} failed", describe(*id));
            }
        }
        targets
    };
    for step in 2..=steps {
        std::thread::sleep(fade / steps);
        let mut muted = MUTED.lock().unwrap();
        if !current() {
            // 又开始了录音：音量调回原值、重新静音（这些设备还记着）。
            // 被下一次结束（如取消）打断时什么都不做，由它从小音量重新渐强
            if WANTED.load(Ordering::SeqCst) {
                for (id, _, volume) in &targets {
                    set_volumes(*id, volume, 1.0);
                    set_device(*id, &mute_elements(*id), true);
                }
                for entry in muted.iter_mut() {
                    entry.volume.clear();
                }
                save_pending(&muted);
            }
            return Vec::new();
        }
        for (id, _, volume) in &targets {
            set_volumes(*id, volume, first(step));
        }
    }
    let mut muted = MUTED.lock().unwrap();
    if !current() {
        return Vec::new();
    }
    for (id, _, volume) in &targets {
        set_volumes(*id, volume, 1.0);
        log::info!("Restored output {} (reads back {})", describe(*id), state(*id, &mute_elements(*id)));
    }
    // 找不到的设备（已拔掉）也不再记着：重新接上时它的静音状态由系统保存，用户能看到
    muted.clear();
    let _ = std::fs::remove_file(pending_file());
    targets.into_iter().map(|(_, uid, _)| uid).collect()
}

/// 恢复后每隔一会儿调用：恢复过的设备又变成静音（蓝牙耳机切回音乐模式时带回了静音），再取消一次
fn unmute_again(restored: &[String]) {
    let _guard = MUTED.lock().unwrap();
    if WANTED.load(Ordering::SeqCst) {
        return;
    }
    for uid in restored {
        let Some(id) = find(uid) else { continue };
        let elements = mute_elements(id);
        if any_muted(id, &elements) {
            let ok = set_device(id, &elements, false);
            log::info!("Output {} was muted again after restoring, unmuted it {}", describe(id), if ok { "again" } else { "- failed" });
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
