use edge_keymap::{WindowsScanCode, windows_scancode_to_evdev};

#[derive(Debug, thiserror::Error)]
pub enum WindowsInputError {
    #[error("Windows input capture is only available on Windows")]
    UnsupportedPlatform,
    #[error("unmapped Windows scan code {scan_code:#x}, extended={extended}")]
    UnmappedKey { scan_code: u16, extended: bool },
    #[error("Windows tray error: {0}")]
    Tray(String),
}

pub type Result<T> = std::result::Result<T, WindowsInputError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlState {
    LocalActive,
    RemoteActive,
}

pub fn map_key(scan_code: u16, extended: bool) -> Result<u16> {
    windows_scancode_to_evdev(WindowsScanCode {
        scan_code,
        extended,
    })
    .ok_or(WindowsInputError::UnmappedKey {
        scan_code,
        extended,
    })
}

#[cfg(windows)]
pub fn install_hooks() -> Result<()> {
    tracing::info!("Windows hook installation placeholder");
    Ok(())
}

#[cfg(not(windows))]
pub fn install_hooks() -> Result<()> {
    Err(WindowsInputError::UnsupportedPlatform)
}

#[cfg(windows)]
pub fn run_tray(status: &str) -> Result<()> {
    tray::run(status)
}

#[cfg(not(windows))]
pub fn run_tray(_status: &str) -> Result<()> {
    Err(WindowsInputError::UnsupportedPlatform)
}

#[cfg(windows)]
mod tray {
    use std::{ffi::c_void, mem::size_of, ptr::null_mut, sync::OnceLock};

    use windows_sys::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{
                NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
                Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                AppendMenuW, CW_USEDEFAULT, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
                DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
                IDI_APPLICATION, LoadIconW, MF_SEPARATOR, MF_STRING, MSG, PostQuitMessage,
                RegisterClassW, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
                TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP, WM_COMMAND, WM_DESTROY,
                WM_LBUTTONDBLCLK, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPEDWINDOW,
            },
        },
    };

    use crate::{Result, WindowsInputError};

    const TRAY_ID: u32 = 1;
    const WM_TRAY_ICON: u32 = WM_APP + 1;
    const ID_RELEASE: usize = 1001;
    const ID_QUIT: usize = 1002;

    static TRAY_STATUS: OnceLock<Vec<u16>> = OnceLock::new();

    pub fn run(status: &str) -> Result<()> {
        unsafe {
            let _ = TRAY_STATUS.set(to_wide(status));

            let instance = GetModuleHandleW(null_mut());
            if instance.is_null() {
                return Err(WindowsInputError::Tray(
                    "GetModuleHandleW failed".to_string(),
                ));
            }

            let class_name = to_wide("EdgeKvmTrayWindow");
            let window_name = to_wide("edge-kvm");
            let window_class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                hIcon: LoadIconW(null_mut(), IDI_APPLICATION),
                lpszClassName: class_name.as_ptr(),
                ..Default::default()
            };

            if RegisterClassW(&window_class) == 0 {
                return Err(WindowsInputError::Tray("RegisterClassW failed".to_string()));
            }

            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                window_name.as_ptr(),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                null_mut(),
                null_mut(),
                instance,
                null_mut::<c_void>(),
            );
            if hwnd.is_null() {
                return Err(WindowsInputError::Tray(
                    "CreateWindowExW failed".to_string(),
                ));
            }

            add_tray_icon(hwnd, status)?;

            let mut message = MSG::default();
            while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }

            Ok(())
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            match message {
                WM_TRAY_ICON => {
                    if lparam as u32 == WM_RBUTTONUP || lparam as u32 == WM_LBUTTONDBLCLK {
                        show_menu(hwnd);
                    }
                    0
                }
                WM_COMMAND => {
                    match wparam & 0xffff {
                        ID_RELEASE => {
                            tracing::info!("release requested from tray");
                        }
                        ID_QUIT => {
                            remove_tray_icon(hwnd);
                            DestroyWindow(hwnd);
                        }
                        _ => {}
                    }
                    0
                }
                WM_DESTROY => {
                    remove_tray_icon(hwnd);
                    PostQuitMessage(0);
                    0
                }
                _ => DefWindowProcW(hwnd, message, wparam, lparam),
            }
        }
    }

    fn add_tray_icon(hwnd: HWND, status: &str) -> Result<()> {
        let mut data = notify_icon_data(hwnd);
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = WM_TRAY_ICON;
        data.hIcon = unsafe { LoadIconW(null_mut(), IDI_APPLICATION) };
        copy_wide("edge-kvm", status, &mut data.szTip);

        if unsafe { Shell_NotifyIconW(NIM_ADD, &data) } == 0 {
            return Err(WindowsInputError::Tray(
                "Shell_NotifyIconW(NIM_ADD) failed".to_string(),
            ));
        }
        Ok(())
    }

    fn remove_tray_icon(hwnd: HWND) {
        let data = notify_icon_data(hwnd);
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &data);
        }
    }

    fn show_menu(hwnd: HWND) {
        let menu = unsafe { CreatePopupMenu() };
        if menu.is_null() {
            return;
        }

        let status = TRAY_STATUS
            .get()
            .map(|value| value.as_ptr())
            .unwrap_or_else(|| to_wide("edge-kvm").leak().as_ptr());
        let release = to_wide("Release control");
        let quit = to_wide("Quit");

        unsafe {
            AppendMenuW(menu, MF_STRING, 0, status);
            AppendMenuW(menu, MF_SEPARATOR, 0, null_mut());
            AppendMenuW(menu, MF_STRING, ID_RELEASE, release.as_ptr());
            AppendMenuW(menu, MF_STRING, ID_QUIT, quit.as_ptr());

            let mut point = POINT::default();
            if GetCursorPos(&mut point) != 0 {
                SetForegroundWindow(hwnd);
                TrackPopupMenu(
                    menu,
                    TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON,
                    point.x,
                    point.y,
                    0,
                    hwnd,
                    null_mut(),
                );
            }

            DestroyMenu(menu);
        }
    }

    fn notify_icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: TRAY_ID,
            ..Default::default()
        }
    }

    fn copy_wide(prefix: &str, status: &str, target: &mut [u16]) {
        let text = format!("{prefix}: {status}");
        let wide = to_wide(&text);
        let count = wide
            .len()
            .saturating_sub(1)
            .min(target.len().saturating_sub(1));
        target[..count].copy_from_slice(&wide[..count]);
        target[count] = 0;
    }

    fn to_wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }
}
