//! CoreAudio 的最小封装：音频设备的列表、UID、名称、连接方式、采样率、静音。
//! 录音时选麦克风（见 builtin_mic_instead_of_bluetooth）和录音时静音（mute.rs）共用

use super::cf::{CFTypeRef, CF};
use std::ffi::c_void;

pub type AudioObjectId = u32;

#[repr(C)]
pub struct Address {
    pub selector: u32,
    pub scope: u32,
    pub element: u32,
}

const fn code(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const SYSTEM_OBJECT: AudioObjectId = 1;
const DEFAULT_INPUT: u32 = code(b"dIn ");
const DEFAULT_OUTPUT: u32 = code(b"dOut");
const DEVICES: u32 = code(b"dev#");
const DEVICE_UID: u32 = code(b"uid ");
/// 设备名（kAudioDevicePropertyDeviceNameCFString，cpal 的 Device::name 用的也是它）
const NAME: u32 = code(b"name");
const TRANSPORT: u32 = code(b"tran");
const SAMPLE_RATE: u32 = code(b"nsrt");
const STREAMS: u32 = code(b"stm#");
const ALIVE: u32 = code(b"livn");
pub const MUTE: u32 = code(b"mute");
pub const SCOPE_GLOBAL: u32 = code(b"glob");
pub const SCOPE_INPUT: u32 = code(b"inpt");
pub const SCOPE_OUTPUT: u32 = code(b"outp");
/// 蓝牙、低功耗蓝牙的连接方式
const BLUETOOTH: [u32; 2] = [code(b"blue"), code(b"blea")];
/// 内置（Mac 自带的麦克风、扬声器）
const BUILT_IN: u32 = code(b"bltn");
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
pub fn get<T: Default>(id: AudioObjectId, addr: &Address) -> Option<T> {
    let mut value = T::default();
    let mut size = std::mem::size_of::<T>() as u32;
    let status = unsafe { AudioObjectGetPropertyData(id, addr, 0, std::ptr::null(), &mut size, &mut value as *mut T as *mut c_void) };
    (status == 0).then_some(value)
}

/// 写入定长的属性值
pub fn set<T>(id: AudioObjectId, addr: &Address, value: &T) -> bool {
    let size = std::mem::size_of::<T>() as u32;
    unsafe { AudioObjectSetPropertyData(id, addr, 0, std::ptr::null(), size, value as *const T as *const c_void) == 0 }
}

fn global(selector: u32) -> Address {
    Address { selector, scope: SCOPE_GLOBAL, element: 0 }
}

pub fn default_output() -> Option<AudioObjectId> {
    get::<AudioObjectId>(SYSTEM_OBJECT, &global(DEFAULT_OUTPUT)).filter(|&id| id != 0)
}

pub fn default_input() -> Option<AudioObjectId> {
    get::<AudioObjectId>(SYSTEM_OBJECT, &global(DEFAULT_INPUT)).filter(|&id| id != 0)
}

pub fn all_devices() -> Vec<AudioObjectId> {
    let addr = global(DEVICES);
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
    let s: CFTypeRef = get::<usize>(id, &global(selector))? as CFTypeRef;
    CF::owned(s)?.to_string()
}

pub fn device_uid(id: AudioObjectId) -> Option<String> {
    string_property(id, DEVICE_UID)
}

pub fn name(id: AudioObjectId) -> Option<String> {
    string_property(id, NAME)
}

/// 按 UID 找当前连着的设备
pub fn find(uid: &str) -> Option<AudioObjectId> {
    all_devices().into_iter().find(|&id| device_uid(id).as_deref() == Some(uid))
}

fn transport(id: AudioObjectId) -> Option<u32> {
    get::<u32>(id, &global(TRANSPORT))
}

pub fn is_bluetooth(id: AudioObjectId) -> bool {
    transport(id).is_some_and(|t| BLUETOOTH.contains(&t))
}

pub fn sample_rate(id: AudioObjectId) -> f64 {
    get::<f64>(id, &global(SAMPLE_RATE)).unwrap_or(0.0)
}

/// 设备有没有录音用的输入流
fn has_input(id: AudioObjectId) -> bool {
    let addr = Address { selector: STREAMS, scope: SCOPE_INPUT, element: 0 };
    let mut size = 0u32;
    unsafe { AudioObjectGetPropertyDataSize(id, &addr, 0, std::ptr::null(), &mut size) == 0 && size > 0 }
}

/// 日志里的设备描述：名称、连接方式（blue = 蓝牙，bltn = 内置，usb = USB）和采样率
pub fn describe(id: AudioObjectId) -> String {
    let name = name(id).unwrap_or_else(|| format!("device {id}"));
    let transport = transport(id)
        .map(|t| String::from_utf8_lossy(&t.to_be_bytes()).trim().to_string())
        .unwrap_or_else(|| "?".into());
    format!("\"{name}\" ({transport}, {} Hz)", sample_rate(id))
}

/// 设备上可以设置某个输出属性的通道：有主通道就只用它
pub fn settable_elements(id: AudioObjectId, selector: u32) -> Vec<u32> {
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

/// 默认麦克风是蓝牙耳机时，改用的 Mac 内置麦克风（名称，和 cpal 的设备名一致）。
///
/// 蓝牙耳机的麦克风一打开，耳机就从音乐模式切到通话模式：正在播放的声音音质变差、音量换成通话音量，
/// 麦克风关掉后要一两秒才切回来，期间会断音，静音和恢复也跟着乱。用内置麦克风录音，耳机一直留在音乐模式。
/// 没有内置麦克风（Mac mini、合上盖子外接显示器）时返回 None，仍用耳机的麦克风
pub fn builtin_mic_instead_of_bluetooth() -> Option<(String, String)> {
    let input = default_input()?;
    if !is_bluetooth(input) {
        return None;
    }
    let builtin = all_devices()
        .into_iter()
        .find(|&id| transport(id) == Some(BUILT_IN) && has_input(id) && get::<u32>(id, &global(ALIVE)) != Some(0))?;
    Some((name(input)?, name(builtin)?))
}
