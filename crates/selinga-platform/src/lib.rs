#[cfg(windows)]
fn hide_from_taskbar(hwnd: windows::Win32::Foundation::HWND) {
    use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
    use windows::Win32::UI::Shell::{ITaskbarList, TaskbarList};

    unsafe {
        let Ok(list) = CoCreateInstance::<_, ITaskbarList>(&TaskbarList, None, CLSCTX_ALL) else {
            return;
        };
        let _ = list.HrInit();
        let _ = list.DeleteTab(hwnd);
    }
}

#[cfg(windows)]
fn hide_from_alt_tab(hwnd: windows::Win32::Foundation::HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GWLP_HWNDPARENT, GetClassNameW, GetWindowLongPtrW, SetWindowLongPtrW,
        WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    };

    unsafe {
        let mut class = [0u16; 64];
        let length = GetClassNameW(hwnd, &mut class);
        if length <= 0 {
            return;
        }
        let name = String::from_utf16_lossy(&class[..length as usize]).to_ascii_lowercase();
        if !(name.contains("winit") || name == "window class") {
            return;
        }

        let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let desired = (current & !WS_EX_APPWINDOW.0) | WS_EX_TOOLWINDOW.0;
        if current != desired {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, desired as isize);
        }
        if let Some(owner) = alt_tab_owner() {
            SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, owner.0 as isize);
        }
    }
}

#[cfg(windows)]
fn alt_tab_owner() -> Option<windows::Win32::Foundation::HWND> {
    use std::sync::OnceLock;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
    };
    use windows::core::PCWSTR;

    static OWNER: OnceLock<usize> = OnceLock::new();
    let raw = *OWNER.get_or_init(|| unsafe {
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        let title: Vec<u16> = "SelingaOwner\0".encode_utf16().collect();
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
        .map_or(0, |hwnd| hwnd.0 as usize)
    });
    if raw == 0 {
        None
    } else {
        Some(HWND(raw as *mut core::ffi::c_void))
    }
}

pub fn exclude_self_from_capture() -> usize {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{HWND, LPARAM};
        use windows::Win32::System::Threading::GetCurrentProcessId;
        use windows::Win32::UI::WindowsAndMessaging::{
            EnumWindows, GetWindowThreadProcessId, SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE,
        };

        unsafe extern "system" fn on_window(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
            unsafe {
                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                if pid == GetCurrentProcessId() {
                    let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
                    hide_from_taskbar(hwnd);
                    hide_from_alt_tab(hwnd);
                    let counter = lparam.0 as *mut usize;
                    *counter += 1;
                }
                true.into()
            }
        }

        unsafe {
            let mut count = 0usize;
            let lparam = LPARAM(&mut count as *mut usize as isize);
            let _ = EnumWindows(Some(on_window), lparam);
            count
        }
    }

    #[cfg(not(windows))]
    {
        0
    }
}

pub fn set_window_geometry_px(title: &str, x: i32, y: i32, w: i32, h: i32) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            FindWindowW, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER, SetWindowPos,
        };
        let mut wide: Vec<u16> = title.encode_utf16().collect();
        wide.push(0);
        let Ok(hwnd) = FindWindowW(
            windows::core::PCWSTR::null(),
            windows::core::PCWSTR(wide.as_ptr()),
        ) else {
            return false;
        };
        if hwnd == HWND::default() {
            return false;
        }
        SetWindowPos(
            hwnd,
            Some(HWND::default()),
            x,
            y,
            w.max(1),
            h.max(1),
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        )
        .is_ok()
    }

    #[cfg(not(windows))]
    {
        let _ = (title, x, y, w, h);
        false
    }
}

pub fn open_in_editor(path: &std::path::Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        match shell_open(path) {
            Ok(()) => Ok(()),
            Err(shell_error) => {
                if let Some(editor) = std::env::var_os("SELINGA_EDITOR").filter(|v| !v.is_empty()) {
                    return spawn(&editor, path).map_err(|e| format!("{shell_error}; {e}"));
                }
                spawn(std::ffi::OsStr::new("notepad.exe"), path)
                    .map_err(|e| format!("{shell_error}; notepad fallback: {e}"))
            }
        }
    }

    #[cfg(not(windows))]
    {
        let _ = path;
        Err("open_in_editor is Windows-only".to_owned())
    }
}

#[cfg(windows)]
fn shell_open(path: &std::path::Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::PCWSTR;

    let file: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let operation: Vec<u16> = "open\0".encode_utf16().collect();
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(operation.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = result.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(format!("ShellExecuteW returned {code}"))
    }
}

#[cfg(windows)]
fn spawn(program: &std::ffi::OsStr, path: &std::path::Path) -> Result<(), String> {
    std::process::Command::new(program)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_child| ())
        .map_err(|e| format!("cannot start {}: {e}", program.to_string_lossy()))
}

pub fn single_instance() -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
        use windows::Win32::System::Threading::CreateMutexW;
        use windows::core::PCWSTR;

        let name: Vec<u16> = "Local\\selinga-single-instance\0".encode_utf16().collect();
        let Ok(handle) = CreateMutexW(None, true, PCWSTR(name.as_ptr())) else {
            return true;
        };
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(handle);
            // 顺序很重要：插件实例建了「重新框选」事件，信号被接住时不该再去抢焦点；
            // 只有没人接信号（独立版）才退回老行为 —— 把旧窗口拎到前台。
            if !request_reselect() {
                activate_existing();
            }
            return false;
        }
        let _ = handle;
        true
    }

    #[cfg(not(windows))]
    {
        true
    }
}

/// 插件版「重新框选」的命名事件。
///
/// 为什么用内核事件而不是窗口消息：宿主重复触发扇区时会拉起**第二份**进程，
/// 它只活几毫秒、没有窗口可以收消息；命名事件是最便宜的跨进程信令，
/// `Local\` 前缀保证只在当前登录会话内可见，不与其它会话串扰。
pub const RESELECT_EVENT_NAME: &str = "Local\\selinga-plugin-reselect";

/// 已运行实例持有的监听端。只有插件模式才创建 —— 独立版的重复启动不该被改行为。
pub struct ReselectListener {
    #[cfg(windows)]
    handle: windows::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
unsafe impl Send for ReselectListener {}

impl ReselectListener {
    #[cfg(windows)]
    pub fn create() -> Option<Self> {
        use windows::Win32::System::Threading::CreateEventW;
        use windows::core::PCWSTR;

        let name: Vec<u16> = RESELECT_EVENT_NAME.encode_utf16().chain(Some(0)).collect();
        unsafe {
            CreateEventW(None, false, false, PCWSTR(name.as_ptr()))
                .ok()
                .map(|handle| Self { handle })
        }
    }

    #[cfg(not(windows))]
    pub fn create() -> Option<Self> {
        None
    }

    /// 非阻塞查询并消费信号（事件是自动复位的，读到即清除）。
    #[cfg(windows)]
    pub fn signaled(&self) -> bool {
        use windows::Win32::Foundation::WAIT_OBJECT_0;
        use windows::Win32::System::Threading::WaitForSingleObject;
        unsafe { WaitForSingleObject(self.handle, 0) == WAIT_OBJECT_0 }
    }

    #[cfg(not(windows))]
    pub fn signaled(&self) -> bool {
        false
    }
}

/// 让已运行的插件实例重新进入框选。返回信号是否真的递送到了。
#[cfg(windows)]
pub fn request_reselect() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};
    use windows::core::PCWSTR;

    let name: Vec<u16> = RESELECT_EVENT_NAME.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let Ok(handle) = OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name.as_ptr())) else {
            return false;
        };
        let delivered = SetEvent(handle).is_ok();
        let _ = CloseHandle(handle);
        delivered
    }
}

#[cfg(not(windows))]
pub fn request_reselect() -> bool {
    false
}

/// 把 Selinga 自己的窗口拉到前台。
/// 插件版专用：宿主轮盘刚关闭时前台窗口可能还是宿主，光靠 `ViewportCommand::Focus` 不一定生效。
pub fn force_foreground() {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{
            BringWindowToTop, FindWindowW, SetForegroundWindow,
        };
        use windows::core::PCWSTR;

        for title in ["Selinga", "Selinga Box"] {
            let wide: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
            if let Ok(hwnd) = FindWindowW(PCWSTR::null(), PCWSTR(wide.as_ptr()))
                && !hwnd.is_invalid()
            {
                let _ = SetForegroundWindow(hwnd);
                let _ = BringWindowToTop(hwnd);
                return;
            }
        }
    }
}

#[cfg(windows)]
fn activate_existing() {
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SW_RESTORE, SetForegroundWindow, ShowWindow,
    };
    use windows::core::PCWSTR;

    for title in ["Selinga Box", "Selinga"] {
        let wide: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        if let Ok(hwnd) = unsafe { FindWindowW(PCWSTR::null(), PCWSTR(wide.as_ptr())) }
            && !hwnd.is_invalid()
        {
            unsafe {
                let _ = ShowWindow(hwnd, SW_RESTORE);
                let _ = SetForegroundWindow(hwnd);
            }
            return;
        }
    }
}

pub fn set_autostart(enabled: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
        use windows::Win32::System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegDeleteValueW,
            RegOpenKeyExW, RegSetValueExW,
        };
        use windows::core::PCWSTR;

        const KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
        const VALUE: &str = "Selinga";

        let payload = if enabled {
            let exe = std::env::current_exe().map_err(|e| format!("current exe: {e}"))?;
            let command = format!("\"{}\"", exe.display());
            Some(
                command
                    .encode_utf16()
                    .chain(Some(0))
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<u8>>(),
            )
        } else {
            None
        };

        let key_path: Vec<u16> = format!("{KEY}\0").encode_utf16().collect();
        let value_name: Vec<u16> = format!("{VALUE}\0").encode_utf16().collect();
        unsafe {
            let mut key = HKEY::default();
            let status = RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(key_path.as_ptr()),
                None,
                KEY_SET_VALUE,
                &mut key,
            );
            if status != ERROR_SUCCESS {
                return Err(format!("cannot open Run key: {status:?}"));
            }
            let outcome = match &payload {
                Some(data) => {
                    let status =
                        RegSetValueExW(key, PCWSTR(value_name.as_ptr()), None, REG_SZ, Some(data));
                    if status == ERROR_SUCCESS {
                        Ok(())
                    } else {
                        Err(format!("RegSetValueExW: {status:?}"))
                    }
                }
                None => {
                    let status = RegDeleteValueW(key, PCWSTR(value_name.as_ptr()));
                    if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
                        Ok(())
                    } else {
                        Err(format!("RegDeleteValueW: {status:?}"))
                    }
                }
            };
            let _ = RegCloseKey(key);
            outcome
        }
    }

    #[cfg(not(windows))]
    {
        let _ = enabled;
        Err("autostart is Windows-only".to_owned())
    }
}

pub fn enable_utf8_console() {
    #[cfg(windows)]
    {
        use windows::Win32::System::Console::{GetConsoleOutputCP, SetConsoleOutputCP};
        const CP_UTF8: u32 = 65001;
        unsafe {
            if GetConsoleOutputCP() != CP_UTF8 {
                let _ = SetConsoleOutputCP(CP_UTF8);
            }
        }
    }
}

pub fn enable_per_monitor_dpi() {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

pub fn virtual_screen_px() -> (i32, i32, u32, u32) {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN,
        };
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1) as u32,
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1) as u32,
        )
    }

    #[cfg(not(windows))]
    {
        (0, 0, 1280, 720)
    }
}

pub fn monitor_scale_at(x: i32, y: i32) -> f32 {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromPoint};
        use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

        let monitor = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut dpi_x = 96u32;
        let mut dpi_y = 96u32;
        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_ok() {
            (dpi_x as f32 / 96.0).max(1.0)
        } else {
            1.0
        }
    }

    #[cfg(not(windows))]
    {
        let _ = (x, y);
        1.0
    }
}
pub fn primary_screen_points() -> (f32, f32) {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::HiDpi::GetDpiForSystem;
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
        let scale = (GetDpiForSystem() as f32 / 96.0).max(1.0);
        (
            GetSystemMetrics(SM_CXSCREEN) as f32 / scale,
            GetSystemMetrics(SM_CYSCREEN) as f32 / scale,
        )
    }

    #[cfg(not(windows))]
    {
        (1280.0, 720.0)
    }
}

#[allow(dead_code)]
pub fn primary_screen_px() -> (u32, u32) {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
        (
            GetSystemMetrics(SM_CXSCREEN).max(0) as u32,
            GetSystemMetrics(SM_CYSCREEN).max(0) as u32,
        )
    }

    #[cfg(not(windows))]
    {
        (1280, 720)
    }
}
