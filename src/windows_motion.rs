//! Dedicated pointer hook: inputs' relative translator cannot track a cursor
//! whose physical movement we suppress. Keys/buttons still use inputs.
use std::{cell::Cell, sync::mpsc, thread};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{Input::*, WindowsAndMessaging::*},
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

unsafe extern "system" fn raw_window(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_INPUT {
        let mut raw = RAWINPUT::default();
        let mut size = std::mem::size_of::<RAWINPUT>() as u32;
        let read = unsafe {
            GetRawInputData(
                lparam as HRAWINPUT,
                RID_INPUT,
                (&mut raw as *mut RAWINPUT).cast(),
                &mut size,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        if read != u32::MAX
            && read
                >= (std::mem::size_of::<RAWINPUTHEADER>() + std::mem::size_of::<RAWMOUSE>()) as u32
            && raw.header.dwType == RIM_TYPEMOUSE
        {
            let mouse = unsafe { raw.data.mouse };
            if mouse.usFlags & MOUSE_MOVE_ABSOLUTE == 0 {
                let mut position = POINT::default();
                if unsafe { GetCursorPos(&mut position) } != 0 {
                    crate::platform_input::raw_motion(
                        mouse.lLastX.into(),
                        mouse.lLastY.into(),
                        position.x.into(),
                        position.y.into(),
                    );
                }
            }
        }
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
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
                let class: Vec<u16> = "OneKeyMouseRawMotion\0".encode_utf16().collect();
                let module = GetModuleHandleW(std::ptr::null());
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(raw_window),
                    hInstance: module,
                    lpszClassName: class.as_ptr(),
                    ..Default::default()
                };
                if RegisterClassW(&wc) == 0 {
                    let _ = tx.send(Err("Не удалось зарегистрировать окно Raw Input".into()));
                    return;
                }
                let window = CreateWindowExW(
                    0,
                    class.as_ptr(),
                    class.as_ptr(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    HWND_MESSAGE,
                    std::ptr::null_mut(),
                    module,
                    std::ptr::null(),
                );
                let device = RAWINPUTDEVICE {
                    usUsagePage: 1,
                    usUsage: 2,
                    dwFlags: RIDEV_INPUTSINK,
                    hwndTarget: window,
                };
                if window.is_null()
                    || RegisterRawInputDevices(
                        &device,
                        1,
                        std::mem::size_of::<RAWINPUTDEVICE>() as u32,
                    ) == 0
                {
                    if !window.is_null() {
                        DestroyWindow(window);
                    }
                    UnregisterClassW(class.as_ptr(), module);
                    let _ = tx.send(Err("Не удалось включить относительное движение мыши".into()));
                    return;
                }
                let handle = SetWindowsHookExW(WH_MOUSE_LL, Some(hook), std::ptr::null_mut(), 0);
                if handle.is_null() {
                    let remove = RAWINPUTDEVICE {
                        dwFlags: RIDEV_REMOVE,
                        hwndTarget: std::ptr::null_mut(),
                        ..device
                    };
                    RegisterRawInputDevices(
                        &remove,
                        1,
                        std::mem::size_of::<RAWINPUTDEVICE>() as u32,
                    );
                    DestroyWindow(window);
                    UnregisterClassW(class.as_ptr(), module);
                    let _ = tx.send(Err("Не удалось захватить курсор Windows".to_string()));
                    return;
                }
                let _ = tx.send(Ok(GetCurrentThreadId()));
                while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                UnhookWindowsHookEx(handle);
                let remove = RAWINPUTDEVICE {
                    dwFlags: RIDEV_REMOVE,
                    hwndTarget: std::ptr::null_mut(),
                    ..device
                };
                RegisterRawInputDevices(&remove, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32);
                DestroyWindow(window);
                UnregisterClassW(class.as_ptr(), module);
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
