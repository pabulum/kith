//! What a stream from Windows shows: a monitor or one window, as the app's
//! picker lists them. ffmpeg's gfxcapture (Windows.Graphics.Capture) takes
//! them by handle, so a window is captured exactly, whatever its title says.

/// What to stream. Linux ignores it: the desktop portal asks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub enum Share {
    /// The main monitor, whichever it is when the stream starts.
    #[default]
    MainScreen,
    /// A monitor, by its HMONITOR.
    Screen { handle: u64, label: String },
    /// A window, by its HWND.
    Window { handle: u64, label: String },
}

impl Share {
    /// What the picker shows, e.g. "Screen 2 · 1920×1080".
    pub fn label(&self) -> &str {
        match self {
            Self::MainScreen => "Main screen",
            Self::Screen { label, .. } | Self::Window { label, .. } => label,
        }
    }
}

/// Window titles are cut to this many characters, so the picker stays readable.
#[cfg(any(windows, test))]
const TITLE_LENGTH: usize = 60;

/// How a window reads in the picker: its title, then the app it belongs to.
#[cfg(any(windows, test))]
fn window_label(title: &str, exe: &str) -> String {
    let full = title.trim();
    let mut title: String = full.chars().take(TITLE_LENGTH).collect();
    if full.chars().count() > TITLE_LENGTH {
        title.push('…');
    }
    let app = exe.strip_suffix(".exe").unwrap_or(exe);
    if app.is_empty() {
        title
    } else {
        format!("{title} · {app}")
    }
}

#[cfg(windows)]
pub use windows::{available, main_monitor, still_open};

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;

    use windows_sys::Win32::{
        Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT},
        Graphics::{
            Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
            Gdi::{
                EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTOPRIMARY,
                MONITORINFO, MONITORINFOEXW, MonitorFromPoint,
            },
        },
        System::Threading::{
            GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
        },
        UI::WindowsAndMessaging::{
            EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongW, GetWindowTextLengthW,
            GetWindowTextW, GetWindowThreadProcessId, IsWindow, IsWindowVisible, WS_EX_TOOLWINDOW,
        },
    };

    use super::{Share, window_label};

    /// Every monitor, the main one first, then the windows Alt-Tab would show,
    /// front to back.
    pub fn available() -> Vec<Share> {
        let mut shares = monitors();
        shares.extend(windows());
        shares
    }

    /// The main monitor's handle.
    pub fn main_monitor() -> u64 {
        // SAFETY: MonitorFromPoint takes a point by value and a flag.
        unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) as u64 }
    }

    /// Whether a window picked earlier is still there to capture.
    pub fn still_open(handle: u64) -> bool {
        // SAFETY: IsWindow accepts any value and says whether it names a window.
        unsafe { IsWindow(handle as HWND) != 0 }
    }

    fn monitors() -> Vec<Share> {
        let mut handles: Vec<HMONITOR> = Vec::new();
        // SAFETY: the callback only pushes onto the Vec whose address it's
        // given, which outlives the call.
        unsafe {
            EnumDisplayMonitors(
                std::ptr::null_mut(),
                std::ptr::null(),
                Some(push_monitor),
                &mut handles as *mut Vec<HMONITOR> as LPARAM,
            );
        }
        let main = main_monitor();
        let mut screens: Vec<(bool, Share)> = handles
            .into_iter()
            .filter_map(|handle| {
                // SAFETY: MONITORINFOEXW starts with MONITORINFO, whose cbSize
                // says which of the two GetMonitorInfoW fills.
                let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
                info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
                let ok = unsafe {
                    GetMonitorInfoW(handle, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO)
                };
                if ok == 0 {
                    return None;
                }
                let RECT {
                    left,
                    top,
                    right,
                    bottom,
                } = info.monitorInfo.rcMonitor;
                let device = String::from_utf16_lossy(&info.szDevice);
                // `\\.\DISPLAY2` is the number Windows' display settings show.
                let number: String = device
                    .trim_end_matches('\0')
                    .chars()
                    .rev()
                    .take_while(char::is_ascii_digit)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                let is_main = handle as u64 == main;
                let name = match (number.is_empty(), is_main) {
                    (false, true) => format!("Screen {number} (main)"),
                    (false, false) => format!("Screen {number}"),
                    (true, true) => "Main screen".to_string(),
                    (true, false) => "Screen".to_string(),
                };
                let label = format!("{name} · {}×{}", right - left, bottom - top);
                Some((
                    is_main,
                    Share::Screen {
                        handle: handle as u64,
                        label,
                    },
                ))
            })
            .collect();
        screens.sort_by_key(|(is_main, _)| !is_main);
        screens.into_iter().map(|(_, share)| share).collect()
    }

    unsafe extern "system" fn push_monitor(
        handle: HMONITOR,
        _: HDC,
        _: *mut RECT,
        list: LPARAM,
    ) -> i32 {
        // SAFETY: `list` is the Vec that `monitors` passed in.
        unsafe { (*(list as *mut Vec<HMONITOR>)).push(handle) };
        1
    }

    fn windows() -> Vec<Share> {
        let mut handles: Vec<HWND> = Vec::new();
        // SAFETY: as for EnumDisplayMonitors above.
        unsafe { EnumWindows(Some(push_window), &mut handles as *mut Vec<HWND> as LPARAM) };
        // SAFETY: GetCurrentProcessId has no preconditions.
        let own = unsafe { GetCurrentProcessId() };
        handles
            .into_iter()
            .filter_map(|hwnd| {
                let (title, pid) = alt_tab(hwnd)?;
                if pid == own {
                    return None;
                }
                Some(Share::Window {
                    handle: hwnd as u64,
                    label: window_label(&title, &exe_name(pid)),
                })
            })
            .collect()
    }

    unsafe extern "system" fn push_window(hwnd: HWND, list: LPARAM) -> i32 {
        // SAFETY: `list` is the Vec that `windows` passed in.
        unsafe { (*(list as *mut Vec<HWND>)).push(hwnd) };
        1
    }

    /// The title and process of a window Alt-Tab would list: visible, titled,
    /// not owned by another window, not a tool window, and not cloaked (the
    /// hidden Store apps and other virtual desktops' windows).
    fn alt_tab(hwnd: HWND) -> Option<(String, u32)> {
        // SAFETY: each call takes a window handle, which is valid or makes the
        // call fail, plus buffers sized as passed.
        unsafe {
            if IsWindowVisible(hwnd) == 0 || !GetWindow(hwnd, GW_OWNER).is_null() {
                return None;
            }
            if GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW != 0 {
                return None;
            }
            let mut cloaked: u32 = 0;
            let got = DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED as u32,
                &mut cloaked as *mut u32 as *mut c_void,
                std::mem::size_of::<u32>() as u32,
            );
            if got == 0 && cloaked != 0 {
                return None;
            }
            let length = GetWindowTextLengthW(hwnd);
            if length <= 0 {
                return None;
            }
            let mut text = vec![0u16; length as usize + 1];
            let copied = GetWindowTextW(hwnd, text.as_mut_ptr(), text.len() as i32);
            let title = String::from_utf16_lossy(&text[..copied.max(0) as usize]);
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            Some((title, pid))
        }
    }

    /// The file name of a process's program, e.g. "chrome.exe", or "" when
    /// Windows won't say (another user's process, or one that's gone).
    fn exe_name(pid: u32) -> String {
        // SAFETY: the process handle is checked, and closed after use; the
        // path buffer's length goes in and out through `size`.
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return String::new();
            }
            let mut path = [0u16; 1024];
            let mut size = path.len() as u32;
            let ok = QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                path.as_mut_ptr(),
                &mut size,
            );
            CloseHandle(process);
            if ok == 0 {
                return String::new();
            }
            let path = String::from_utf16_lossy(&path[..size as usize]);
            path.rsplit(['\\', '/'])
                .next()
                .unwrap_or_default()
                .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs under Wine or on Windows: the main monitor is listed first, and
    /// windows follow it.
    #[cfg(windows)]
    #[test]
    fn lists_what_can_be_shared() {
        let shares = available();
        for share in &shares {
            println!("{share:?}");
        }
        assert!(
            matches!(&shares[0], Share::Screen { handle, .. } if *handle == main_monitor()),
            "{shares:?}"
        );
    }

    #[test]
    fn window_labels_name_the_app() {
        assert_eq!(
            window_label("Total War: WARHAMMER III", "Warhammer3.exe"),
            "Total War: WARHAMMER III · Warhammer3"
        );
        assert_eq!(window_label("Notes", ""), "Notes");
        let long = "x".repeat(80);
        let label = window_label(&long, "app.exe");
        assert_eq!(label, format!("{}… · app", "x".repeat(TITLE_LENGTH)));
    }
}
