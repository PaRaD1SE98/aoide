//! 自动更新：读取 GitHub Releases 上的 latest.json（tauri-plugin-updater，安装包有签名校验）
//!
//! - 启动 30 秒后检查一次，之后每 24 小时一次（设置里可关闭）；设置页「关于」可随时手动检查
//! - 发现新版本：系统通知 + 托盘菜单「更新到 x.y.z」+ 设置页显示「立即更新」
//! - 一键更新：下载（进度推给设置页）→ 校验签名 → 运行安装程序（只显示进度）并退出，装完自动重启；
//!   macOS 上就地替换 Aoide.app 后自己重启
//! - 正在录音 / 识别时不安装

use crate::controller::Controller;
use crate::i18n::tr;
use crate::{trf, tray};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

const FIRST_CHECK: Duration = Duration::from_secs(30);
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// 发布页（更新说明）
const RELEASE_URL: &str = "https://github.com/PaRaD1SE98/aoide/releases/tag/v";

#[derive(Clone, Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub notes: String,
    pub url: String,
}

#[derive(Clone, Serialize)]
struct Progress {
    done: u64,
    total: Option<u64>,
}

#[derive(Default)]
pub struct UpdaterState {
    available: Mutex<Option<Update>>,
    /// 已经通知过的版本，避免每次检查都弹通知
    notified: Mutex<Option<String>>,
    installing: AtomicBool,
}

fn info(u: &Update) -> UpdateInfo {
    UpdateInfo { version: u.version.clone(), notes: u.body.clone().unwrap_or_default(), url: format!("{RELEASE_URL}{}", u.version) }
}

/// 当前已知的新版本（托盘菜单、设置页用）
pub fn available(app: &AppHandle) -> Option<UpdateInfo> {
    app.state::<UpdaterState>().available.lock().unwrap().as_ref().map(info)
}

/// 检查更新；有新版本时记下来、刷新托盘菜单、通知设置页，必要时弹系统通知
pub async fn check(app: &AppHandle, notify: bool) -> Result<Option<UpdateInfo>, String> {
    let update = app.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())?;
    let state = app.state::<UpdaterState>();
    let Some(update) = update else {
        log::info!("Update check: up to date");
        *state.available.lock().unwrap() = None;
        tray::refresh_menu(app);
        return Ok(None);
    };
    let found = info(&update);
    log::info!("Update check: {} available", found.version);
    *state.available.lock().unwrap() = Some(update);
    tray::refresh_menu(app);
    let _ = app.emit("update-available", &found);
    let first_time = state.notified.lock().unwrap().replace(found.version.clone()).as_deref() != Some(found.version.as_str());
    if notify && first_time {
        app.state::<Arc<Controller>>().notify(&trf!(
            "Aoide {} 可以更新了：点击托盘菜单「更新到 {}」或在设置中更新",
            "Aoide {} is available: choose \"Update to {}\" in the tray menu or update from Settings",
            "Aoide {} に更新できます。トレイメニューの「{} に更新」か、設定から更新してください",
            found.version,
            found.version
        ));
    }
    Ok(Some(found))
}

/// 后台定时检查（设置里关闭「自动检查更新」时跳过）
pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        std::thread::sleep(FIRST_CHECK);
        loop {
            if app.state::<Arc<Controller>>().config().auto_check_updates {
                if let Err(e) = tauri::async_runtime::block_on(check(&app, true)) {
                    log::warn!("Update check failed: {e}");
                }
            }
            std::thread::sleep(CHECK_EVERY);
        }
    });
}

/// 下载并安装新版本。成功时不会返回：Windows 上安装程序启动后本进程退出，装完自动重启；
/// macOS 上替换完程序包后重启
pub async fn install(app: &AppHandle) -> Result<(), String> {
    if app.state::<Arc<Controller>>().is_busy() {
        return Err(tr("正在录音或识别，请稍后再更新", "Aoide is recording or transcribing. Update when it is done", "録音中または認識中です。終わってから更新してください").into());
    }
    let state = app.state::<UpdaterState>();
    if state.installing.swap(true, Ordering::SeqCst) {
        return Err(tr("正在更新", "An update is already in progress", "更新しています").into());
    }
    let result = async {
        // 先按最新的检查结果来（可能刚发布了更新的版本）
        let update = match app.updater().map_err(|e| e.to_string())?.check().await {
            Ok(Some(u)) => u,
            Ok(None) => return Err(tr("已是最新版本", "Aoide is up to date", "Aoide は最新です").to_string()),
            Err(e) => state.available.lock().unwrap().clone().ok_or_else(|| e.to_string())?,
        };
        log::info!("Updating to {}", update.version);
        let mut done = 0u64;
        let bytes = update
            .download(
                |chunk, total| {
                    done += chunk as u64;
                    let _ = app.emit("update-progress", Progress { done, total });
                },
                || {},
            )
            .await
            .map_err(|e| e.to_string())?;
        let _ = app.emit("update-installing", ());
        // 安装程序会结束本进程：先恢复被静音的程序
        crate::mute::end();
        log::info!("=== Aoide Ended (installing update {}) ===", update.version);
        let installed = update.install(bytes).map_err(|e| e.to_string());
        #[cfg(target_os = "macos")]
        if installed.is_ok() {
            crate::platform::relaunch_after_exit();
            // 万一退出流程卡住，3 秒后直接结束进程，新版本照样会被打开
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_secs(3));
                std::process::exit(0);
            });
            app.exit(0);
        }
        installed
    }
    .await;
    state.installing.store(false, Ordering::SeqCst);
    if let Err(e) = &result {
        log::error!("Update failed: {e}");
        let _ = app.emit("update-error", e.clone());
    }
    result
}
