//! The tray on Windows: `Shell_NotifyIcon`, driven by a hidden window on
//! the tray's own thread, which also runs its message loop.

use std::{
    cell::{Cell, RefCell},
    sync::mpsc,
    thread::JoinHandle,
};

use tracing::{debug, warn};
use windows_sys::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
        UI::{
            Shell::{
                NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION,
                NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
                DestroyWindow, DispatchMessageW, GetMessageW, GetSystemMetrics, IMAGE_ICON,
                LR_DEFAULTCOLOR, LoadImageW, MF_SEPARATOR, MF_STRING, MSG, PostMessageW,
                PostThreadMessageW, RegisterClassW, RegisterWindowMessageW, SM_CXSMICON,
                SM_CYSMICON, SetForegroundWindow, SetMenuDefaultItem, TPM_NONOTIFY, TPM_RETURNCMD,
                TPM_RIGHTBUTTON, TrackPopupMenuEx, TranslateMessage, WM_APP, WM_CONTEXTMENU,
                WM_NULL, WM_QUIT, WNDCLASSW, WS_OVERLAPPED,
            },
        },
    },
    core::PCWSTR,
};

use super::{Action, OnAction};

/// The message the icon's clicks arrive as.
const CALLBACK: u32 = WM_APP + 1;
/// A tray icon picked with the keyboard (Enter or Space on it).
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
/// Menu item ids.
const OPEN: usize = 1;
const QUIT: usize = 2;

thread_local! {
    static ON_ACTION: RefCell<Option<OnAction>> = const { RefCell::new(None) };
    /// Explorer broadcasts this when it (re)starts, and the icon has to be added again.
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
}

pub struct Tray {
    thread: u32,
    join: Option<JoinHandle<()>>,
}

impl Drop for Tray {
    fn drop(&mut self) {
        // SAFETY: posting to a thread id takes no pointers.
        unsafe { PostThreadMessageW(self.thread, WM_QUIT, 0, 0) };
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

pub fn spawn(on_action: OnAction) -> Option<Tray> {
    let (ready, is_ready) = mpsc::channel();
    let join = std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            ON_ACTION.with(|cell| *cell.borrow_mut() = Some(on_action));
            // SAFETY: the window lives on this thread, and every call below
            // happens here, before it's destroyed.
            unsafe {
                let Some(window) = create_window() else {
                    let _ = ready.send(None);
                    return;
                };
                // Explorer may not be up yet, at login: then the icon comes
                // with its TaskbarCreated broadcast.
                if !add_icon(window) {
                    debug!("the taskbar isn't there yet; the tray icon waits for it");
                }
                let _ = ready.send(Some(GetCurrentThreadId()));
                let mut message = MSG::default();
                while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                remove_icon(window);
                DestroyWindow(window);
            }
        })
        .ok()?;
    match is_ready.recv() {
        Ok(Some(thread)) => Some(Tray {
            thread,
            join: Some(join),
        }),
        _ => {
            let _ = join.join();
            None
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

/// A top-level window that's never shown: a message-only one would miss
/// Explorer's broadcasts.
unsafe fn create_window() -> Option<HWND> {
    let class = wide("KithTray");
    // SAFETY: the class name outlives both calls, and a class registered by an
    // earlier tray in this process is fine to reuse.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class_info = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..Default::default()
        };
        RegisterClassW(&class_info);
        let window = CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if window.is_null() {
            warn!("couldn't create the tray's window");
            return None;
        }
        TASKBAR_CREATED.set(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()));
        Some(window)
    }
}

fn icon_data(window: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: 1,
        ..Default::default()
    }
}

/// Adds the icon: the exe's own (resource 1), at the size the tray uses.
unsafe fn add_icon(window: HWND) -> bool {
    let mut data = icon_data(window);
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    data.uCallbackMessage = CALLBACK;
    for (slot, unit) in data.szTip.iter_mut().zip("Kith".encode_utf16()) {
        *slot = unit;
    }
    // SAFETY: resource 1 is a name by number (MAKEINTRESOURCE), and `data`
    // outlives the calls that read it.
    unsafe {
        data.hIcon = LoadImageW(
            GetModuleHandleW(std::ptr::null()),
            1 as PCWSTR,
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_DEFAULTCOLOR,
        );
        if Shell_NotifyIconW(NIM_ADD, &data) == 0 {
            return false;
        }
        // Version 4: clicks arrive as NIN_SELECT and WM_CONTEXTMENU, with
        // the position in wParam.
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Shell_NotifyIconW(NIM_SETVERSION, &data);
    }
    true
}

unsafe fn remove_icon(window: HWND) {
    let data = icon_data(window);
    // SAFETY: `data` names the icon by window and id, and outlives the call.
    unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
}

fn act(action: Action) {
    ON_ACTION.with(|cell| {
        if let Some(on_action) = cell.borrow().as_ref() {
            on_action(action);
        }
    });
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == CALLBACK {
        match (lparam & 0xffff) as u32 {
            NIN_SELECT | NIN_KEYSELECT => act(Action::Open),
            WM_CONTEXTMENU => {
                // Screen coordinates, signed: monitors left of the main one are negative.
                let x = (wparam & 0xffff) as u16 as i16 as i32;
                let y = ((wparam >> 16) & 0xffff) as u16 as i16 as i32;
                // SAFETY: called on the window's own thread.
                unsafe { menu(window, x, y) };
            }
            _ => {}
        }
        return 0;
    }
    if message != 0 && message == TASKBAR_CREATED.get() {
        // SAFETY: as above.
        unsafe { add_icon(window) };
        return 0;
    }
    // SAFETY: everything else gets Windows' default handling.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

/// The right-click menu: Open Kith, and Quit.
unsafe fn menu(window: HWND, x: i32, y: i32) {
    let (open, quit) = (wide("Open Kith"), wide("Quit Kith"));
    // SAFETY: the labels outlive the menu, which is destroyed before returning.
    let chosen = unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        AppendMenuW(menu, MF_STRING, OPEN, open.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, QUIT, quit.as_ptr());
        SetMenuDefaultItem(menu, OPEN as u32, 0);
        // Without these two, the menu stays open when you click elsewhere.
        SetForegroundWindow(window);
        let chosen = TrackPopupMenuEx(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            x,
            y,
            window,
            std::ptr::null(),
        );
        PostMessageW(window, WM_NULL, 0, 0);
        DestroyMenu(menu);
        chosen as usize
    };
    match chosen {
        OPEN => act(Action::Open),
        QUIT => act(Action::Quit),
        _ => {}
    }
}
