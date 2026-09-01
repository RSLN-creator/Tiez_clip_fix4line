use std::sync::atomic::{AtomicIsize, Ordering};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::Threading::AttachThreadInput;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CallNextHookEx, GA_ROOT, GetAncestor, GetForegroundWindow, GetWindowRect,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, LLMHF_INJECTED, MessageBoxW,
    MSLLHOOKSTRUCT, PostMessageW, SetForegroundWindow, SetWindowPos, SetWindowsHookExW,
    ShowWindow, WindowFromPoint, HWND_TOPMOST, MB_ICONERROR, MB_OK, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_SHOWWINDOW, SW_RESTORE, SW_SHOWNA, WH_MOUSE_LL, WM_MOUSEWHEEL,
};

/// 安全封装的窗口辅助工具
pub struct WindowExt;

impl WindowExt {
    /// 获取当前前台窗口句柄
    pub fn get_foreground_window() -> HWND {
        unsafe { GetForegroundWindow() }
    }

    /// 检查窗口是否可见
    pub fn is_window_visible(hwnd: HWND) -> bool {
        unsafe { IsWindowVisible(hwnd).as_bool() }
    }

    /// 获取窗口矩形区域
    pub fn get_window_rect(hwnd: HWND) -> Option<RECT> {
        let mut rect = RECT::default();
        unsafe {
            if GetWindowRect(hwnd, &mut rect).is_ok() {
                Some(rect)
            } else {
                None
            }
        }
    }

    /// 释放 Windows 键（防止开始菜单弹出）
    pub fn release_win_keys() {
        unsafe {
            let dummy_vk = VIRTUAL_KEY(0xFF);
            let inputs = [
                Self::create_key_input(dummy_vk, false),
                Self::create_key_input(dummy_vk, true),
                Self::create_key_input(VK_LWIN, true),
                Self::create_key_input(VK_RWIN, true),
            ];
            SendInput(&inputs, core::mem::size_of::<INPUT>() as i32);
        }
    }

    /// 强力恢复窗口焦点（处理跨线程输入附加）
    pub fn force_focus_window(hwnd: HWND) {
        if hwnd.0.is_null() {
            return;
        }

        unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                return;
            }
            let should_restore = IsIconic(hwnd).as_bool();

            let fg_hwnd = GetForegroundWindow();
            if fg_hwnd != hwnd {
                let fg_thread_id = GetWindowThreadProcessId(fg_hwnd, None);
                let target_thread_id = GetWindowThreadProcessId(hwnd, None);

                if fg_thread_id != 0 && target_thread_id != 0 && fg_thread_id != target_thread_id {
                    let _ = AttachThreadInput(fg_thread_id, target_thread_id, true);
                    let _ = SetForegroundWindow(hwnd);
                    if should_restore {
                        let _ = ShowWindow(hwnd, SW_RESTORE);
                    }
                    let _ = BringWindowToTop(hwnd);
                    let _ = AttachThreadInput(fg_thread_id, target_thread_id, false);
                } else {
                    let _ = SetForegroundWindow(hwnd);
                    if should_restore {
                        let _ = ShowWindow(hwnd, SW_RESTORE);
                    }
                    let _ = BringWindowToTop(hwnd);
                }
            }
        }
    }

    /// 无感显示置顶窗口（不夺取焦点）
    pub fn show_window_no_activate(hwnd: HWND) {
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNA);
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW | SWP_NOACTIVATE,
            );
        }
    }

    /// 无激活显示普通窗口（不置顶）
    pub fn show_window_no_activate_normal(hwnd: HWND) {
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNA);
            // Bring to front without activation by temporarily toggling TOPMOST.
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW | SWP_NOACTIVATE,
            );
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW | SWP_NOACTIVATE,
            );
        }
    }

    /// 弹出错误消息框
    pub fn show_error_box(title: &str, msg: &str) {
        use windows::core::PCWSTR;
        let title_w: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let msg_w: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();

        unsafe {
            let _ = MessageBoxW(
                None,
                PCWSTR(msg_w.as_ptr()),
                PCWSTR(title_w.as_ptr()),
                MB_ICONERROR | MB_OK,
            );
        }
    }

    fn create_key_input(vk: VIRTUAL_KEY, is_up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    dwFlags: if is_up {
                        KEYEVENTF_KEYUP
                    } else {
                        windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS(0)
                    },
                    ..Default::default()
                },
            },
        }
    }
}

// ============ 无焦点窗口滚轮转发 ============
// 主窗口以 WS_EX_NOACTIVATE + SW_SHOWNA 呼出时不持有键盘焦点，Windows 会把
// WM_MOUSEWHEEL 投递给焦点窗口而非光标下窗口，导致鼠标滚轮失效或穿透到底下应用。
// WH_MOUSE_LL 钩子在系统路由前拦截：光标命中主窗口树时，把滚轮消息直接投递给
// 命中的 WebView2 子窗口并吞掉。触摸板走 WM_POINTERWHEEL（按光标位置投递），不受影响。

static WHEEL_MOUSE_HOOK: AtomicIsize = AtomicIsize::new(0);
static WHEEL_MAIN_HWND: AtomicIsize = AtomicIsize::new(0);

/// 安装低级鼠标滚轮钩子（进程生命周期内有效，进程退出时由系统回收）
pub fn install_wheel_forward_hook(main_hwnd: isize) {
    if WHEEL_MOUSE_HOOK.load(Ordering::Relaxed) != 0 {
        return;
    }
    WHEEL_MAIN_HWND.store(main_hwnd, Ordering::Relaxed);
    unsafe {
        if let Ok(hook) = SetWindowsHookExW(WH_MOUSE_LL, Some(wheel_ll_proc), None, 0) {
            WHEEL_MOUSE_HOOK.store(hook.0 as usize as isize, Ordering::Relaxed);
        }
    }
}

unsafe extern "system" fn wheel_ll_proc(n_code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    if n_code >= 0 && w_param.0 as u32 == WM_MOUSEWHEEL {
        let info = &*(l_param.0 as *const MSLLHOOKSTRUCT);
        // 过滤注入事件，避免吞掉 AutoHotkey 等工具注入的滚轮（PostMessage 自身不经过本钩子）
        if info.flags & LLMHF_INJECTED == 0 {
            let hit = WindowFromPoint(info.pt);
            if GetAncestor(hit, GA_ROOT).0 as isize == WHEEL_MAIN_HWND.load(Ordering::Relaxed) {
                // wParam：高 16 位 = 带符号 wheel delta，低 16 位 = MK_* 修饰键
                let delta = ((info.mouseData >> 16) as u16 as usize) << 16;
                let mut mods = 0usize;
                if GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000 != 0 {
                    mods |= 0x8; // MK_CONTROL
                }
                if GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000 != 0 {
                    mods |= 0x4; // MK_SHIFT
                }
                // lParam：屏幕坐标，x 低位 / y 高位（i16 保位，兼容负坐标显示器）
                let l_fwd = ((info.pt.y as i16 as u16 as usize) << 16)
                    | info.pt.x as i16 as u16 as usize;
                let _ = PostMessageW(
                    Some(hit),
                    WM_MOUSEWHEEL,
                    WPARAM(delta | mods),
                    LPARAM(l_fwd as isize),
                );
                return LRESULT(1); // 吞掉，不再投递给前台焦点窗口
            }
        }
    }
    CallNextHookEx(None, n_code, w_param, l_param)
}
