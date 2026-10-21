//! 录音期间静音其他程序的声音，结束后恢复（macOS）
//!
//! macOS 没有 Windows 音量合成器那样按程序静音的接口，这里静音的是默认输出设备：
//! 只在它原本没被静音时静音，恢复时只取消我们静音的设备，用户原本就静音的保持原样。
//! 开始提示音播完后才静音，结束时先恢复再播结束音（见 controller.rs），提示音都听得到。
//! 被静音的设备同时记在文件里：程序被强退来不及恢复时，下次启动按文件恢复。

use super::cf::{CFTypeRef, CF};
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// 被我们静音的设备（设备 UID，重新插拔后也不变），恢复时按 UID 找回
static MUTED: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// 当前是否应处于静音状态。快速开始又停止时，恢复可能抢在静音之前执行；
/// 静音在锁内检查这个标志，避免录音已结束却留下静音
static WANTED: AtomicBool = AtomicBool::new(false);

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
const MUTE: u32 = code(b"mute");
const SCOPE_GLOBAL: u32 = code(b"glob");
const SCOPE_OUTPUT: u32 = code(b"outp");
/// 主通道；没有主通道静音开关的设备逐个声道静音
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

fn device_uid(id: AudioObjectId) -> Option<String> {
    let addr = Address { selector: DEVICE_UID, scope: SCOPE_GLOBAL, element: 0 };
    let uid: CFTypeRef = get::<usize>(id, &addr)? as CFTypeRef;
    CF::owned(uid)?.to_string()
}

/// 设备上可以设置静音的通道：有主通道开关就只用它
fn mute_elements(id: AudioObjectId) -> Vec<u32> {
    let settable = |element| {
        let addr = Address { selector: MUTE, scope: SCOPE_OUTPUT, element };
        let mut ok = 0u8;
        unsafe { AudioObjectHasProperty(id, &addr) != 0 && AudioObjectIsPropertySettable(id, &addr, &mut ok) == 0 && ok != 0 }
    };
    if settable(ELEMENTS[0]) {
        return vec![ELEMENTS[0]];
    }
    ELEMENTS[1..].iter().copied().filter(|&e| settable(e)).collect()
}

fn is_muted(id: AudioObjectId, element: u32) -> bool {
    get::<u32>(id, &Address { selector: MUTE, scope: SCOPE_OUTPUT, element }).is_some_and(|m| m != 0)
}

fn set_muted(id: AudioObjectId, element: u32, muted: bool) -> bool {
    let addr = Address { selector: MUTE, scope: SCOPE_OUTPUT, element };
    let value = u32::from(muted);
    let size = std::mem::size_of::<u32>() as u32;
    unsafe { AudioObjectSetPropertyData(id, &addr, 0, std::ptr::null(), size, &value as *const u32 as *const c_void) == 0 }
}

/// 录音开始：静音默认输出设备
pub fn begin() {
    WANTED.store(true, Ordering::SeqCst);
    mute_output();
}

/// 录音结束：恢复（可重复调用）
pub fn end() {
    WANTED.store(false, Ordering::SeqCst);
    restore();
}

/// 启动时调用：上次运行留下了未恢复的静音（被强退、崩溃），现在恢复
pub fn recover() {
    let Some(uids) = std::fs::read_to_string(pending_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
    else {
        let _ = std::fs::remove_file(pending_file());
        return;
    };
    log::info!("Recovering {} audio device(s) left muted by the last run", uids.len());
    MUTED.lock().unwrap().extend(uids);
    restore();
}

fn pending_file() -> PathBuf {
    crate::config::data_dir().join("muted-devices.json")
}

fn save_pending(uids: &[String]) {
    let _ = std::fs::create_dir_all(crate::config::data_dir());
    let json = serde_json::to_string(uids).expect("device uids are serializable");
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
    let Some(uid) = device_uid(id) else { return };
    let elements = mute_elements(id);
    if muted.contains(&uid) || elements.is_empty() || elements.iter().all(|&e| is_muted(id, e)) {
        return;
    }
    // 先落盘再静音：静音之后、写文件之前被强退也不会漏掉
    let mut pending = muted.clone();
    pending.push(uid.clone());
    save_pending(&pending);
    if elements.iter().fold(false, |any, &e| set_muted(id, e, true) || any) {
        log::info!("Muted output device while recording");
        muted.push(uid);
    }
}

fn restore() {
    let mut muted = MUTED.lock().unwrap();
    if muted.is_empty() {
        // 记了文件但没静音成功时，文件也要清掉
        let _ = std::fs::remove_file(pending_file());
        return;
    }
    for id in all_devices() {
        if device_uid(id).is_some_and(|uid| muted.contains(&uid)) {
            for e in mute_elements(id) {
                set_muted(id, e, false);
            }
        }
    }
    // 找不到的设备（已拔掉）也不再记着：重新接上时它的静音状态由系统保存，用户能看到
    log::info!("Restored {} audio device(s)", muted.len());
    muted.clear();
    let _ = std::fs::remove_file(pending_file());
}
