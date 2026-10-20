//! COM 初始化守卫

use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

/// 在当前线程初始化 COM，离开作用域时反初始化。
/// 注意 COM 对象必须在它之前释放：先声明 guard，再声明持有 COM 对象的局部变量。
pub struct ComGuard(bool);

impl ComGuard {
    pub fn new() -> Self {
        ComGuard(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok())
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}
