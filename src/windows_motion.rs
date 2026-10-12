//! Dedicated pointer hook: inputs' relative translator cannot track a cursor
//! whose physical movement we suppress. Keys/buttons still use inputs.
use std::{cell::Cell, sync::mpsc, thread};
use windows_sys::Win32::{
    Foundation::{LPARAM, LRESULT, POINT, WPARAM},
    System::Threading::GetCurrentThreadId,
    UI::WindowsAndMessaging::*,
};
thread_local! { static WARPING: Cell<bool> = const {Cell::new(false)}; }

pub fn warp(x: i32, y: i32) -> bool {
    WARPING.with(|flag| {
        flag.set(true);
        let ok = unsafe { SetCursorPos(x, y) } != 0;
        flag.set(false);
        ok
    })
}
unsafe extern "system" fn hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code >= 0 && message as u32 == WM_MOUSEMOVE && !WARPING.with(Cell::get) {
        // WH_MOUSE_LL guarantees a live MSLLHOOKSTRUCT for HC_ACTION.
        let event = unsafe { &*(data as *const MSLLHOOKSTRUCT) };
        if event.flags & LLMHF_INJECTED == 0 {
            let mut before = POINT::default();
            if unsafe { GetCursorPos(&mut before) } != 0
                && crate::platform_input::source_motion(
                    f64::from(event.pt.x),
                    f64::from(event.pt.y),
                    f64::from(before.x),
                    f64::from(before.y),
                )
            {
                return 1;
            }
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, message, data) }
}
pub struct MotionHook {
    id: u32,
    thread: Option<thread::JoinHandle<()>>,
}
impl MotionHook {
    pub fn start() -> Result<Self, String> {
        let (tx, rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("onekeymouse-motion".into())
            .spawn(move || unsafe {
                let _dpi = crate::desktop::DpiContext::physical();
                let mut message = MSG::default();
                // Create the queue before publishing the thread id for shutdown.
                PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
                let handle = SetWindowsHookExW(WH_MOUSE_LL, Some(hook), std::ptr::null_mut(), 0);
                if handle.is_null() {
                    let _ = tx.send(Err("Не удалось захватить курсор Windows".to_string()));
                    return;
                }
                let _ = tx.send(Ok(GetCurrentThreadId()));
                while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                UnhookWindowsHookEx(handle);
            })
            .map_err(|e| e.to_string())?;
        match rx.recv().map_err(|e| e.to_string())? {
            Ok(id) => Ok(Self {
                id,
                thread: Some(thread),
            }),
            Err(e) => {
                let _ = thread.join();
                Err(e)
            }
        }
    }
}
impl Drop for MotionHook {
    fn drop(&mut self) {
        unsafe {
            PostThreadMessageW(self.id, WM_QUIT, 0, 0);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
