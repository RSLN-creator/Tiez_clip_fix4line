#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

pub mod app;
pub mod app_state;
pub mod database;
pub mod domain;
pub mod error;
pub mod global_state;
pub mod infrastructure;
pub mod logger;
pub mod migration;
pub mod services;

use crate::app::setup;
use crate::global_state::*;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set as soon as this app's own Tauri `setup` hook starts running.
///
/// Tauri creates the windows declared in `tauri.conf.json` *before* that hook, and the
/// main window is a WebView2 window. If WebView2 environment creation stalls there, the
/// process stays alive forever having done nothing observable - no window, no tray icon,
/// no `tiez.log` line - which is exactly what "I clicked it and nothing happens" looks
/// like. The watchdog below turns that silence into a message and a report file.
pub static APP_SETUP_REACHED: AtomicBool = AtomicBool::new(false);

/// Names of the diagnostic files written next to the executable.
const STARTUP_TIMEOUT_LOG: &str = "tiez-startup-timeout.log";

fn report_dir_log(name: &str) -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(name)))
}

/// Writes a panic report next to the executable (and to %TEMP% as a fallback).
///
/// Why this exists: the release build is a `windows_subsystem = "windows"` binary with
/// `panic = "abort"`, and Tauri turns a failed `App::setup()` - which includes creating
/// the configured windows, *before* any of this app's own startup code runs - into
/// `panic!("Failed to setup app: {e}")`. Without this hook that failure terminates the
/// process with no window, no tray icon and not a single line in `tiez.log`, which is
/// indistinguishable from "the exe does nothing when I click it".
fn install_crash_reporter() {
    let exe_dir_log = report_dir_log("tiez-crash.log");

    std::panic::set_hook(Box::new(move |info| {
        let msg = format!("TieZ startup panic:\n{info}\n");
        if let Some(path) = &exe_dir_log {
            let _ = std::fs::write(path, &msg);
        }
        let _ = std::fs::write(std::env::temp_dir().join("tiez-crash.log"), &msg);
    }));
}

/// If the app's own setup hook has not started after `SECS`, the webview never came up.
/// Write a report and tell the user instead of leaving a hung, invisible process behind.
fn install_startup_watchdog() {
    const SECS: u64 = 20;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(SECS));
        if APP_SETUP_REACHED.load(Ordering::SeqCst) {
            return;
        }

        let msg = format!(
            "TieZ did not finish starting up within {} seconds.\n\
             \n\
             The Windows WebView2 window could not be created, so the app hung before\n\
             its own startup code ran. Typical causes: a stuck WebView2 runtime state,\n\
             or a broken WebView2 user-data folder.\n\
             \n\
             Suggested fix:\n\
             1. End any leftover tiez-app / tiez-app-fixed process in Task Manager.\n\
             2. Rename or delete: %LOCALAPPDATA%\\com.tiez.app\\EBWebView\n\
             3. Try again. If it still hangs, reboot (this clears stuck WebView2 state),\n\
                then repair the WebView2 Runtime from Microsoft.\n",
            SECS
        );

        if let Some(path) = report_dir_log(STARTUP_TIMEOUT_LOG) {
            let _ = std::fs::write(path, &msg);
        }
        let _ = std::fs::write(std::env::temp_dir().join(STARTUP_TIMEOUT_LOG), &msg);

        #[cfg(windows)]
        crate::infrastructure::windows_ext::WindowExt::show_error_box(
            "TieZ 启动超时",
            &msg,
        );
    });
}

fn main() {
    install_crash_reporter();
    install_startup_watchdog();

    // 显式安装 rustls 的 crypto provider，防止 rumqttc 因缺少 provider 而 panic
    let _ = rustls::crypto::ring::default_provider().install_default();

    let _ = dotenvy::dotenv();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_sql::Builder::default().build())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                        setup::handle_global_shortcut(app, shortcut);
                    }
                })
                .build(),
        )
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_single_instance::init(|_app, _args, _cwd| {}))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        // NOTE: `tauri-plugin-http` used to be registered here. It is dead code in this
        // app - nothing in `src/` references `@tauri-apps/plugin-http`, and the
        // capability set in `tauri.conf.json` grants no `http:*` permission - yet its
        // `setup` hook hard-fails the whole application when it cannot create its cookie
        // store: `create_dir_all(app_cache_dir())` then opening
        // `%LOCALAPPDATA%\<identifier>\.cookies` for append. Any environment where that
        // write is denied (locked-down profile, hardened policy, restricted token) made
        // TieZ exit silently before it even wrote a log line. Removing the registration
        // costs nothing and removes that startup failure mode.
        .setup(|app| {
            // Reached means the configured webview window was created successfully, so
            // the watchdog can stand down.
            APP_SETUP_REACHED.store(true, Ordering::SeqCst);
            setup::init(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app::window_manager::toggle_window_cmd,
            app::window_manager::hide_window_cmd,
            app::window_manager::activate_window_focus,
            app::window_manager::focus_clipboard_window,
            app::window_manager::set_navigation_enabled,
            app::window_manager::set_navigation_mode,
            app::hooks::set_recording_mode,
            services::content_handler::open_content,
            services::clipboard_ops::copy_to_clipboard,
            services::clipboard_ops::paste_latest_rich,
            app::commands::get_clipboard_history,
            app::commands::search_clipboard_history,
            app::commands::delete_clipboard_entry,
            app::commands::clear_clipboard_history,
            app::commands::get_tag_items,
            app::commands::get_all_tags_info,
            app::commands::rename_tag_globally,
            app::commands::delete_tag_from_all,
            app::commands::create_new_tag,
            app::commands::update_pinned_order,
            app::commands::get_db_count,
            app::commands::get_clipboard_content,
            app::commands::set_sequential_mode,
            app::commands::set_sequential_hotkey,
            app::commands::set_rich_paste_hotkey,
            app::commands::set_search_hotkey,
            app::commands::set_deduplication,
            app::commands::save_setting,
            app::commands::set_ignore_blur,
            app::commands::set_window_pinned,
            app::commands::get_settings,
            app::commands::set_file_server_auto_close,
            app::commands::set_persistence,
            app::commands::set_capture_files,
            app::commands::set_capture_rich_text,
            app::commands::set_auto_copy_file,
            app::commands::set_silent_start,
            app::commands::set_delete_after_paste,
            app::commands::set_privacy_protection,
            app::commands::set_privacy_protection_kinds,
            app::commands::set_privacy_protection_custom_rules,
            app::commands::set_cleanup_rules,
            app::commands::set_app_cleanup_policies,
            app::commands::reset_settings,
            app::commands::get_mqtt_status,
            app::commands::get_mqtt_running,
            app::commands::restart_mqtt_client,
            app::commands::get_cloud_sync_status,
            app::commands::restart_cloud_sync_client,
            app::commands::request_cloud_sync,
            app::commands::cloud_sync_now,
            app::commands::set_sound_enabled,
            app::commands::set_file_transfer_auto_open,
            app::commands::set_arrow_key_selection,
            app::commands::set_tray_visible,
            app::commands::set_edge_docking,
            app::commands::set_follow_mouse,
            app::commands::get_data_path,
            app::commands::open_folder,
            app::commands::open_data_folder,
            app::commands::open_file_with_default_app,
            app::commands::open_file_location,
            app::commands::set_data_path,
            app::commands::toggle_autostart,
            app::commands::is_autostart_enabled,
            app::commands::set_windows_clipboard_history,
            app::commands::get_windows_clipboard_history,
            app::commands::set_win_clipboard_disabled,
            app::commands::trigger_registry_win_v_optimization,
            app::commands::is_registry_win_v_optimized,
            app::commands::restart_explorer,
            app::commands::restart_as_admin,
            app::commands::check_is_admin,
            app::commands::quit,
            app::commands::relaunch,
            app::commands::set_theme,
            app::commands::get_platform_info,
            app::commands::send_system_notification,
            app::commands::register_hotkey,
            app::commands::test_hotkey_available,
            app::commands::toggle_clipboard_pin,
            app::commands::update_tags,
            app::commands::add_manual_item,
            app::commands::update_item_content,
            app::commands::save_emoji_favorite,
            app::commands::remove_emoji_favorite,
            app::commands::list_emoji_favorites,
            app::commands::save_emoji_favorite_data_url,
            app::commands::save_emoji_favorite_url,
            app::commands::get_file_size,
            app::commands::save_file_copy,
            services::clipboard_ops::paste_text_directly,
            services::clipboard_ops::paste_content_transiently,
            services::file_transfer::send_chat_message,
            services::file_transfer::get_chat_history,
            services::file_transfer::send_file_to_client,
            services::file_transfer::get_app_logo,
            services::file_transfer::get_local_ip_addr,
            services::file_transfer::get_available_ips,
            services::file_transfer::get_file_server_status,
            services::file_transfer::toggle_file_server,
            services::file_transfer::get_active_file_transfer_path,
            services::paste_queue::get_paste_queue,
            services::paste_queue::set_paste_queue,
            services::paste_queue::paste_next_step,
            app::commands::get_tag_colors,
            app::commands::set_tag_color,
            app::commands::call_ai,
            app::commands::check_ai_connectivity,
            infrastructure::windows_api::apps::get_system_default_app,
            infrastructure::windows_api::apps::get_executable_icon,
            infrastructure::windows_api::apps::get_file_icon,
            infrastructure::windows_api::apps::scan_installed_apps,
            infrastructure::windows_api::apps::get_associated_apps
        ])
        .on_window_event(|window, event| {
            setup::handle_window_event(window, event);
        })
        .build(tauri::generate_context!());

    match app {
        Ok(app) => {
            info!(">>> [STARTUP] Tauri app built successfully.");
            app.run(|_app_handle, _event| {});
        }
        Err(e) => {
            error!(">>> [STARTUP] Failed to build tauri app: {}", e);
        }
    }

    // Cleanup Hooks on exit
    unsafe {
        let h_hook = HOOK_HANDLE.swap(std::ptr::null_mut(), Ordering::SeqCst);
        if !h_hook.is_null() {
            let _ = windows::Win32::UI::WindowsAndMessaging::UnhookWindowsHookEx(
                windows::Win32::UI::WindowsAndMessaging::HHOOK(h_hook as _),
            );
        }
        let h_mouse = HOOK_MOUSE_HANDLE.swap(std::ptr::null_mut(), Ordering::SeqCst);
        if !h_mouse.is_null() {
            let _ = windows::Win32::UI::WindowsAndMessaging::UnhookWindowsHookEx(
                windows::Win32::UI::WindowsAndMessaging::HHOOK(h_mouse as _),
            );
        }
    }
}
