#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::utils::config::BackgroundThrottlingPolicy;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

/// Discord の Web クライアント。UI も通信（Gateway / REST）も公式 Web のものをそのまま使う。
const DISCORD_URL: &str = "https://discord.com/app";

/// Discord 公式デスクトップクライアントと同じ UA。
/// `discord/<version>` を含めることで、Discord 側に「ブラウザ」ではなく
/// 「デスクトップアプリ」として認識させる（Chrome 偽装はしない）。
const CHROME_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/141.0.0.0 Safari/537.36";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Config {
    /// ページ読み込み完了ごとに HTTP キャッシュを削除する
    clear_cache_on_navigation: bool,
    /// チャンネル切替（SPA のルート変更）時に前ページのキャッシュを削除する
    clear_cache_on_channel_change: bool,
    /// 最小化中に WebView をサスペンドする（既定 OFF / 通知が止まるため）
    suspend_on_minimize: bool,
    /// 非フォーカス時にメモリ目標を Low にする
    low_memory_on_blur: bool,
    /// 表示倍率（Discord 側に拡大率設定が出ない環境向け）
    zoom: f64,
    /// グローバルに監視するプッシュトゥトークのキー（KeyboardEvent.code 形式。空で無効）
    push_to_talk_code: String,
    /// Windows 起動時に自動で開く
    start_with_windows: bool,
    /// 起動時は最小化（非表示）で開始する
    start_minimized: bool,
    /// 最小化時にタスクトレイへ格納する
    minimize_to_tray: bool,
    /// ハードウェアアクセラレーション（GPU）を使う
    hardware_acceleration: bool,
    /// UI 言語: "auto" (Discord に合わせる) / "en" / "ja"
    language: String,
    /// 画面共有（実験的）
    screen_share: bool,
    /// 画面共有の方式: "rust" (独自キャプチャ) / "browser" (Discord/ブラウザ標準)
    screen_share_mode: String,
    /// チャンネル切替時にページを再読み込みしてメモリを解放する
    reload_on_channel_switch: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            clear_cache_on_navigation: true,
            clear_cache_on_channel_change: true,
            suspend_on_minimize: false,
            low_memory_on_blur: true,
            zoom: 1.0,
            push_to_talk_code: String::new(),
            start_with_windows: false,
            start_minimized: false,
            minimize_to_tray: false,
            hardware_acceleration: true,
            language: "auto".to_string(),
            screen_share: false,
            screen_share_mode: "rust".to_string(),
            reload_on_channel_switch: false,
        }
    }
}

static CONFIG: OnceLock<Mutex<Config>> = OnceLock::new();
static LAST_PATH: OnceLock<Mutex<String>> = OnceLock::new();
static DISCORD_LOCALE: OnceLock<Mutex<String>> = OnceLock::new();
static SELECTED_SOURCE: std::sync::Mutex<Option<(i32, i32, i32, i32)>> = std::sync::Mutex::new(None);
static CAPTURING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static QUITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static SETTINGS_SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn main() {
    migrate_legacy_data();
    init_config();
    configure_webview2();
    log("FastDiscd starting");

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            clear_cache_now,
            close_settings,
            open_settings_window,
            list_sources,
            show_source_picker,
            select_source,
            selection_state,
            reset_selection,
            close_picker,


            get_autostart,
            set_autostart,
            get_discord_locale,
            capture_start,
            capture_stop,
            js_guard_error,
            window_minimize,
            window_toggle_maximize,
            window_close,
            set_zoom,
            debug_log
        ])
        .setup(|app| {
            let url = WebviewUrl::External(DISCORD_URL.parse().expect("invalid Discord URL"));

            let cfg = config();
            let window = WebviewWindowBuilder::new(app, "main", url)
                .title("Discord")
                .visible(!cfg.start_minimized)
                .inner_size(1280.0, 820.0)
                .min_inner_size(720.0, 480.0)
                .user_agent(CHROME_UA)
                .disable_drag_drop_handler()
                .decorations(false)
                .initialization_script(TITLEBAR_JS)
                .initialization_script(SHARE_JS)
                .background_color(tauri::utils::config::Color(30, 31, 34, 255))
                .background_throttling(BackgroundThrottlingPolicy::Suspend)
                .build()?;

            let main_handle = window.clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { .. } = event {
                    QUITTING.store(true, std::sync::atomic::Ordering::SeqCst);
                    if let Some(settings) = main_handle.app_handle().get_webview_window("settings") {
                        let _ = settings.close();
                    }
                    main_handle.app_handle().exit(0);
                }
            });

            install_webview_hooks(&window);
            apply_zoom(&window, config().zoom);
            build_tray(app)?;

            // 設定ウィンドウは起動を邪魔しないよう数秒後に事前生成する
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(4));
                if let Err(e) = create_settings_window(&handle, true) {
                    log(&format!("prewarm settings window failed: {e}"));
                }
            });

            let win = window.clone();
            std::thread::spawn(move || supervisor(win));

            // グローバル PTT（別ウィンドウを見ていてもキーを拾う）
            let ptt_win = window.clone();
            std::thread::spawn(move || ptt_loop(ptt_win));

            if std::env::var_os("RUSTCORD_OPEN_SETTINGS").is_some() {
                open_settings(app.handle());
            }

            {
                let app_handle = app.handle().clone();
                let _ = WebviewWindowBuilder::new(&app_handle, "picker", WebviewUrl::App("picker.html".into()))
                    .title("配信する画面を選択")
                    .inner_size(720.0, 620.0)
                    .resizable(true)
                    .maximizable(false)
                    .minimizable(false)
                    .visible(false)
                    .background_color(tauri::utils::config::Color(30, 31, 34, 255))
                    .build();
            }

            if std::env::var_os("RUSTCORD_TEST_WGC").is_some() {
                *SELECTED_MONITOR.lock().unwrap() = Some(0);
                CAPTURING.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = start_wgc_capture(app.handle().clone(), 0);
            }

            if std::env::var_os("RUSTCORD_OPEN_PICKER").is_some() {
                show_source_picker(app.handle().clone());
            }

            if std::env::var_os("RUSTCORD_SELFTEST").is_some() {
                let win = window.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(15));
                    selftest(&win);
                });
            }

            if std::env::var_os("RUSTCORD_DEVTOOLS").is_some() {
                window.open_devtools();
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("FastDiscd の起動に失敗しました");
}

// ---------------------------------------------------------------------------
// 設定
// ---------------------------------------------------------------------------

fn data_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|local| PathBuf::from(local).join("FastDiscd"))
}

/// 旧名 (RustCord / FastDisc) のデータを FastDiscd フォルダへ移行する。
fn migrate_legacy_data() {
    let Some(local) = std::env::var_os("LOCALAPPDATA") else {
        return;
    };
    let base = PathBuf::from(local);
    let new = base.join("FastDiscd");
    if new.exists() {
        return;
    }

    for old_name in ["FastDisc", "RustCord"] {
        let old = base.join(old_name);
        if !old.join("EBWebView").exists() {
            continue;
        }
        let _ = std::fs::create_dir_all(&new);
        for name in ["EBWebView", "config.json"] {
            let from = old.join(name);
            if from.exists() {
                let _ = std::fs::rename(&from, new.join(name));
            }
        }
        for log_name in ["fastdisc.log", "rustcord.log"] {
            let from = old.join(log_name);
            if from.exists() {
                let _ = std::fs::rename(&from, new.join("fastdiscd.log"));
            }
        }
        break;
    }
}
fn init_config() {
    let config = data_dir()
        .and_then(|dir| std::fs::read_to_string(dir.join("config.json")).ok())
        .and_then(|text| serde_json::from_str::<Config>(&text).ok())
        .unwrap_or_default();

    let _ = CONFIG.set(Mutex::new(config));
    let _ = LAST_PATH.set(Mutex::new(String::new()));
    let _ = DISCORD_LOCALE.set(Mutex::new(String::new()));
}
fn config() -> Config {
    CONFIG
        .get()
        .map(|c| c.lock().map(|guard| guard.clone()).unwrap_or_default())
        .unwrap_or_default()
}

fn update_config(f: impl FnOnce(&mut Config) -> bool) -> bool {
    let Some(store) = CONFIG.get() else {
        return false;
    };
    let mut guard = match store.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let value = f(&mut guard);
    let json = serde_json::to_string_pretty(&*guard).unwrap_or_default();
    drop(guard);

    if let Some(dir) = data_dir() {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("config.json"), json);
    }
    value
}

// ---------------------------------------------------------------------------
// コマンド（設定ウィンドウから呼ばれる）
// ---------------------------------------------------------------------------

#[tauri::command]
fn get_settings() -> Config {
    config()
}

#[tauri::command]
fn save_settings(app: AppHandle, settings: Config) {
    let zoom = settings.zoom.clamp(0.5, 2.0);
    update_config(move |c| {
        *c = settings;
        c.zoom = zoom;
        true
    });

    if let Some(window) = app.get_webview_window("main") {
        apply_zoom(&window, zoom);
    }
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.set_title(settings_title());
    }
    log(&format!("settings saved: zoom={zoom} lang={}", config().language));
}

#[tauri::command]
fn clear_cache_now(app: AppHandle) {
    clear_cache_for(&app);
}

#[tauri::command]
fn debug_log(message: String) {
    log(&format!("settings-window: {message}"));
}

#[tauri::command]
fn close_settings(app: AppHandle) {
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.hide();
    }
}

#[tauri::command]
fn open_settings_window(app: AppHandle) {
    open_settings(&app);
}


#[tauri::command]
fn window_minimize(window: WebviewWindow) {
    if let Ok(hwnd) = window.hwnd() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_MINIMIZE);
        }
    }
}

#[tauri::command]
fn window_toggle_maximize(window: WebviewWindow) -> bool {
    if let Ok(hwnd) = window.hwnd() {
        unsafe {
            if IsZoomed(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
                return false;
            }
            let _ = ShowWindow(hwnd, SW_MAXIMIZE);
            return true;
        }
    }
    false
}

#[tauri::command]
fn window_close(window: WebviewWindow) {
    let _ = window.close();
}

#[tauri::command]
fn set_zoom(app: AppHandle, zoom: f64) {
    let value = zoom.clamp(0.5, 2.0);
    update_config(|c| {
        c.zoom = value;
        true
    });
    if let Some(window) = app.get_webview_window("main") {
        apply_zoom(&window, value);
    }
    log(&format!("zoom set to {value}"));
}

#[tauri::command]
fn get_autostart() -> bool {
    let output = std::process::Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "FastDiscd",
        ])
        .output();
    matches!(output, Ok(o) if o.status.success())
}

#[tauri::command]
fn set_autostart(enabled: bool) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let command = format!("\"{}\"", exe.display());

    let status = if enabled {
        std::process::Command::new("reg")
            .args([
                "add",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "FastDiscd",
                "/t",
                "REG_SZ",
                "/d",
                &command,
                "/f",
            ])
            .status()
    } else {
        std::process::Command::new("reg")
            .args([
                "delete",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "FastDiscd",
                "/f",
            ])
            .status()
    };

    matches!(status, Ok(s) if s.success())
}

#[tauri::command]
fn get_discord_locale() -> String {
    DISCORD_LOCALE
        .get()
        .and_then(|m| m.lock().ok().map(|g| g.clone()))
        .unwrap_or_default()
}

fn open_settings(app: &AppHandle) {
    SETTINGS_SHOWN.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.center();
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        return;
    }

    if let Err(e) = create_settings_window(app, false) {
        log(&format!("failed to open settings window: {e}"));
    }
}

fn create_settings_window(app: &AppHandle, prewarm: bool) -> tauri::Result<WebviewWindow> {
    let window = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("settings.html".into()))
        .title(settings_title())
        .inner_size(580.0, 660.0)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .visible(!prewarm)
        .background_throttling(BackgroundThrottlingPolicy::Disabled)
        .center()
        .build()?;

    // × ボタンで破棄せず非表示にする（再生成によるフリーズ防止）
    let handle = window.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            if QUITTING.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            api.prevent_close();
            let _ = handle.hide();
        }
    });

    Ok(window)
}

fn settings_title() -> &'static str {
    if ui_lang() == "ja" { "FastDiscd 設定" } else { "FastDiscd Settings" }
}

fn ui_lang() -> &'static str {
    match config().language.as_str() {
        "ja" => "ja",
        "en" => "en",
        _ => {
            let discord = DISCORD_LOCALE
                .get()
                .and_then(|m| m.lock().ok().map(|g| g.clone()))
                .unwrap_or_default();
            if discord.to_ascii_lowercase().starts_with("ja") {
                "ja"
            } else if !discord.is_empty() {
                "en"
            } else {
                unsafe {
                    let langid = GetUserDefaultUILanguage();
                    if (langid & 0x3ff) == 0x11 { "ja" } else { "en" }
                }
            }
        }
    }
}

fn clear_cache_for(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let _ = window.with_webview(|webview| {
        let controller = webview.controller();
        if let Ok(core) = unsafe { controller.CoreWebView2() } {
            clear_http_cache(&core);
        }
    });
}

// ---------------------------------------------------------------------------
// トレイメニュー
// ---------------------------------------------------------------------------

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let ja = ui_lang() == "ja";
    let settings_item = MenuItem::with_id(app, "settings", if ja { "設定..." } else { "Settings..." }, true, None::<&str>)?;
    let clear_item =
        MenuItem::with_id(app, "clear_now", if ja { "キャッシュを今すぐ削除" } else { "Clear cache now" }, true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", if ja { "終了" } else { "Quit" }, true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;

    let items: [&dyn tauri::menu::IsMenuItem<tauri::Wry>; 4] =
        [&settings_item, &clear_item, &separator, &quit_item];
    let menu = Menu::with_items(app, &items)?;

    let icon = app.default_window_icon().cloned().unwrap_or_else(|| {
        tauri::image::Image::new_owned(vec![88, 101, 242, 255].repeat(16 * 16), 16, 16)
    });

    TrayIconBuilder::with_id("fastdiscd-tray")
        .icon(icon)
        .tooltip("FastDiscd")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "settings" => open_settings(app),
            "clear_now" => clear_cache_for(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.unminimize();
                    let _ = window.set_focus();
                }
            }
        })
        .build(app)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// WebView フック
// ---------------------------------------------------------------------------

fn install_webview_hooks(window: &WebviewWindow) {
    let result = window.with_webview(|webview| {
        let controller = webview.controller();
        let core = match unsafe { controller.CoreWebView2() } {
            Ok(core) => core,
            Err(e) => {
                log(&format!("CoreWebView2() failed: {e}"));
                return;
            }
        };

        // 権限要求の自動許可（WebView2 には許可ダイアログが無い）
        let permission = PermissionRequestedEventHandler::create(Box::new(|_sender, args| {
            if let Some(args) = args {
                unsafe {
                    args.SetState(COREWEBVIEW2_PERMISSION_STATE_ALLOW)?;
                }
            }
            Ok(())
        }));
        let mut permission_token = 0i64;
        unsafe {
            match core.add_PermissionRequested(&permission, &mut permission_token) {
                Ok(()) => log("permission handler registered"),
                Err(e) => log(&format!("add_PermissionRequested failed: {e}")),
            }
        }
        std::mem::forget(permission);

        // ページ読み込み完了ごとにキャッシュを削除（設定で ON/OFF）
        let navigation = NavigationCompletedEventHandler::create(Box::new(|sender, _args| {
            if config().clear_cache_on_navigation {
                if let Some(sender) = sender {
                    clear_http_cache(&sender);
                }
            }
            Ok(())
        }));
        let mut navigation_token = 0i64;
        unsafe {
            match core.add_NavigationCompleted(&navigation, &mut navigation_token) {
                Ok(()) => log("navigation hook registered"),
                Err(e) => log(&format!("add_NavigationCompleted failed: {e}")),
            }
        }
        std::mem::forget(navigation);

        if let Ok(controller2) = controller.cast::<ICoreWebView2Controller2>() {
            let color = COREWEBVIEW2_COLOR { A: 255, R: 30, G: 31, B: 34 };
            unsafe {
                let _ = controller2.SetDefaultBackgroundColor(color);
            }
        }

        // 画面共有（配信）を許可する
        if let Ok(core27) = core.cast::<ICoreWebView2_27>() {
            let screen = ScreenCaptureStartingEventHandler::create(Box::new(|_sender, args| {
                if let Some(args) = args {
                    unsafe {
                        let _ = args.SetHandled(false);
                        let _ = args.SetCancel(false);
                    }
                }
                Ok(())
            }));
            let mut screen_token = 0i64;
            unsafe {
                match core27.add_ScreenCaptureStarting(&screen, &mut screen_token) {
                    Ok(()) => log("screen capture hook registered"),
                    Err(e) => log(&format!("add_ScreenCaptureStarting failed: {e}")),
                }
            }
            std::mem::forget(screen);
        }    });

    if let Err(e) = result {
        log(&format!("with_webview failed: {e}"));
    }
}

/// HTTP ディスクキャッシュ（画像・GIF・JS など）を削除する。
fn clear_http_cache(core: &ICoreWebView2) {
    unsafe {
        let Ok(profile) = core.cast::<ICoreWebView2_13>().and_then(|c| c.Profile()) else {
            return;
        };
        let Ok(profile2) = profile.cast::<ICoreWebView2Profile2>() else {
            return;
        };
        let handler = ClearBrowsingDataCompletedHandler::create(Box::new(|hr| {
            log(&format!("cache cleared hr={hr:?}"));
            Ok(())
        }));
        if let Err(e) =
            profile2.ClearBrowsingData(COREWEBVIEW2_BROWSING_DATA_KINDS_DISK_CACHE, &handler)
        {
            log(&format!("ClearBrowsingData failed: {e}"));
        }
    }
}

/// 表示倍率を適用する（Discord 側に拡大率設定が出ない環境向け）。
fn apply_zoom(window: &WebviewWindow, zoom: f64) {
    let zoom = zoom.clamp(0.5, 2.0);
    let _ = window.with_webview(move |webview| {
        let controller = webview.controller();
        unsafe {
            let _ = controller.SetZoomFactor(zoom);
        }
    });
}

/// 最小化・フォーカス・チャンネル切替を監視する。
fn supervisor(window: WebviewWindow) {
    let mut suspended = false;
    let mut low_memory = false;
    let mut tick: u32 = 0;
    let mut last_apply = std::time::Instant::now();
    let mut low_since: Option<std::time::Instant> = None;
    let mut last_trim = std::time::Instant::now() - Duration::from_secs(3600);
    let mut last_pressure = std::time::Instant::now() - Duration::from_secs(3600);
    let mut last_gc = std::time::Instant::now() - Duration::from_secs(3600);

    loop {
        std::thread::sleep(Duration::from_millis(1200));
        tick = tick.wrapping_add(1);

        let cfg = config();
        // Win32 で直接判定（Tauri の is_focused はフレームレス窓で不安定なことがある）
        let (minimized, focused) = match window.hwnd() {
            Ok(h) => unsafe { (IsIconic(h).as_bool(), GetForegroundWindow() == h) },
            Err(_) => (window.is_minimized().unwrap_or(false), window.is_focused().unwrap_or(true)),
        };

        let want_suspend = false;
        let want_low = minimized || !focused;

        if want_low {
            if last_pressure.elapsed() > Duration::from_secs(5) {
                send_memory_pressure(&window, "critical");
                last_pressure = std::time::Instant::now();
            }
            if last_gc.elapsed() > Duration::from_secs(30) {
                request_gc(&window);
                last_gc = std::time::Instant::now();
            }
            if last_trim.elapsed() > Duration::from_secs(5) {
                trim_webview_processes();
                last_trim = std::time::Instant::now();
            }
            if low_since.is_none() { low_since = Some(std::time::Instant::now()); }
        } else {
            low_since = None;
        }

        let reassert = want_low && last_apply.elapsed() > Duration::from_secs(20);
        if want_suspend != suspended || want_low != low_memory || reassert {
            apply_memory_state(&window, want_suspend, want_low);
            suspended = want_suspend;
            low_memory = want_low;
            last_apply = std::time::Instant::now();
        }

        maybe_run_command(&window);

        if tick % 10 == 0 && !suspended {
            poll_locale(&window);
        }

        // 表示中のみ、チャンネル（SPA ルート）の変化を監視してキャッシュを破棄する
        // 最小化時にトレイへ格納
        if cfg.minimize_to_tray && minimized && window.is_visible().unwrap_or(false) {
            let _ = window.hide();
        }

        if !suspended && focused && tick % 2 == 0 && cfg.clear_cache_on_channel_change {
            poll_route(&window);
        }
    }
}


/// デバッグ用: `%LOCALAPPDATA%\RustCord\cmd.js` があれば、その内容をページで実行して結果をログに出す。
fn maybe_run_command(window: &WebviewWindow) {
    let Some(dir) = data_dir() else { return };
    let path = dir.join("cmd.js");
    let Ok(script) = std::fs::read_to_string(&path) else {
        return;
    };
    let _ = std::fs::remove_file(&path);

    let _ = window.with_webview(move |webview| {
        let controller = webview.controller();
        let Ok(core) = (unsafe { controller.CoreWebView2() }) else {
            return;
        };

        let js = HSTRING::from(script);
        let handler = ExecuteScriptCompletedHandler::create(Box::new(|hr, result| {
            log(&format!("cmd hr={hr:?} result={result}"));
            Ok(())
        }));
        unsafe {
            let _ = core.ExecuteScript(&js, &handler);
        }
    });
}



/// 非フォーカス中に V8 ヒープの GC を要求して、以前開いたチャンネルの情報を解放させる
fn request_gc(window: &WebviewWindow) {
    let _ = window.with_webview(|webview| {
        let controller = webview.controller();
        let Ok(core) = (unsafe { controller.CoreWebView2() }) else {
            return;
        };
        collect_garbage(&core);
    });
}
/// Chromium に定期的なメモリ逼迫を通知してキャッシュを手放させる（挙動は穏やか）
fn send_memory_pressure(window: &WebviewWindow, level: &str) {
    let level = HSTRING::from(level);
    let _ = window.with_webview(move |webview| {
        let controller = webview.controller();
        let Ok(core) = (unsafe { controller.CoreWebView2() }) else {
            return;
        };
        simulate_memory_pressure(&core, &level.to_string());
    });
}

#[derive(Clone, serde::Serialize)]
struct CaptureSource {
    id: String,
    name: String,
    kind: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    thumb: String,
}

static SELECTED_WINDOW: std::sync::Mutex<Option<isize>> = std::sync::Mutex::new(None);
static SELECTED_MONITOR: std::sync::Mutex<Option<usize>> = std::sync::Mutex::new(None);

fn encode_thumb(pixels: &[u8], w: i32, h: i32) -> String {
    let rgb: Vec<u8> = pixels
        .chunks_exact(4)
        .flat_map(|p| [p[2], p[1], p[0]])
        .collect();
    let mut out = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 55);
    if enc.encode(&rgb, w as u32, h as u32, image::ExtendedColorType::Rgb8).is_err() {
        return String::new();
    }
    base64::engine::general_purpose::STANDARD.encode(out)
}

unsafe extern "system" fn enum_monitor_proc(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> windows::core::BOOL {
    let monitors = &mut *(data.0 as *mut Vec<HMONITOR>);
    monitors.push(monitor);
    windows::core::BOOL(1)
}

/// GDI で指定矩形のサムネイルを作る（画面全体用）
fn screen_thumb(x: i32, y: i32, w: i32, h: i32, tw: i32, th: i32) -> String {
    use windows::Win32::Graphics::Gdi::*;
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() { return String::new(); }
        let mem = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, tw, th);
        let old = SelectObject(mem, bitmap.into());
        let _ = SetStretchBltMode(mem, HALFTONE);
        let _ = StretchBlt(mem, 0, 0, tw, th, Some(screen), x, y, w, h, SRCCOPY);
        let mut pixels = vec![0u8; (tw * th * 4) as usize];
        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = tw;
        info.bmiHeader.biHeight = -th;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB.0;
        let _ = GetDIBits(mem, bitmap, 0, th as u32, Some(pixels.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
        let _ = SelectObject(mem, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(mem);
        let _ = ReleaseDC(None, screen);
        encode_thumb(&pixels, tw, th)
    }
}

/// ウィンドウのサムネイル（PrintWindow）
fn window_thumb(hwnd: isize, tw: i32, th: i32) -> String {
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::Foundation::HWND;
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() { return String::new(); }
        let mem = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, tw, th);
        let old = SelectObject(mem, bitmap.into());
        let _ = SetStretchBltMode(mem, HALFTONE);
        let h = HWND(hwnd as *mut core::ffi::c_void);
        let src_dc = GetDC(Some(h));
        let _ = BitBlt(mem, 0, 0, tw, th, Some(src_dc), 0, 0, SRCCOPY);
        let _ = ReleaseDC(Some(h), src_dc);
        let mut pixels = vec![0u8; (tw * th * 4) as usize];
        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = tw;
        info.bmiHeader.biHeight = -th;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB.0;
        let _ = GetDIBits(mem, bitmap, 0, th as u32, Some(pixels.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
        let _ = SelectObject(mem, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(mem);
        let _ = ReleaseDC(None, screen);
        encode_thumb(&pixels, tw, th)
    }
}

#[tauri::command]
fn list_sources() -> Vec<CaptureSource> {
    let mut out = Vec::new();
    unsafe {
        let mut monitors: Vec<HMONITOR> = Vec::new();
        let _ = EnumDisplayMonitors(None::<HDC>, None::<*const RECT>, Some(enum_monitor_proc), LPARAM(&mut monitors as *mut _ as isize));
        for (i, mon) in monitors.iter().enumerate() {
            let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            if GetMonitorInfoW(*mon, &mut info).as_bool() {
                let r = info.rcMonitor;
                let w = r.right - r.left;
                let h = r.bottom - r.top;
                out.push(CaptureSource {
                    id: format!("monitor:{i}"),
                    name: format!("画面 {}", i + 1),
                    kind: "screen".into(),
                    x: r.left,
                    y: r.top,
                    width: w,
                    height: h,
                    thumb: screen_thumb(r.left, r.top, w, h, 160, 90),
                });
            }
        }

        let mut windows: Vec<(isize, String, RECT)> = Vec::new();
        let _ = EnumWindows(Some(enum_window_proc), LPARAM(&mut windows as *mut _ as isize));
        for (hwnd, title, rect) in windows {
            let w = rect.right - rect.left;
            let h = rect.bottom - rect.top;
            if w <= 0 || h <= 0 { continue; }
            out.push(CaptureSource {
                id: format!("window:{hwnd}"),
                name: title,
                kind: "window".into(),
                x: rect.left,
                y: rect.top,
                width: w,
                height: h,
                thumb: window_thumb(hwnd, 160, 90),
            });
        }
    }
    out
}

unsafe extern "system" fn enum_window_proc(hwnd: HWND, data: LPARAM) -> windows::core::BOOL {
    let list = &mut *(data.0 as *mut Vec<(isize, String, RECT)>);
    if !IsWindowVisible(hwnd).as_bool() { return windows::core::BOOL(1); }
    if IsIconic(hwnd).as_bool() { return windows::core::BOOL(1); }
    let mut pid = 0u32;
    let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == std::process::id() { return windows::core::BOOL(1); }
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 { return windows::core::BOOL(1); }
    let mut buf = vec![0u16; (len + 1) as usize];
    let n = GetWindowTextW(hwnd, &mut buf);
    let title = String::from_utf16_lossy(&buf[..n as usize]);
    if title.trim().is_empty() { return windows::core::BOOL(1); }
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() { return windows::core::BOOL(1); }
    list.push((hwnd.0 as isize, title, rect));
    windows::core::BOOL(1)
}

#[tauri::command]
fn show_source_picker(app: AppHandle) {
    log("show_source_picker invoked");
    match app.get_webview_window("picker") {
        Some(window) => {
            let _ = window.show();
            let _ = window.set_focus();
            log("picker shown");
        }
        None => log("picker window missing"),
    }
}
#[tauri::command]
fn close_picker(app: AppHandle) {
    if let Some(window) = app.get_webview_window("picker") {
        let _ = window.hide();
    }
    let _ = app.emit("fastdisc://source-cancelled", ());
}
#[tauri::command]
fn reset_selection() {
    *SELECTED_SOURCE.lock().unwrap() = None;
    *SELECTED_WINDOW.lock().unwrap() = None;
    *SELECTED_MONITOR.lock().unwrap() = None;
}

#[tauri::command]
fn selection_state() -> bool {
    SELECTED_SOURCE.lock().map(|s| s.is_some()).unwrap_or(false)
}

#[tauri::command]
fn select_source(app: AppHandle, id: String, kind: String, x: i32, y: i32, width: i32, height: i32) {
    *SELECTED_SOURCE.lock().unwrap() = Some((x, y, width, height));
    let hwnd = id.strip_prefix("window:").and_then(|v| v.parse::<isize>().ok());
    *SELECTED_WINDOW.lock().unwrap() = hwnd;
    *SELECTED_MONITOR.lock().unwrap() = id.strip_prefix("monitor:").and_then(|v| v.parse::<usize>().ok());
    log(&format!("capture source selected: {id}"));
    if let Some(window) = app.get_webview_window("picker") {
        let _ = window.hide();
    }
    let _ = app.emit("fastdisc://source-selected", ());
}
/// Discord の表示言語を取得して保持する（「Discordに合わせる」用）。
fn poll_locale(window: &WebviewWindow) {
    let _ = window.with_webview(|webview| {
        let controller = webview.controller();
        let Ok(core) = (unsafe { controller.CoreWebView2() }) else {
            return;
        };

        let script = HSTRING::from("[document.documentElement.lang, navigator.language].filter(Boolean).join('|')");
        let handler = ExecuteScriptCompletedHandler::create(Box::new(|hr, result| {
            if hr.is_err() {
                return Ok(());
            }
            let value = result.trim_matches('"').to_string();
            let first = value.split('|').next().unwrap_or("").to_string();
            if !first.is_empty() {
                if let Some(store) = DISCORD_LOCALE.get() {
                    if let Ok(mut guard) = store.lock() {
                        *guard = first;
                    }
                }
            }
            Ok(())
        }));
        unsafe {
            let _ = core.ExecuteScript(&script, &handler);
        }
    });
}
/// 現在のパスを取得し、前回と変わっていればキャッシュを削除する。
fn poll_route(window: &WebviewWindow) {
    let _ = window.with_webview(|webview| {
        let controller = webview.controller();
        let Ok(core) = (unsafe { controller.CoreWebView2() }) else {
            return;
        };

        let script = HSTRING::from("location.pathname");
        let core_for_handler = core.clone();
        let handler = ExecuteScriptCompletedHandler::create(Box::new(move |hr, result| {
            if hr.is_err() {
                return Ok(());
            }

            let path = serde_json::from_str::<String>(&result).unwrap_or_default();
            if path.is_empty() {
                return Ok(());
            }

            let Some(store) = LAST_PATH.get() else {
                return Ok(());
            };
            let mut last = match store.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };

            if !last.is_empty() && *last != path && !last.starts_with("/login") {
                log(&format!("route changed: {} -> {path}", *last));
                clear_http_cache(&core_for_handler);
                collect_garbage(&core_for_handler);
                if config().reload_on_channel_switch {
                    unsafe { let _ = core_for_handler.Reload(); }
                }
            }

            *last = path;
            Ok(())
        }));

        unsafe {
            let _ = core.ExecuteScript(&script, &handler);
        }
    });
}

fn apply_memory_state(window: &WebviewWindow, suspend: bool, low_memory: bool) -> bool {
    let ok = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ok_inner = ok.clone();

    let dispatched = window.with_webview(move |webview| {
        let controller = webview.controller();

        // 可視状態は CoreWebView2 が取れなくても必ず戻す（復帰時に真っ白になるのを防ぐ）
        unsafe {
            let _ = controller.SetIsVisible(!suspend);
        }

        let core = match unsafe { controller.CoreWebView2() } {
            Ok(core) => core,
            Err(e) => {
                log(&format!("CoreWebView2() failed in memory state: {e}"));
                return;
            }
        };

        unsafe {
            if suspend {
                // サスペンドは復帰時にハングするため使わない（Low メモリ目標のみ）
            }

            if let Ok(core19) = core.cast::<ICoreWebView2_19>() {
                let level = if low_memory {
                    COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
                } else {
                    COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
                };
                let _ = core19.SetMemoryUsageTargetLevel(level);
                log(&format!("memory level low={low_memory}"));
            }
        }

        ok_inner.store(true, std::sync::atomic::Ordering::SeqCst);
    });

    dispatched.is_ok() && ok.load(std::sync::atomic::Ordering::SeqCst)
}

/// 動作確認用: ページ状態をログに出す。
fn selftest(window: &WebviewWindow) {
    let _ = window.with_webview(|webview| {
        let controller = webview.controller();
        let Ok(core) = (unsafe { controller.CoreWebView2() }) else {
            log("selftest: no core");
            return;
        };

        let script = HSTRING::from(
            "JSON.stringify({href:location.href,title:document.title,native:typeof window.DiscordNative,\
             nodes:document.querySelectorAll('*').length,iw:innerWidth,text:(document.body?document.body.innerText:'').slice(0,60)})",
        );
        let handler = ExecuteScriptCompletedHandler::create(Box::new(|hr, result| {
            log(&format!("selftest hr={hr:?} result={result}"));
            Ok(())
        }));

        unsafe {
            let _ = core.ExecuteScript(&script, &handler);
        }
    });
}


// ---------------------------------------------------------------------------
// プッシュトゥトーク（グローバル）
// ---------------------------------------------------------------------------

fn vk_from_code(code: &str) -> Option<u32> {
    let c = code.trim();

    if let Some(rest) = c.strip_prefix("Key") {
        if rest.len() == 1 {
            let ch = rest.as_bytes()[0];
            if ch.is_ascii_uppercase() {
                return Some(ch as u32);
            }
        }
    }
    if let Some(rest) = c.strip_prefix("Digit") {
        if rest.len() == 1 {
            let ch = rest.as_bytes()[0];
            if ch.is_ascii_digit() {
                return Some(ch as u32);
            }
        }
    }
    if let Some(rest) = c.strip_prefix('F') {
        if let Ok(n) = rest.parse::<u32>() {
            if (1..=24).contains(&n) {
                return Some(0x70 + n - 1);
            }
        }
    }

    let vk = match c {
        "Space" => 0x20,
        "Tab" => 0x09,
        "Enter" => 0x0D,
        "Escape" => 0x1B,
        "Backspace" => 0x08,
        "ShiftLeft" | "ShiftRight" => 0x10,
        "ControlLeft" | "ControlRight" => 0x11,
        "AltLeft" | "AltRight" => 0x12,
        "CapsLock" => 0x14,
        "ArrowLeft" => 0x25,
        "ArrowUp" => 0x26,
        "ArrowRight" => 0x27,
        "ArrowDown" => 0x28,
        "Insert" => 0x2D,
        "Delete" => 0x2E,
        "Home" => 0x24,
        "End" => 0x23,
        "PageUp" => 0x21,
        "PageDown" => 0x22,
        "Backquote" => 0xC0,
        "Minus" => 0xBD,
        "Equal" => 0xBB,
        "BracketLeft" => 0xDB,
        "BracketRight" => 0xDD,
        "Backslash" => 0xDC,
        "Semicolon" => 0xBA,
        "Quote" => 0xDE,
        "Comma" => 0xBC,
        "Period" => 0xBE,
        "Slash" => 0xBF,
        _ => return None,
    };
    Some(vk)
}

fn key_from_code(code: &str) -> String {
    if let Some(rest) = code.strip_prefix("Key") {
        if rest.len() == 1 {
            return rest.to_ascii_lowercase();
        }
    }
    if let Some(rest) = code.strip_prefix("Digit") {
        return rest.to_string();
    }
    match code {
        "Space" => " ".to_string(),
        "Enter" => "Enter".to_string(),
        "Tab" => "Tab".to_string(),
        other => other.to_string(),
    }
}

fn ptt_script(code: &str, down: bool) -> String {
    let event = if down { "keydown" } else { "keyup" };
    let key = key_from_code(code);
    format!(
        "(function(){{var e=new KeyboardEvent(\"{event}\",{{code:\"{code}\",key:\"{key}\",bubbles:true,cancelable:true}});document.dispatchEvent(e);return true;}})()"
    )
}

#[cfg(windows)]
fn ptt_loop(window: WebviewWindow) {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

    let mut pressed = false;

    loop {
        std::thread::sleep(Duration::from_millis(25));

        let code = config().push_to_talk_code;
        if code.is_empty() {
            pressed = false;
            continue;
        }

        let Some(vk) = vk_from_code(&code) else {
            continue;
        };

        let down = unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 };
        if down == pressed {
            continue;
        }
        pressed = down;

        let script = ptt_script(&code, down);
        log(&format!("ptt {} {code}", if down { "down" } else { "up" }));

        let _ = window.with_webview(move |webview| {
            let controller = webview.controller();
            if let Ok(core) = unsafe { controller.CoreWebView2() } {
                let js = HSTRING::from(script);
                let handler = ExecuteScriptCompletedHandler::create(Box::new(|_hr, _res| Ok(())));
                unsafe {
                    let _ = core.ExecuteScript(&js, &handler);
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// 画面共有（実験的）: Rust 側で画面をキャプチャして JPEG フレームを JS に送る
// ---------------------------------------------------------------------------

#[tauri::command]
fn capture_start(app: AppHandle) -> bool {
    if CAPTURING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return true;
    }
    let monitor = *SELECTED_MONITOR.lock().unwrap();
    if let Some(index) = monitor {
        if start_wgc_capture(app.clone(), index) {
            return true;
        }
    }
    std::thread::spawn(move || capture_loop(app));
    true
}

#[tauri::command]
fn capture_stop() {
    CAPTURING.store(false, std::sync::atomic::Ordering::SeqCst);
    if let Some(control) = WGC_CONTROL.lock().unwrap().take() {
        let _ = control.stop();
        log("wgc session stopped");
    }
}

#[tauri::command]
fn js_guard_error(message: String) {
    log(&format!("js guard error: {message}"));
}

#[tauri::command]
fn disable_screen_share() {
    update_config(|c| {
        c.screen_share = false;
        true
    });
    log("screen share auto-disabled");
}


type WgcError = Box<dyn std::error::Error + Send + Sync>;
static WGC_CONTROL: std::sync::Mutex<Option<windows_capture::capture::CaptureControl<WgcHandler, WgcError>>> =
    std::sync::Mutex::new(None);

struct WgcHandler {
    app: AppHandle,
    frames: u64,
}

impl windows_capture::capture::GraphicsCaptureApiHandler for WgcHandler {
    type Flags = AppHandle;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: windows_capture::capture::Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { app: ctx.flags, frames: 0 })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut windows_capture::frame::Frame,
        _control: windows_capture::graphics_capture_api::InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if !CAPTURING.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        let width = frame.width();
        let height = frame.height();
        let mut buffer = frame.buffer()?;
        let data = buffer.as_nopadding_buffer()?;
        let rgb: Vec<u8> = data.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
        let mut out = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 70);
        enc.encode(&rgb, width, height, image::ExtendedColorType::Rgb8)?;
        let payload = base64::engine::general_purpose::STANDARD.encode(out);
        let _ = self.app.emit("fastdisc://frame", payload);
        self.frames += 1;
        if self.frames == 1 || self.frames % 60 == 0 {
            log(&format!("wgc frames={} {}x{}", self.frames, width, height));
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        CAPTURING.store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

fn start_wgc_capture(app: AppHandle, monitor_index: usize) -> bool {
    use windows_capture::capture::GraphicsCaptureApiHandler;
    use windows_capture::monitor::Monitor;
    use windows_capture::settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    };

    // windows-capture のモニタインデックスは 1 始まり
    let monitor = match Monitor::from_index(monitor_index + 1) {
        Ok(m) => m,
        Err(e) => {
            log(&format!("wgc monitor error: {e}"));
            return false;
        }
    };
    let settings = Settings::new(
        monitor,
        CursorCaptureSettings::WithCursor,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Custom(Duration::from_millis(16)),
        DirtyRegionSettings::Default,
        ColorFormat::Rgba8,
        app,
    );
    if let Some(control) = WGC_CONTROL.lock().unwrap().take() {
        let _ = control.stop();
        log("wgc previous session stopped");
    }
    match WgcHandler::start_free_threaded(settings) {
        Ok(control) => {
            *WGC_CONTROL.lock().unwrap() = Some(control);
            log("wgc capture started");
            true
        }
        Err(e) => {
            log(&format!("wgc error: {e}"));
            false
        }
    }
}
fn capture_loop(app: AppHandle) {
    let mut width = 1024i32;
    let mut height = 576i32;
    let mut delay = 50u64; // ~20fps target when GPU is free
    let mut last_gpu_check = std::time::Instant::now() - Duration::from_secs(30);

    while CAPTURING.load(std::sync::atomic::Ordering::SeqCst) {
        // GPU（ビデオエンコーダー）負荷が高いときは配信を弱めてゲームを優先
        if last_gpu_check.elapsed() > Duration::from_secs(10) {
            last_gpu_check = std::time::Instant::now();
            if let Some(util) = gpu_encoder_utilization() {
                if util >= 70 {
                    width = 960;
                    height = 540;
                    delay = 200; // 5fps
                } else if util <= 40 {
                    width = 1024;
                    height = 576;
                    delay = 50;
                }
                log(&format!("gpu encoder util={util}% -> {width}x{height} delay={delay}ms"));
            }
        }

        let started = std::time::Instant::now();
        if let Some(frame) = grab_screen_jpeg(width, height) {
            let payload = base64::engine::general_purpose::STANDARD.encode(frame);
            let _ = app.emit("fastdisc://frame", payload);
        }

        let cost = started.elapsed().as_millis() as u64;
        if cost > delay {
            delay = (delay * 3 / 2).min(300);
        } else if cost * 2 < delay {
            delay = (delay * 3 / 4).max(50);
        }

        std::thread::sleep(Duration::from_millis(delay));
    }
}

/// NVIDIA のビデオエンコーダー使用率（%）。取得できない環境では None。
fn gpu_encoder_utilization() -> Option<u32> {
    let mut cmd = std::process::Command::new("nvidia-smi");
    cmd.args(["--query-gpu=utilization.encoder", "--format=csv,noheader,nounits"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .next()?
        .trim()
        .parse::<u32>()
        .ok()
}

#[cfg(windows)]
fn grab_screen_jpeg(out_w: i32, out_h: i32) -> Option<Vec<u8>> {
    use windows::Win32::Graphics::Gdi::*;

    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            return None;
        }
        let width = GetDeviceCaps(Some(screen), HORZRES);
        let height = GetDeviceCaps(Some(screen), VERTRES);

        let mem = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, out_w, out_h);
        let old = SelectObject(mem, bitmap.into());
        let _ = SetStretchBltMode(mem, HALFTONE);
        let selected_window = *SELECTED_WINDOW.lock().unwrap();
        let ok = if let Some(hwnd) = selected_window {
            use windows::Win32::Foundation::HWND;
            let (sw, sh) = {
                let sel = SELECTED_SOURCE.lock().unwrap();
                match *sel { Some((_, _, w, h)) => (w, h), None => (width, height) }
            };
            let src_bmp = CreateCompatibleBitmap(screen, sw, sh);
            let src_mem = CreateCompatibleDC(Some(screen));
            let old_src = SelectObject(src_mem, src_bmp.into());
            let h = HWND(hwnd as *mut core::ffi::c_void);
            let src_dc = GetDC(Some(h));
            let printed = BitBlt(src_mem, 0, 0, sw, sh, Some(src_dc), 0, 0, SRCCOPY).is_ok();
            let _ = ReleaseDC(Some(h), src_dc);
            let ok = printed && StretchBlt(mem, 0, 0, out_w, out_h, Some(src_mem), 0, 0, sw, sh, SRCCOPY).as_bool();
            let _ = SelectObject(src_mem, old_src);
            let _ = DeleteObject(src_bmp.into());
            let _ = DeleteDC(src_mem);
            ok
        } else {
            let (sx, sy, sw, sh) = {
                let sel = SELECTED_SOURCE.lock().unwrap();
                match *sel {
                    Some((x, y, w, h)) => (x, y, w, h),
                    None => (0, 0, width, height),
                }
            };
            StretchBlt(mem, 0, 0, out_w, out_h, Some(screen), sx, sy, sw, sh, SRCCOPY).as_bool()
        };

        let mut pixels = vec![0u8; (out_w * out_h * 4) as usize];
        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = out_w;
        info.bmiHeader.biHeight = -out_h;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB.0;

        let lines = GetDIBits(
            mem,
            bitmap,
            0,
            out_h as u32,
            Some(pixels.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );

        let _ = SelectObject(mem, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(mem);
        let _ = ReleaseDC(None, screen);

        if !ok || lines == 0 {
            return None;
        }

        let rgb: Vec<u8> = pixels.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
        let mut out = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 70);
        encoder
            .encode(&rgb, out_w as u32, out_h as u32, image::ExtendedColorType::Rgb8)
            .ok()?;
        Some(out)
    }
}
// ---------------------------------------------------------------------------
// 初期設定・ログ
// ---------------------------------------------------------------------------

/// WebView2 のプロファイル保存先と起動オプション。
///
/// - `WEBVIEW2_USER_DATA_FOLDER`: ログイン状態（Cookie / localStorage）を保持
/// - `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`: 通知音の自動再生を許可
///   ※GPU は WebView2 が既定で有効（--disable-gpu などは付けない）
fn configure_webview2() {
    if std::env::var_os("WEBVIEW2_USER_DATA_FOLDER").is_none() {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let dir: PathBuf = PathBuf::from(local).join("FastDiscd");
            let _ = std::fs::create_dir_all(&dir);
            std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", &dir);
        }
    }

    if std::env::var_os("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS").is_none() {
        let mut args = String::from("--autoplay-policy=no-user-gesture-required");
        if !config().hardware_acceleration {
            args.push_str(" --disable-gpu --disable-gpu-compositing");
        }
        std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", args);
    }
}

fn log(message: &str) {
    if let Some(dir) = data_dir() {
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("fastdiscd.log"))
        {
            let _ = writeln!(file, "{message}");
        }
    }
}

use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2Controller2, COREWEBVIEW2_COLOR,
    ICoreWebView2, ICoreWebView2Profile2, ICoreWebView2_13, ICoreWebView2_19, ICoreWebView2_27,
    ICoreWebView2_3,
    COREWEBVIEW2_BROWSING_DATA_KINDS_DISK_CACHE, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW,
    COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL, COREWEBVIEW2_PERMISSION_STATE_ALLOW,
};
use webview2_com::{
    CallDevToolsProtocolMethodCompletedHandler, ClearBrowsingDataCompletedHandler, ExecuteScriptCompletedHandler,
    NavigationCompletedEventHandler, PermissionRequestedEventHandler, ScreenCaptureStartingEventHandler,
    TrySuspendCompletedHandler,
};
use windows::core::{HSTRING, Interface};
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::K32EmptyWorkingSet;
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_SET_QUOTA};
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, IsZoomed, ShowWindow, SW_MAXIMIZE,
    SW_MINIMIZE, SW_RESTORE,
};



/// WebView2 の子プロセス（レンダラ/GPU/ユーティリティ）のワーキングセットを空にして、
/// 物理メモリを OS に返す。最小化中のみ実行する。
fn trim_webview_processes() {
    use std::collections::HashMap;

    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return;
        };

        let mut entry = PROCESSENTRY32W::default();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        let mut parents: HashMap<u32, u32> = HashMap::new();
        let mut webviews: Vec<(u32, u32)> = Vec::new();

        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let name = String::from_utf16_lossy(&entry.szExeFile)
                    .trim_end_matches('\0')
                    .to_ascii_lowercase();
                let pid = entry.th32ProcessID;
                let ppid = entry.th32ParentProcessID;
                parents.insert(pid, ppid);
                if name == "msedgewebview2.exe" {
                    webviews.push((pid, ppid));
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);

        let me = std::process::id();
        let mut trimmed = 0usize;

        for (pid, mut ppid) in webviews {
            let mut depth = 0;
            while depth < 8 {
                if ppid == me {
                    if let Ok(handle) = OpenProcess(
                        PROCESS_SET_QUOTA | PROCESS_QUERY_INFORMATION,
                        false,
                        pid,
                    ) {
                        let _ = K32EmptyWorkingSet(handle);
                        let _ = CloseHandle(handle);
                        trimmed += 1;
                    }
                    break;
                }
                match parents.get(&ppid) {
                    Some(next) => {
                        ppid = *next;
                        depth += 1;
                    }
                    None => break,
                }
            }
        }

        log(&format!("trimmed {trimmed} webview processes"));
    }
}
/// Chromium にメモリ逼迫を通知し、キャッシュや GC を強制して解放させる。
fn simulate_memory_pressure(core: &ICoreWebView2, level: &str) {
    unsafe {
        let method = HSTRING::from("Memory.simulatePressureNotification");
        let params = HSTRING::from(format!("{{\"level\":\"{level}\"}}"));
        let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(|_hr, _json| {
            log("memory pressure sent");
            Ok(())
        }));
        let _ = core.CallDevToolsProtocolMethod(&method, &params, &handler);
    }
}
/// JS ヒープの GC を強制して、前のチャンネルのメッセージ等を解放させる。
fn collect_garbage(core: &ICoreWebView2) {
    unsafe {
        let method = HSTRING::from("HeapProfiler.collectGarbage");
        let params = HSTRING::from("{}");
        let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(|_hr, _json| {
            log("gc done");
            Ok(())
        }));
        let _ = core.CallDevToolsProtocolMethod(&method, &params, &handler);
    }
}

const TITLEBAR_JS: &str = r##"
(function () {
    if (window.top !== window) return;

    var MIN = "\uE921", MAX = "\uE922", CLOSE = "\uE8BB";

    function install() {
        if (!document.body || document.getElementById("rc-titlebar")) return;
        if (!window.__TAURI__ || !window.__TAURI__.window) return;
        var invoke = window.__TAURI__.core.invoke;
        var current = window.__TAURI__.window.getCurrentWindow();

        var bar = document.createElement("div");
        bar.id = "rc-titlebar";
        bar.style.cssText = "position:fixed;top:0;right:0;height:32px;width:138px;display:flex;justify-content:flex-end;z-index:2147483647;font-family:'Segoe Fluent Icons','Segoe MDL2 Assets',sans-serif;font-size:10px;color:#b5bac1;user-select:none;";

        function makeButton(glyph, hover) {
            var b = document.createElement("div");
            b.textContent = glyph;
            b.style.cssText = "width:46px;height:32px;display:flex;align-items:center;justify-content:center;cursor:default;";
            b.addEventListener("mouseenter", function () {
                b.style.background = hover;
                if (hover === "#c42b1c") b.style.color = "#fff";
            });
            b.addEventListener("mouseleave", function () {
                b.style.background = "transparent";
                b.style.color = "#b5bac1";
            });
            return b;
        }

        var min = makeButton(MIN, "rgba(255,255,255,.08)");
        var max = makeButton(MAX, "rgba(255,255,255,.08)");
        var close = makeButton(CLOSE, "#c42b1c");

        min.addEventListener("click", function () { invoke("window_minimize"); });
        max.addEventListener("click", function () { invoke("window_toggle_maximize"); });
        close.addEventListener("click", function () { invoke("window_close"); });

        bar.appendChild(min);
        bar.appendChild(max);
        bar.appendChild(close);
        document.body.appendChild(bar);

        // 右上の 受信トレイ / ヘルプ をボタン分だけ左へ寄せる
        function reserveSpace() {
            var anchor = document.querySelector('[aria-label="ヘルプ"],[aria-label="Help"],[aria-label="受信トレイ"],[aria-label="Inbox"]');
            if (!anchor) return;
            var row = anchor.parentElement;
            if (row && !row.dataset.rcReserved) {
                row.dataset.rcReserved = "1";
                row.style.paddingRight = "138px";
            }
        }
        reserveSpace();
        setTimeout(reserveSpace, 1200);
        setTimeout(reserveSpace, 3500);

        // 上部 32px の余白はウィンドウドラッグ（Discord の操作要素は除外）
        document.addEventListener("mousedown", function (e) {
            if (e.button !== 0 || e.clientY > 32) return;
            if (e.target.closest("a,button,input,textarea,select,[role='button'],[role='link'],[contenteditable='true'],#rc-titlebar")) return;
            try { current.startDragging(); } catch (err) { }
        }, true);

        document.addEventListener("dblclick", function (e) {
            if (e.clientY > 32) return;
            if (e.target.closest("a,button,input,textarea,select,[role='button'],[role='link'],[contenteditable='true'],#rc-titlebar")) return;
            invoke("window_toggle_maximize");
        }, true);



    }

    if (document.readyState === "loading") {
        document.addEventListener("DOMContentLoaded", install, { once: true });
    } else {
        install();
    }
})();
"##;
const SHARE_JS: &str = r##"
(function () {
    if (window.top !== window) return;
    var tauri = window.__TAURI__;
    if (!tauri || !tauri.core) return;
    var invoke = tauri.core.invoke;

    var errors = 0;
    function guard(message) {
        errors++;
        try { invoke("js_guard_error", { message: String(message).slice(0, 400) }); } catch (e) { }
        if (errors >= 3) {
            try { delete window.DiscordNative; } catch (e) { }
            try { invoke("disable_screen_share"); } catch (e) { }
        }
    }
    window.addEventListener("error", function (e) {
        var detail = (e.error && e.error.stack) ? String(e.error.stack).slice(0, 700) : (e.message || "error");
        guard(detail);
    });
    window.addEventListener("unhandledrejection", function (e) {
        guard((e.reason && e.reason.message) || "rejection");
    });

    function installCapture() {
        if (window.__fastdiscCapture) return;
        window.__fastdiscCapture = true;

        var canvas = document.createElement("canvas");
        canvas.width = 1280;
        canvas.height = 720;
        var ctx = canvas.getContext("2d");
        var listening = false;

        function ensure() {
            if (listening) return Promise.resolve();
            listening = true;
            return tauri.event.listen("fastdisc://frame", function (event) {
                var img = new Image();
                img.onload = function () { ctx.drawImage(img, 0, 0, canvas.width, canvas.height); };
                img.src = "data:image/jpeg;base64," + event.payload;
            }).then(function () { return invoke("capture_start"); });
        }

        var proto = Object.getPrototypeOf(navigator.mediaDevices);
        try {
            Object.defineProperty(proto, "getDisplayMedia", {
                configurable: true,
                value: function () {
                    try { invoke("js_guard_error", { message: "getDisplayMedia called" }); } catch (e) { }
                    return invoke("list_sources").then(function (sources) {
                        if (!sources || sources.length <= 1) return;
                        try { invoke("reset_selection"); } catch (e) { }
                        return new Promise(function (resolve) {
                            try { invoke("js_guard_error", { message: "showing picker n=" + sources.length }); } catch (e) { }
                            invoke("show_source_picker").catch(function (e) {
                                try { invoke("js_guard_error", { message: "picker invoke failed: " + e }); } catch (e2) { }
                            });
                            var done = false;
                            var finish = function (ok) { if (done) return; done = true; resolve(ok); };
                            try {
                                var ev = window.__TAURI__ && window.__TAURI__.event;
                                if (ev && ev.listen) {
                                    ev.listen("fastdisc://source-selected", function () { finish(true); });
                                    ev.listen("fastdisc://source-cancelled", function () { finish(false); });
                                }
                            } catch (e) { }
                            var timer = setInterval(function () {
                                invoke("selection_state").then(function (ok) {
                                    if (ok) { clearInterval(timer); finish(true); }
                                }).catch(function () { });
                            }, 400);
                            setTimeout(function () { clearInterval(timer); finish(false); }, 180000);
                        });
                    }).then(function (picked) {
                        if (picked === false) { throw new Error("capture cancelled"); }
                        return ensure();
                    }).then(function () {
                    var stream = canvas.captureStream(30);
                        var track = stream.getVideoTracks()[0];
                        if (track) {
                            try { track.contentHint = "detail"; } catch (e) { }
                            track.addEventListener("ended", function () { invoke("capture_stop"); });
                        }
                        return stream;
                    });
                }
            });
        } catch (e) { guard(e); }

        try {
            var origQuery = navigator.permissions.query.bind(navigator.permissions);
            navigator.permissions.query = function (desc) {
                if (desc && desc.name === "display-capture") {
                    return Promise.resolve({ state: "granted", onchange: null });
                }
                return origQuery(desc);
            };
        } catch (e) { }
    }

    invoke("get_settings").then(function (s) {
        window.__fastdiscSettings = s;
        if (!s.screenShare) return;
        if ((s.screenShareMode || "rust") !== "rust") return;
        installCapture();
    }).catch(function () { });
})();
"##;