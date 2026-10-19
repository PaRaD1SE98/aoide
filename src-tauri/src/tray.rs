//! 托盘图标：灰 = 空闲，红 = 录音中，绿 = 输出完成

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use crate::i18n::tr;
use tauri::AppHandle;

const TRAY_ID: &str = "main";

#[derive(Clone, Copy)]
pub enum TrayState {
    Idle,
    Recording,
    Done,
}

/// 应用图标；状态用右下角的圆点角标表示（红 = 录音中，绿 = 完成）
fn mic_icon(state: TrayState) -> Image<'static> {
    let base = Image::from_bytes(include_bytes!("../icons/64x64.png")).expect("bundled tray icon");
    let (w, h) = (base.width() as usize, base.height() as usize);
    let mut rgba = base.rgba().to_vec();
    let badge: Option<[u8; 3]> = match state {
        TrayState::Idle => None,
        TrayState::Recording => Some([235, 50, 50]),
        TrayState::Done => Some([30, 190, 90]),
    };
    if let Some(color) = badge {
        // 白边圆点，边缘按覆盖率做抗锯齿
        let (cx, cy, r) = (w as f32 * 0.76, h as f32 * 0.76, w as f32 * 0.24);
        for y in 0..h {
            for x in 0..w {
                let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                let outer = (r + 0.5 - d).clamp(0.0, 1.0);
                if outer == 0.0 {
                    continue;
                }
                let inner = (r - 3.0 + 0.5 - d).clamp(0.0, 1.0);
                let i = (y * w + x) * 4;
                for c in 0..3 {
                    let fill = 255.0 * (1.0 - inner) + color[c] as f32 * inner; // 白边 → 彩色内圆
                    rgba[i + c] = (rgba[i + c] as f32 * (1.0 - outer) + fill * outer) as u8;
                }
                rgba[i + 3] = (rgba[i + 3] as f32 * (1.0 - outer) + 255.0 * outer) as u8;
            }
        }
    }
    Image::new_owned(rgba, w as u32, h as u32)
}

pub fn tooltip(hotkey: &str) -> String {
    crate::trf!("Aoide — 按 {hotkey} 开始/停止", "Aoide — press {hotkey} to start/stop")
}

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let open = MenuItem::with_id(app, "settings", tr("打开 Aoide", "Open Aoide"), true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", tr("退出", "Quit"), true, None::<&str>)?;
    Menu::with_items(app, &[&open, &PredefinedMenuItem::separator(app)?, &quit])
}

/// 切换界面语言后重建菜单
pub fn refresh_menu(app: &AppHandle) {
    if let (Some(tray), Ok(menu)) = (app.tray_by_id(TRAY_ID), build_menu(app)) {
        let _ = tray.set_menu(Some(menu));
        log::info!("Tray menu language: {}", tr("中文", "English"));
    }
}

pub fn create(app: &AppHandle, hotkey: &str) -> tauri::Result<()> {
    let menu = build_menu(app)?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(mic_icon(TrayState::Idle))
        .tooltip(tooltip(hotkey))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "settings" => crate::open_settings(app),
            "quit" => {
                crate::mute::end(); // 退出前恢复被静音的程序
                log::info!("=== Aoide Ended ===");
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                crate::open_settings(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

pub fn set_state(app: &AppHandle, state: TrayState) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_icon(Some(mic_icon(state)));
    }
}

pub fn set_tooltip(app: &AppHandle, hotkey: &str) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(tooltip(hotkey)));
    }
}
