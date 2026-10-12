use crate::handoff::Rect;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScreenInfo {
    pub id: String,
    pub label: String,
    pub bounds: Rect,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub primary: bool,
    pub coordinate_unit: String,
}
pub fn validate_screens(screens: &[ScreenInfo]) -> bool {
    !screens.is_empty()
        && screens.len() <= 32
        && screens.iter().all(|s| {
            let b = s.bounds;
            !s.id.is_empty()
                && s.id.len() <= 256
                && s.label.len() <= 256
                && b.x.is_finite()
                && b.y.is_finite()
                && b.x.abs() <= 1_000_000.
                && b.y.abs() <= 1_000_000.
                && b.width.is_finite()
                && b.height.is_finite()
                && (2.0..=100_000.).contains(&b.width)
                && (8.0..=100_000.).contains(&b.height)
                && s.pixel_width > 0
                && s.pixel_height > 0
                && s.pixel_width <= 100_000
                && s.pixel_height <= 100_000
                && matches!(s.coordinate_unit.as_str(), "px" | "pt")
        })
        && screens.iter().filter(|s| s.primary).count() == 1
        && screens
            .iter()
            .enumerate()
            .all(|(i, s)| screens[..i].iter().all(|prev| prev.id != s.id))
}
#[cfg(target_os = "windows")]
pub fn screens() -> Result<Vec<ScreenInfo>, String> {
    let _dpi = DpiContext::physical();
    use windows_sys::{
        Win32::{
            Foundation::{LPARAM, RECT},
            Graphics::Gdi::*,
        },
        core::BOOL,
    };
    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) } == 0 {
            return 1;
        }
        let r = info.monitorInfo.rcMonitor;
        let len = info
            .szDevice
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(info.szDevice.len());
        let id = String::from_utf16_lossy(&info.szDevice[..len]);
        let screens = unsafe { &mut *(data as *mut Vec<ScreenInfo>) };
        screens.push(ScreenInfo {
            label: format!("Windows · {}", id.trim_start_matches("\\\\.\\")),
            id,
            bounds: Rect {
                x: f64::from(r.left),
                y: f64::from(r.top),
                width: f64::from(r.right - r.left),
                height: f64::from(r.bottom - r.top),
            },
            pixel_width: (r.right - r.left) as u32,
            pixel_height: (r.bottom - r.top) as u32,
            primary: info.monitorInfo.dwFlags & 1 != 0,
            coordinate_unit: "px".into(),
        });
        1
    }
    let mut screens: Vec<ScreenInfo> = Vec::new();
    if unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(collect),
            &mut screens as *mut _ as isize,
        )
    } == 0
        || screens.is_empty()
    {
        return Err("Не удалось прочитать границы мониторов Windows".into());
    }
    screens.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(screens)
}
#[cfg(target_os = "macos")]
pub fn screens() -> Result<Vec<ScreenInfo>, String> {
    use core_graphics::display::CGDisplay;
    let ids = CGDisplay::active_displays().map_err(|_| "Не удалось определить экраны Mac")?;
    let mut screens: Vec<ScreenInfo> = Vec::new();
    for id in ids {
        let display = CGDisplay::new(id);
        let b = display.bounds();
        let mode = display.display_mode();
        screens.push(ScreenInfo {
            id: format!("mac-{id}"),
            label: format!("Mac · экран {}", screens.len() + 1),
            bounds: Rect {
                x: b.origin.x,
                y: b.origin.y,
                width: b.size.width,
                height: b.size.height,
            },
            pixel_width: mode
                .as_ref()
                .map_or_else(|| display.pixels_wide(), |m| m.pixel_width())
                as u32,
            pixel_height: mode
                .as_ref()
                .map_or_else(|| display.pixels_high(), |m| m.pixel_height())
                as u32,
            primary: display.is_main(),
            coordinate_unit: "pt".into(),
        });
    }
    screens.sort_by(|a, b| a.id.cmp(&b.id));
    if screens.is_empty() {
        return Err("Не найден активный экран Mac".into());
    }
    Ok(screens)
}
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn screens() -> Result<Vec<ScreenInfo>, String> {
    Err("Поддерживаются Windows и macOS".into())
}
pub fn monitors() -> Result<Vec<Rect>, String> {
    Ok(screens()?.into_iter().map(|s| s.bounds).collect())
}

#[cfg(target_os = "windows")]
pub fn warp(x: f64, y: f64) -> Result<(), String> {
    let _dpi = DpiContext::physical();
    if !crate::windows_motion::warp(x.round() as i32, y.round() as i32) {
        Err("Не удалось переместить курсор Windows".into())
    } else {
        Ok(())
    }
}
#[cfg(not(target_os = "windows"))]
pub fn warp(_x: f64, _y: f64) -> Result<(), String> {
    Err("Курсор источника доступен только на Windows".into())
}

#[cfg(target_os = "macos")]
fn post_pointer(
    x: f64,
    y: f64,
    kind: core_graphics::event::CGEventType,
    button: core_graphics::event::CGMouseButton,
    number: Option<i64>,
) -> Result<(), String> {
    use core_graphics::{
        event::{CGEvent, CGEventTapLocation, EventField},
        event_source::{CGEventSource, CGEventSourceStateID},
        geometry::CGPoint,
    };
    // Reuse one source per backend thread rather than allocating one for every
    // hardware sample. Events still carry the shared HID state and our tag.
    thread_local! { static MOUSE_SOURCE: std::cell::RefCell<Option<CGEventSource>> = const {std::cell::RefCell::new(None)}; }
    let source = MOUSE_SOURCE.with(|cached| {
        let mut cached = cached.borrow_mut();
        if cached.is_none() {
            *cached = Some(
                CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                    .map_err(|_| "Нет доступа к вводу macOS")?,
            );
        }
        Ok::<_, String>(cached.as_ref().unwrap().clone())
    })?;
    let event = CGEvent::new_mouse_event(source, kind, CGPoint::new(x, y), button)
        .map_err(|_| "Не удалось передать мышь Mac")?;
    event.set_integer_value_field(
        EventField::EVENT_SOURCE_USER_DATA,
        inputs::inject::default_signature().0 as i64,
    );
    if let Some(n) = number {
        event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, n);
    }
    event.post(CGEventTapLocation::HID);
    Ok(())
}
#[cfg(target_os = "macos")]
pub fn move_pointer(
    x: f64,
    y: f64,
    button: Option<crate::input::MouseButton>,
) -> Result<(), String> {
    use crate::input::MouseButton;
    use core_graphics::event::{CGEventType, CGMouseButton};
    let (kind, button, number) = match button {
        Some(MouseButton::Left) => (CGEventType::LeftMouseDragged, CGMouseButton::Left, None),
        Some(MouseButton::Right) => (CGEventType::RightMouseDragged, CGMouseButton::Right, None),
        Some(MouseButton::Middle) => (
            CGEventType::OtherMouseDragged,
            CGMouseButton::Center,
            Some(2),
        ),
        Some(MouseButton::Back) => (
            CGEventType::OtherMouseDragged,
            CGMouseButton::Center,
            Some(3),
        ),
        Some(MouseButton::Forward) => (
            CGEventType::OtherMouseDragged,
            CGMouseButton::Center,
            Some(4),
        ),
        None => (CGEventType::MouseMoved, CGMouseButton::Left, None),
    };
    post_pointer(x, y, kind, button, number)
}
#[cfg(target_os = "macos")]
pub fn button_pointer(
    x: f64,
    y: f64,
    button: crate::input::MouseButton,
    pressed: bool,
) -> Result<(), String> {
    use crate::input::MouseButton;
    use core_graphics::event::{CGEventType, CGMouseButton};
    let (down, up, button, number) = match button {
        MouseButton::Left => (
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGMouseButton::Left,
            None,
        ),
        MouseButton::Right => (
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGMouseButton::Right,
            None,
        ),
        other => (
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGMouseButton::Center,
            Some(match other {
                MouseButton::Middle => 2,
                MouseButton::Back => 3,
                _ => 4,
            }),
        ),
    };
    post_pointer(x, y, if pressed { down } else { up }, button, number)
}
#[cfg(not(target_os = "macos"))]
pub fn button_pointer(
    _x: f64,
    _y: f64,
    _button: crate::input::MouseButton,
    _pressed: bool,
) -> Result<(), String> {
    Err("Получатель должен работать на macOS".into())
}

#[cfg(not(target_os = "macos"))]
pub fn move_pointer(
    _x: f64,
    _y: f64,
    _button: Option<crate::input::MouseButton>,
) -> Result<(), String> {
    Err("Получатель должен работать на macOS".into())
}

#[cfg(target_os = "windows")]
pub struct DpiContext(windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT);
#[cfg(target_os = "windows")]
impl DpiContext {
    pub fn physical() -> Self {
        use windows_sys::Win32::UI::HiDpi::*;
        Self(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) })
    }
}
#[cfg(target_os = "windows")]
impl Drop for DpiContext {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                windows_sys::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(self.0);
            }
        }
    }
}
#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightPostEventAccess() -> bool;
    fn CGRequestPostEventAccess() -> bool;
}
pub fn check_receive_permission(request: bool) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        if unsafe { CGPreflightPostEventAccess() } {
            return Ok(());
        }
        if request {
            unsafe {
                CGRequestPostEventAccess();
            }
        }
        Err("Разреши OneKeyMouse в Настройки системы → Конфиденциальность и безопасность → Универсальный доступ, затем подключись повторно.".into())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = request;
        Err("Принимающий компьютер должен работать на macOS".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn screen(id: &str, primary: bool) -> ScreenInfo {
        ScreenInfo {
            id: id.into(),
            label: "Test screen".into(),
            bounds: Rect {
                x: -1920.,
                y: 0.,
                width: 1920.,
                height: 1080.,
            },
            pixel_width: 1920,
            pixel_height: 1080,
            primary,
            coordinate_unit: "px".into(),
        }
    }
    #[test]
    fn peer_inventory_accepts_negative_coordinates_but_rejects_invalid_geometry() {
        let primary = screen("display-1", true);
        assert!(validate_screens(std::slice::from_ref(&primary)));
        assert!(validate_screens(&[
            primary.clone(),
            screen("display-2", false)
        ]));
        assert!(!validate_screens(&[primary.clone(), primary.clone()]));
        assert!(!validate_screens(&[
            primary.clone(),
            screen("display-2", true)
        ]));
        let mut invalid = primary.clone();
        invalid.bounds.width = f64::NAN;
        assert!(!validate_screens(&[invalid]));
        let mut invalid = primary;
        invalid.pixel_width = 0;
        assert!(!validate_screens(&[invalid]));
        assert!(!validate_screens(&[]));
    }
    #[test]
    #[ignore = "Read the live computer's display inventory; run manually on a desktop"]
    fn native_display_inventory() {
        let found = screens().unwrap();
        println!("{}", serde_json::to_string_pretty(&found).unwrap());
        assert!(validate_screens(&found));
    }
}

#[cfg(target_os = "windows")]
pub fn physical_inputs_held() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    // Also covers keys held before capture was started, which have no down
    // transition in the listener's local history.
    (1..=255).any(|key| unsafe { GetAsyncKeyState(key) } < 0)
}
