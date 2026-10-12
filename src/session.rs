//! Duplex transport has one persistent reader. Cancelling a partially read
//! encrypted frame when input/heartbeat wins select would corrupt the stream.
use crate::{
    desktop,
    handoff::RemotePointer,
    input::{self, Frame, InputEvent, InputState, SessionCipher},
    platform_input::{self, Captured},
};
use tokio::{
    net::TcpStream,
    sync::{mpsc, oneshot},
    time::{Duration, timeout},
};
const IDLE: Duration = Duration::from_secs(3);
const WRITE: Duration = Duration::from_secs(1);

fn reader(
    stream: tokio::net::tcp::OwnedReadHalf,
    mut cipher: SessionCipher,
) -> (
    mpsc::Receiver<Result<Frame, String>>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = mpsc::channel(64);
    let task = tokio::spawn(async move {
        let mut stream = stream;
        loop {
            let frame = match timeout(IDLE, input::read_secure(&mut stream, &mut cipher)).await {
                Ok(result) => result.map_err(|e| e.to_string()),
                Err(_) => Err("Связь потеряна: Windows вернёт локальное управление".into()),
            };
            let failed = frame.is_err();
            if tx.send(frame).await.is_err() || failed {
                break;
            }
        }
    });
    (rx, task)
}
async fn send(
    stream: &mut tokio::net::tcp::OwnedWriteHalf,
    cipher: &mut SessionCipher,
    frame: Frame,
) -> Result<(), String> {
    timeout(WRITE, input::write_secure(stream, cipher, &frame))
        .await
        .map_err(|_| "Таймаут отправки ввода".to_string())?
        .map_err(|e| e.to_string())
}
fn valid_position(position: f64) -> bool {
    position.is_finite() && (0.0..=1.0).contains(&position)
}
fn release(state: &mut InputState, injector: &mut inputs::inject::Injector) {
    for event in state.release_all() {
        let _ = platform_input::inject(&event, injector);
    }
}

/// Preserve every delta in geometry, but only post the latest position to the
/// OS. Flush before buttons/keys/wheel so drag and click ordering is unchanged.
#[derive(Default)]
struct PointerOutput {
    pending: Option<(f64, f64)>,
}
impl PointerOutput {
    fn update(&mut self, x: f64, y: f64) {
        self.pending = Some((x, y));
    }
    fn flush(
        &mut self,
        mut post: impl FnMut(f64, f64) -> Result<(), String>,
    ) -> Result<bool, String> {
        if let Some((x, y)) = self.pending {
            post(x, y)?;
            self.pending = None;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[derive(Default)]
struct MotionMetrics {
    received: u64,
    posted: u64,
    peak_queue: usize,
    previous: Option<std::time::Instant>,
    max_gap_ms: f64,
    max_post_ms: f64,
}
impl MotionMetrics {
    fn receive(&mut self, queued: usize) {
        let now = std::time::Instant::now();
        if let Some(previous) = self.previous {
            self.max_gap_ms = self
                .max_gap_ms
                .max(now.duration_since(previous).as_secs_f64() * 1000.);
        }
        self.previous = Some(now);
        self.received += 1;
        self.peak_queue = self.peak_queue.max(queued);
    }
    fn report(&mut self) {
        if self.received > 0 {
            crate::diagnostics::record(
                "motion_metrics",
                &serde_json::json!({
                    "received":self.received,"posted":self.posted,"peak_queue":self.peak_queue,
                    "max_gap_ms":self.max_gap_ms,"max_post_ms":self.max_post_ms
                })
                .to_string(),
            );
        }
        *self = Self::default();
    }
}
fn flush_pointer(
    output: &mut PointerOutput,
    state: &InputState,
    metrics: &mut MotionMetrics,
) -> Result<(), String> {
    let started = std::time::Instant::now();
    if output.flush(|x, y| desktop::move_pointer(x, y, state.drag_button()))? {
        metrics.posted += 1;
        metrics.max_post_ms = metrics
            .max_post_ms
            .max(started.elapsed().as_secs_f64() * 1000.);
    }
    Ok(())
}

pub async fn receiver(
    stream: TcpStream,
    shared: [u8; 32],
    mut stop: oneshot::Receiver<()>,
    backend: std::sync::Arc<crate::Backend>,
) -> Result<(), String> {
    stream.set_nodelay(true).map_err(|e| e.to_string())?;
    let mut injector = inputs::inject::Injector::new()
        .map_err(|e| format!("Создание устройства ввода Mac: {e}"))?;
    let mut bounds = desktop::screens()?
        .into_iter()
        .find(|s| s.primary)
        .ok_or("Не найден основной экран Mac")?
        .bounds;
    let (read, mut write) = stream.into_split();
    let (mut incoming, reader) = reader(read, SessionCipher::new(shared, true)?);
    let mut cipher = SessionCipher::new(shared, true)?;
    let mut state = InputState::default();
    let mut pointer: Option<RemotePointer> = None;
    let mut output = PointerOutput::default();
    let mut metrics = MotionMetrics::default();
    let mut pointer_tick = tokio::time::interval(Duration::from_millis(4));
    pointer_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut metrics_tick = tokio::time::interval(Duration::from_secs(5));
    let mut sequence = 0;
    let mut logged_first_motion = false;
    let mut last = 0;
    let mut heartbeat = tokio::time::interval(Duration::from_millis(500));
    let mut screen_refresh = tokio::time::interval(Duration::from_secs(2));
    let mut last_layout = Vec::new();
    crate::engine_state(&backend, "Подключено · курсор на Windows").await;
    let result=async{
        loop {
            tokio::select!{
                _=pointer_tick.tick()=>{flush_pointer(&mut output,&state,&mut metrics)?;},
                _=metrics_tick.tick()=>{metrics.report();},
                _=&mut stop=>{let _=send(&mut write,&mut cipher,Frame::Disconnect{reason:"stopped by user".into()}).await;break;},
                _=heartbeat.tick()=>{desktop::check_receive_permission(false)?; sequence+=1;send(&mut write,&mut cipher,Frame::KeepAlive{sequence}).await?;},
                _=screen_refresh.tick()=>{
                    let displays=desktop::screens()?;
                    if displays!=last_layout {
                        let next_bounds=displays.iter().find(|s|s.primary).ok_or("Нет основного экрана Mac")?.bounds;
                        if next_bounds!=bounds {
                            if let Some(p)=pointer.take() {
                                flush_pointer(&mut output,&state,&mut metrics)?;
                                metrics.previous=None;
                                release(&mut state,&mut injector);
                                let position=((p.x-p.bounds.x)/(p.bounds.width-1.0).max(1.0)).clamp(0.0,1.0);
                                send(&mut write,&mut cipher,Frame::ReturnControl{position}).await?;
                            }
                            bounds=next_bounds;
                        }
                        backend.state.write().await.layout=displays.clone();
                        last_layout=displays.clone();
                        send(&mut write,&mut cipher,Frame::ScreenLayout{displays}).await?;
                    }
                },
                frame=incoming.recv()=>{
                    match frame.ok_or("Канал закрыт")?? {
                        Frame::Input{sequence,event} if sequence>last=>{
                            last=sequence;
                            match event {
                                InputEvent::Enter{position} if valid_position(position)=>{
                                    flush_pointer(&mut output,&state,&mut metrics)?;
                                    metrics.previous=None;
                                    release(&mut state,&mut injector);
                                    let p=RemotePointer::enter(bounds,position);
                                    logged_first_motion = false;
                                    desktop::move_pointer(p.x,p.y,None)?;
                                    pointer=Some(p);
                                    crate::engine_state(&backend, "Управление на Mac · вверх для возврата").await;
                                },
                                InputEvent::ReleaseAll=>{
                                    flush_pointer(&mut output,&state,&mut metrics)?;
                                    metrics.previous=None;
                                    release(&mut state,&mut injector); pointer=None;
                                    crate::engine_state(&backend, "Подключено · курсор на Windows").await;
                                },
                                InputEvent::Pointer{dx,dy} if dx.is_finite() && dy.is_finite() && dx.abs()<=100000. && dy.abs()<=100000.=>{
                                    if let Some(p)=pointer.as_mut(){
                                        metrics.receive(incoming.len());
                                        if !logged_first_motion { crate::diagnostics::record("first_pointer", "Received Windows pointer movement"); logged_first_motion = true; }
                                        if let Some(position)=p.motion(dx,dy){
                                            flush_pointer(&mut output,&state,&mut metrics)?;
                                            metrics.previous=None;
                                            crate::diagnostics::record("return_control", "Mac top edge crossed");
                                            release(&mut state,&mut injector); pointer=None;
                                            send(&mut write,&mut cipher,Frame::ReturnControl{position}).await?;
                                            crate::engine_state(&backend, "Подключено · курсор на Windows").await;
                                        } else {output.update(p.x,p.y);}
                                    }
                                },
                                event @ (InputEvent::Key{..}|InputEvent::Button{..}|InputEvent::Wheel{..})=>{
                                    if let Some(p)=pointer.as_ref(){
                                        flush_pointer(&mut output,&state,&mut metrics)?;
                                        if let InputEvent::Wheel{horizontal,vertical}=&event
                                            && (!horizontal.is_finite() || !vertical.is_finite()) {
                                            return Err("Некорректная прокрутка".into());
                                        }
                                        // Toggle keys must not toggle repeatedly on Windows auto-repeat.
                                        if state.repeated_lock_press(&event) { continue; }
                                        // Track before injection so cleanup includes an OS call that failed.
                                        state.track(&event);
                                        if let InputEvent::Button{button,pressed}=event {
                                            desktop::button_pointer(p.x,p.y,button,pressed)?;
                                        } else {platform_input::inject(&event,&mut injector)?;}
                                    }
                                },
                                _=>return Err("Некорректное событие ввода".into()),
                            }
                        },
                        Frame::ScreenLayout{displays} if desktop::validate_screens(&displays)=>{
                            crate::diagnostics::record("peer_layout", &serde_json::to_string(&displays).unwrap_or_default());
                            backend.state.write().await.peer_layout=displays;
                        },
                        Frame::KeepAlive{..}=>{},
                        Frame::Disconnect{reason}=>return Err(format!("Другой компьютер отключился: {reason}")),
                        _=>return Err("Некорректный пакет с Windows".into()),
                    }
                }
            }
        }
        Ok(())
    }.await;
    reader.abort();
    metrics.report();
    release(&mut state, &mut injector);
    backend.state.write().await.peer_layout.clear();
    result
}

pub async fn source(
    stream: TcpStream,
    shared: [u8; 32],
    backend: std::sync::Arc<crate::Backend>,
) -> Result<(), String> {
    stream.set_nodelay(true).map_err(|e| e.to_string())?;
    let mut captured = backend
        .capture
        .lock()
        .unwrap()
        .as_mut()
        .and_then(|c| c.take_receiver())
        .ok_or("Захват остановлен")?;
    // No input is queued while local. Drop any stale events from a prior session.
    while captured.try_recv().is_ok() {}
    let percent = backend.state.read().await.handoff_percent;
    platform_input::configure_session(percent);
    crate::engine_state(&backend, "Подключено · курсор на Windows").await;
    let (read, mut write) = stream.into_split();
    let (mut incoming, reader) = reader(read, SessionCipher::new(shared, false)?);
    let mut cipher = SessionCipher::new(shared, false)?;
    let mut sequence = 0;
    let mut logged_first_motion = false;
    let mut heartbeat = tokio::time::interval(Duration::from_millis(500));
    let mut screen_refresh = tokio::time::interval(Duration::from_secs(2));
    let mut last_layout = Vec::new();
    let result=async{
        loop {
            if platform_input::take_capture_overflow(){return Err("Очередь ввода переполнена; управление возвращено Windows".into());}
            tokio::select!{
                _=heartbeat.tick()=>{sequence+=1;send(&mut write,&mut cipher,Frame::KeepAlive{sequence}).await?;},
                _=screen_refresh.tick()=>{
                    let displays=desktop::screens()?;
                    if displays!=last_layout {
                        platform_input::update_monitors(displays.iter().map(|s|s.bounds).collect());
                        backend.state.write().await.layout=displays.clone();
                        last_layout=displays.clone();
                        send(&mut write,&mut cipher,Frame::ScreenLayout{displays}).await?;
                    }
                },
                frame=incoming.recv()=>match frame.ok_or("Канал закрыт")?? {
                    Frame::ReturnControl{position} if valid_position(position)=>platform_input::return_local(position),
                    Frame::ScreenLayout{displays} if desktop::validate_screens(&displays)=>{
                        crate::diagnostics::record("peer_layout", &serde_json::to_string(&displays).unwrap_or_default());
                        backend.state.write().await.peer_layout=displays;
                    },
                    Frame::KeepAlive{..}=>{},
                    Frame::Disconnect{reason}=>return Err(format!("Другой компьютер отключился: {reason}")),
                    _=>return Err("Некорректный пакет с Mac".into()),
                },
                event=captured.recv()=>{
                    let event=match event.ok_or("Захват остановлен")? {
                        Captured::Input(event)=>event,
                        Captured::Local=>InputEvent::ReleaseAll,
                    };
                    if matches!(event,InputEvent::Enter{..}){logged_first_motion=false;crate::engine_state(&backend, "Управление на Mac · Ctrl+Alt+F12 для возврата").await;}
                    if matches!(event,InputEvent::ReleaseAll){crate::engine_state(&backend, "Подключено · курсор на Windows").await;}
                    if matches!(event, InputEvent::Pointer{..}) && !logged_first_motion { crate::diagnostics::record("first_pointer", "Sending pointer movement to Mac"); logged_first_motion=true; }
                    sequence+=1;send(&mut write,&mut cipher,Frame::Input{sequence,event}).await?;
                }
            }
        }
    }.await;
    // Release local suppression before awaiting any network operation.
    platform_input::end_session();
    sequence += 1;
    let _ = send(
        &mut write,
        &mut cipher,
        Frame::Input {
            sequence,
            event: InputEvent::ReleaseAll,
        },
    )
    .await;
    reader.abort();
    if let Some(capture) = backend.capture.lock().unwrap().as_mut() {
        capture.restore_receiver(captured);
    }
    backend.state.write().await.peer_layout.clear();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{io::AsyncWriteExt, net::TcpListener};
    async fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, server) = tokio::join!(
            TcpStream::connect(listener.local_addr().unwrap()),
            listener.accept()
        );
        (client.unwrap(), server.unwrap().0)
    }
    #[test]
    fn pointer_bursts_keep_geometry_and_flush_before_drag_changes() {
        let bounds = crate::handoff::Rect {
            x: 0.,
            y: 0.,
            width: 100.,
            height: 100.,
        };
        let mut pointer = RemotePointer::enter(bounds, 0.5);
        let mut output = PointerOutput::default();
        let mut posted = Vec::new();
        // Clamp each individual delta: summing first would yield a different
        // endpoint after hitting the right boundary and reversing direction.
        assert!(pointer.motion(200., 10.).is_none());
        output.update(pointer.x, pointer.y);
        assert!(pointer.motion(-20., 10.).is_none());
        output.update(pointer.x, pointer.y);
        assert_eq!(pointer.x, 79.);
        output
            .flush(|x, y| {
                posted.push((x, y, false));
                Ok(())
            })
            .unwrap();
        assert_eq!(posted, vec![(79., 23., false)]);
        assert!(
            !output
                .flush(|_, _| panic!("Idle timer must not post"))
                .unwrap()
        );
        // A button transition flushes the pending motion using the old drag
        // state; the subsequent motion belongs to the new button state.
        pointer.motion(-10., 10.);
        output.update(pointer.x, pointer.y);
        output
            .flush(|x, y| {
                posted.push((x, y, false));
                Ok(())
            })
            .unwrap();
        pointer.motion(-10., 10.);
        output.update(pointer.x, pointer.y);
        output
            .flush(|x, y| {
                posted.push((x, y, true));
                Ok(())
            })
            .unwrap();
        assert_eq!(
            posted,
            vec![(79., 23., false), (69., 33., false), (59., 43., true)]
        );
    }

    #[test]
    fn failed_pointer_post_keeps_pending_position() {
        let mut output = PointerOutput::default();
        output.update(10., 20.);
        assert!(output.flush(|_, _| Err("OS error".into())).is_err());
        assert_eq!(output.pending, Some((10., 20.)));
    }

    #[tokio::test]
    async fn reader_preserves_fragmented_frames_while_other_events_win_select() {
        let (mut sender, receiver) = pair().await;
        let (read, _write) = receiver.into_split();
        let shared = [17; 32];
        let (mut incoming, task) = reader(read, SessionCipher::new(shared, false).unwrap());
        let mut cipher = SessionCipher::new(shared, true).unwrap();
        let body = serde_json::to_vec(&Frame::Input {
            sequence: 1,
            event: InputEvent::Enter { position: 0.25 },
        })
        .unwrap();
        let sealed = cipher.seal(&body).unwrap();
        sender.write_u32(sealed.len() as u32).await.unwrap();
        sender.write_all(&sealed[..5]).await.unwrap();
        tokio::select! {
            _=incoming.recv()=>panic!("A partial frame must stay in the persistent reader"),
            _=tokio::time::sleep(Duration::from_millis(20))=>{},
        }
        sender.write_all(&sealed[5..]).await.unwrap();
        assert!(matches!(
            incoming.recv().await,
            Some(Ok(Frame::Input {
                sequence: 1,
                event: InputEvent::Enter { position: 0.25 }
            }))
        ));
        input::write_secure(&mut sender, &mut cipher, &Frame::KeepAlive { sequence: 2 })
            .await
            .unwrap();
        assert!(matches!(
            incoming.recv().await,
            Some(Ok(Frame::KeepAlive { sequence: 2 }))
        ));
        drop(sender);
        assert!(incoming.recv().await.unwrap().is_err());
        task.await.unwrap();
    }
    #[tokio::test]
    async fn unresponsive_peer_times_out_even_with_connection_open() {
        let (_sender, receiver) = pair().await;
        let (read, _write) = receiver.into_split();
        let (mut incoming, task) = reader(read, SessionCipher::new([19; 32], false).unwrap());
        assert!(
            timeout(Duration::from_secs(4), incoming.recv())
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        task.await.unwrap();
    }
    #[test]
    fn positions_must_be_finite_and_normalized() {
        assert!(valid_position(0.));
        assert!(valid_position(1.));
        for x in [f64::NAN, f64::INFINITY, -0.01, 1.01] {
            assert!(!valid_position(x));
        }
    }
}
