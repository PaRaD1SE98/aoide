//! CoreFoundation 的最小封装：引用计数、字符串、数字、布尔值、数组、字典

use std::ffi::c_void;

pub type CFTypeRef = *const c_void;
pub type CFIndex = isize;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CFRange {
    pub location: CFIndex,
    pub length: CFIndex,
}

const UTF8: u32 = 0x0800_0100;
const NUMBER_SINT64: CFIndex = 4;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    pub static kCFBooleanTrue: CFTypeRef;
    pub static kCFBooleanFalse: CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
    fn CFEqual(a: CFTypeRef, b: CFTypeRef) -> u8;
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFStringCreateWithBytes(alloc: CFTypeRef, bytes: *const u8, len: CFIndex, encoding: u32, external: u8) -> CFTypeRef;
    fn CFStringGetLength(s: CFTypeRef) -> CFIndex;
    fn CFStringGetCharacters(s: CFTypeRef, range: CFRange, buffer: *mut u16);
    fn CFBooleanGetValue(b: CFTypeRef) -> u8;
    fn CFNumberGetValue(n: CFTypeRef, kind: CFIndex, out: *mut c_void) -> u8;
    fn CFArrayGetCount(a: CFTypeRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(a: CFTypeRef, i: CFIndex) -> CFTypeRef;
    fn CFDictionaryCreate(
        alloc: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        n: CFIndex,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFTypeRef;
    pub fn CFDataCreate(alloc: CFTypeRef, bytes: *const u8, len: CFIndex) -> CFTypeRef;
}

/// 持有一个引用的 CF 对象，离开作用域时释放
pub struct CF(CFTypeRef);

// 这里用到的 CF 对象（字符串、数字、图片、辅助功能元素）要么不可变、要么本身线程安全
unsafe impl Send for CF {}

impl CF {
    /// 接管「Create / Copy」类函数返回的引用；空指针返回 None
    pub fn owned(ptr: CFTypeRef) -> Option<Self> {
        (!ptr.is_null()).then_some(Self(ptr))
    }

    /// 引用一个不归我们所有的对象（「Get」类函数返回的）：加一次引用计数
    pub fn retained(ptr: CFTypeRef) -> Option<Self> {
        (!ptr.is_null()).then(|| Self(unsafe { CFRetain(ptr) }))
    }

    pub fn as_ptr(&self) -> CFTypeRef {
        self.0
    }

    pub fn string(s: &str) -> Self {
        let ptr = unsafe { CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as CFIndex, UTF8, 0) };
        Self::owned(ptr).expect("CFString from valid UTF-8")
    }

    fn is(&self, type_id: usize) -> bool {
        unsafe { CFGetTypeID(self.0) == type_id }
    }

    /// 是字符串时取出内容
    pub fn to_string(&self) -> Option<String> {
        unsafe {
            if !self.is(CFStringGetTypeID()) {
                return None;
            }
            let len = CFStringGetLength(self.0);
            let mut buf = vec![0u16; len as usize];
            CFStringGetCharacters(self.0, CFRange { location: 0, length: len }, buf.as_mut_ptr());
            Some(String::from_utf16_lossy(&buf))
        }
    }

    pub fn to_bool(&self) -> Option<bool> {
        unsafe { self.is(CFBooleanGetTypeID()).then(|| CFBooleanGetValue(self.0) != 0) }
    }

    pub fn to_i64(&self) -> Option<i64> {
        unsafe {
            if !self.is(CFNumberGetTypeID()) {
                return None;
            }
            let mut v = 0i64;
            (CFNumberGetValue(self.0, NUMBER_SINT64, &mut v as *mut i64 as *mut c_void) != 0).then_some(v)
        }
    }

    /// 是数组时取第 i 项
    pub fn array_item(&self, i: CFIndex) -> Option<CF> {
        unsafe {
            if !self.is(CFArrayGetTypeID()) || i >= CFArrayGetCount(self.0) {
                return None;
            }
            Self::retained(CFArrayGetValueAtIndex(self.0, i))
        }
    }

    pub fn same_as(&self, other: &CF) -> bool {
        unsafe { CFEqual(self.0, other.0) != 0 }
    }

    /// 键值都是 CF 对象的字典
    pub fn dictionary(pairs: &[(CFTypeRef, CFTypeRef)]) -> Self {
        let keys: Vec<CFTypeRef> = pairs.iter().map(|p| p.0).collect();
        let values: Vec<CFTypeRef> = pairs.iter().map(|p| p.1).collect();
        unsafe {
            let ptr = CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                pairs.len() as CFIndex,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            );
            Self::owned(ptr).expect("CFDictionaryCreate")
        }
    }
}

impl Clone for CF {
    fn clone(&self) -> Self {
        Self(unsafe { CFRetain(self.0) })
    }
}

impl Drop for CF {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) }
    }
}
