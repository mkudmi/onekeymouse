use crate::{
    handoff::{Edge, Rect},
    input::InputEvent,
};
use inputs::{
    Button, Event, EventDisposition, EventKind, EventSink, Listener, ListenerBuilder, PressState,
    inject::Injector,
};
use std::{
    collections::HashSet,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::mpsc;
static RUNNING: AtomicBool = AtomicBool::new(false);
static CAPTURE_OVERFLOW: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub enum Captured {
    Input(InputEvent),
    Local,
}
#[derive(Default)]
struct Routing {
    sender: Option<mpsc::Sender<Captured>>,
    connected: bool,
    remote: bool,
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    monitors: Vec<Rect>,
    percent: f32,
    edge: Option<Edge>,
    entry: f64,
    held_keys: HashSet<inputs::PhysicalKey>,
    held_buttons: HashSet<Button>,
    suppressed_keys: HashSet<inputs::PhysicalKey>,
    suppressed_buttons: HashSet<Button>,
}
fn routing() -> &'static Mutex<Routing> {
    static ROUTING: OnceLock<Mutex<Routing>> = OnceLock::new();
    ROUTING.get_or_init(|| Mutex::new(Routing::default()))
}
fn enqueue(r: &mut Routing, event: Captured) -> bool {
    if r.sender
        .as_ref()
        .is_some_and(|tx| tx.try_send(event).is_ok())
    {
        true
    } else {
        CAPTURE_OVERFLOW.store(true, Ordering::SeqCst);
        r.remote = false;
        r.connected = false;
        false
    }
}
pub struct InputCapture {
    _listener: Listener,
    #[cfg(target_os = "windows")]
    _motion: crate::windows_motion::MotionHook,
    receiver: Option<mpsc::Receiver<Captured>>,
}
impl InputCapture {
    pub fn restore_receiver(&mut self, receiver: mpsc::Receiver<Captured>) {
        if self.receiver.is_none() {
            self.receiver = Some(receiver);
        }
    }
    pub fn take_receiver(&mut self) -> Option<mpsc::Receiver<Captured>> {
        self.receiver.take()
    }
}
struct ForwardSink;
impl EventSink for ForwardSink {
    fn on_event(&self, event: &Event) -> EventDisposition {
        if event.injected {
            return EventDisposition::Pass;
        }
        let mut r = routing().lock().unwrap();
        let remote = r.remote;
        let mut suppress = remote;
        match &event.kind {
            EventKind::Key(k) => {
                if k.state == PressState::Pressed {
                    r.held_keys.insert(k.physical);
                } else {
                    r.held_keys.remove(&k.physical);
                }
                suppress |= r.suppressed_keys.contains(&k.physical);
                if remote && k.state == PressState::Pressed {
                    r.suppressed_keys.insert(k.physical);
                }
                if k.state == PressState::Released {
                    r.suppressed_keys.remove(&k.physical);
                }
                if remote
                    && k.code() == Some(inputs::KeyCode::F12)
                    && k.modifiers.control
                    && k.modifiers.alt
                    && k.state == PressState::Pressed
                {
                    let position = r.entry;
                    drop(r);
                    return_local(position);
                    return EventDisposition::Suppress;
                }
            }
            EventKind::Button(b) => {
                if b.state == PressState::Pressed {
                    r.held_buttons.insert(b.button);
                } else {
                    r.held_buttons.remove(&b.button);
                }
                suppress |= r.suppressed_buttons.contains(&b.button);
                if remote && b.state == PressState::Pressed {
                    r.suppressed_buttons.insert(b.button);
                }
                if b.state == PressState::Released {
                    r.suppressed_buttons.remove(&b.button);
                }
            }
            EventKind::CaptureInterrupted => {
                r.remote = false;
                r.connected = false;
                enqueue(&mut r, Captured::Local);
                return EventDisposition::Pass;
            }
            _ => {}
        }
        if remote
            && let Some(wire) = to_wire(event)
            && !enqueue(&mut r, Captured::Input(wire))
        {
            return EventDisposition::Pass;
        }
        if suppress {
            EventDisposition::Suppress
        } else {
            EventDisposition::Pass
        }
    }
}
pub fn start_capture() -> Result<InputCapture, String> {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return Err("Захват уже запущен".into());
    }
    let result = (|| {
        let monitors = crate::desktop::monitors()?;
        let (sender, receiver) = mpsc::channel(4096);
        *routing().lock().unwrap() = Routing {
            sender: Some(sender),
            monitors,
            percent: 42.9,
            ..Routing::default()
        };
        let builder = ListenerBuilder::default();
        #[cfg(target_os = "windows")]
        let builder = builder.event_kinds(
            inputs::EventKinds::KEY | inputs::EventKinds::BUTTON | inputs::EventKinds::SCROLL,
        );
        let listener = builder
            .start(ForwardSink)
            .map_err(|e| format!("Не удалось включить системный захват: {e}"))?;
        #[cfg(target_os = "windows")]
        let motion = crate::windows_motion::MotionHook::start()?;
        Ok(InputCapture {
            _listener: listener,
            receiver: Some(receiver),
            #[cfg(target_os = "windows")]
            _motion: motion,
        })
    })();
    if result.is_err() {
        stop_capture();
    }
    result
}
pub fn stop_capture() {
    let position = routing().lock().unwrap().entry;
    return_local(position);
    let mut r = routing().lock().unwrap();
    r.connected = false;
    r.sender = None;
    RUNNING.store(false, Ordering::SeqCst);
    CAPTURE_OVERFLOW.store(false, Ordering::SeqCst);
}
pub fn configure_session(percent: f32) {
    let mut r = routing().lock().unwrap();
    r.percent = percent;
    r.connected = true;
    r.remote = false;
}
pub fn update_monitors(monitors: Vec<Rect>) {
    let mut r = routing().lock().unwrap();
    if r.monitors == monitors {
        return;
    }
    // Windows moves its cursor onto a surviving screen when a display is
    // removed. Keep that OS position and release remote input atomically.
    if r.remote {
        r.remote = false;
        enqueue(&mut r, Captured::Local);
    }
    r.monitors = monitors;
    r.edge = None;
}
pub fn update_percent(percent: f32) {
    routing().lock().unwrap().percent = percent;
}
pub fn end_session() {
    let position = routing().lock().unwrap().entry;
    return_local(position);
    routing().lock().unwrap().connected = false;
}
pub fn return_local(position: f64) {
    let landing = {
        let mut r = routing().lock().unwrap();
        if !r.remote {
            return;
        }
        r.remote = false;
        enqueue(&mut r, Captured::Local);
        r.edge.map(|e| e.landing(position))
    };
    if let Some((x, y)) = landing {
        let _ = crate::desktop::warp(x, y);
    }
}
pub fn take_capture_overflow() -> bool {
    CAPTURE_OVERFLOW.swap(false, Ordering::SeqCst)
}
/// Called synchronously by the Windows mouse hook, before Windows moves its
/// cursor. For suppressed motion, each proposed point is relative to the
/// parked OS cursor, not the preceding proposed (suppressed) point.
#[cfg(target_os = "windows")]
pub fn source_motion(x: f64, y: f64, previous_x: f64, previous_y: f64) -> bool {
    let mut r = routing().lock().unwrap();
    let dx = x - previous_x;
    let dy = y - previous_y;
    if r.remote {
        return enqueue(&mut r, Captured::Input(InputEvent::Pointer { dx, dy }));
    }
    if !r.connected || !r.held_keys.is_empty() || !r.held_buttons.is_empty() {
        return false;
    }
    let Some(edge) = crate::handoff::exit_edge(&r.monitors, r.percent, x, y, dy) else {
        return false;
    };
    if crate::desktop::physical_inputs_held() {
        return false;
    }
    let position = edge.normalized(x);
    if !enqueue(&mut r, Captured::Input(InputEvent::Enter { position })) {
        return false;
    }
    r.edge = Some(edge);
    r.entry = position;
    r.remote = true;
    // Park away from the Windows desktop boundary so motion in every direction
    // keeps producing coordinates even while the physical stream is suppressed.
    let cx = edge.monitor.x + edge.monitor.width / 2.;
    let cy = edge.monitor.y + edge.monitor.height / 2.;
    // This is the mouse-hook thread; windows_motion::warp bypasses our own
    // reentrant callback. Keep the route locked until parking completes so a
    // simultaneous remote return cannot be overwritten by a late park.
    if crate::desktop::warp(cx, cy).is_err() {
        CAPTURE_OVERFLOW.store(true, Ordering::SeqCst);
        r.remote = false;
        r.connected = false;
        return false;
    }
    true
}
pub fn to_wire(event: &Event) -> Option<crate::input::InputEvent> {
    use crate::input::{InputEvent as Wire, MouseButton as B};
    match &event.kind {
        EventKind::Key(k) => Some(Wire::Key {
            code: k.physical,
            pressed: k.state == PressState::Pressed,
        }),
        EventKind::Button(b) => Some(Wire::Button {
            button: match b.button {
                Button::Left => B::Left,
                Button::Right => B::Right,
                Button::Middle => B::Middle,
                Button::Back => B::Back,
                Button::Forward => B::Forward,
                Button::Other(_) => return None,
                _ => return None,
            },
            pressed: b.state == PressState::Pressed,
        }),
        EventKind::Scroll(s) => Some(Wire::Wheel {
            horizontal: s.delta_x,
            vertical: s.delta_y,
        }),
        EventKind::Motion(m) => Some(Wire::Pointer {
            dx: m.delta_x,
            dy: m.delta_y,
        }),
        EventKind::CaptureInterrupted => Some(Wire::ReleaseAll),
        _ => None,
    }
}
pub fn inject(event: &crate::input::InputEvent, injector: &mut Injector) -> Result<(), String> {
    use crate::input::{InputEvent as W, MouseButton as B};
    let r = match event {
        W::Key { code, pressed } => {
            injector.physical_key(*code, PressState::from_pressed(*pressed))
        }
        W::Button { button, pressed } => {
            let b = match button {
                B::Left => Button::Left,
                B::Right => Button::Right,
                B::Middle => Button::Middle,
                B::Back => Button::Back,
                B::Forward => Button::Forward,
            };
            injector.button(b, PressState::from_pressed(*pressed))
        }
        W::Wheel {
            horizontal,
            vertical,
        } => injector.scroll(*horizontal, *vertical),
        W::Pointer { dx, dy } => injector.move_relative(*dx, *dy),
        W::ReleaseAll | W::Enter { .. } => return Ok(()),
    };
    r.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(code: inputs::KeyCode, pressed: bool, ctrl_alt: bool) -> Event {
        Event::new(
            EventKind::Key(inputs::KeyEvent {
                physical: inputs::PhysicalKey::Code(code),
                logical: None,
                state: PressState::from_pressed(pressed),
                repeat: false,
                modifiers: inputs::Modifiers {
                    control: ctrl_alt,
                    alt: ctrl_alt,
                    ..Default::default()
                },
            }),
            None,
            false,
        )
    }
    #[test]
    fn local_input_remote_hotkey_and_late_releases_are_routed_safely() {
        use inputs::KeyCode;
        let (sender, mut receiver) = mpsc::channel(32);
        *routing().lock().unwrap() = Routing {
            sender: Some(sender),
            connected: true,
            ..Default::default()
        };
        let sink = ForwardSink;
        assert_eq!(
            sink.on_event(&key(KeyCode::KeyA, true, false)),
            EventDisposition::Pass
        );
        assert_eq!(
            sink.on_event(&key(KeyCode::KeyA, false, false)),
            EventDisposition::Pass
        );
        assert!(receiver.try_recv().is_err());
        routing().lock().unwrap().remote = true;
        for code in [KeyCode::ControlLeft, KeyCode::AltLeft] {
            assert_eq!(
                sink.on_event(&key(code, true, true)),
                EventDisposition::Suppress
            );
            assert!(matches!(
                receiver.try_recv(),
                Ok(Captured::Input(InputEvent::Key { pressed: true, .. }))
            ));
        }
        assert_eq!(
            sink.on_event(&key(KeyCode::F12, true, true)),
            EventDisposition::Suppress
        );
        assert!(matches!(receiver.try_recv(), Ok(Captured::Local)));
        assert!(!routing().lock().unwrap().remote);
        for code in [KeyCode::ControlLeft, KeyCode::AltLeft, KeyCode::F12] {
            assert_eq!(
                sink.on_event(&key(code, false, false)),
                EventDisposition::Suppress
            );
        }
        assert_eq!(
            sink.on_event(&key(KeyCode::KeyB, true, false)),
            EventDisposition::Pass
        );
        assert!(receiver.try_recv().is_err());
        // Failure must immediately stop suppressing newly captured input.
        drop(receiver);
        routing().lock().unwrap().remote = true;
        assert_eq!(
            sink.on_event(&key(KeyCode::KeyC, true, false)),
            EventDisposition::Pass
        );
        assert!(!routing().lock().unwrap().remote);
        assert!(take_capture_overflow());
        let (sender, mut receiver) = mpsc::channel(32);
        let bounds = Rect {
            x: 0.,
            y: 0.,
            width: 1920.,
            height: 1080.,
        };
        *routing().lock().unwrap() = Routing {
            sender: Some(sender),
            remote: true,
            monitors: vec![bounds],
            ..Default::default()
        };
        update_monitors(vec![bounds]);
        assert!(routing().lock().unwrap().remote);
        assert!(receiver.try_recv().is_err());
        let changed = Rect {
            width: 2560.,
            ..bounds
        };
        update_monitors(vec![changed]);
        assert!(!routing().lock().unwrap().remote);
        assert!(matches!(receiver.try_recv(), Ok(Captured::Local)));
        assert_eq!(routing().lock().unwrap().monitors, vec![changed]);
        *routing().lock().unwrap() = Routing::default();
    }
}
