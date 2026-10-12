#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tauri::Manager;
use tokio::sync::RwLock;
use uuid::Uuid;

const PORT: u16 = input::INPUT_PORT;
const SERVICE_TYPE: &str = "_onekeymouse._tcp.local.";

#[derive(Clone, Serialize, Deserialize)]
struct AppState {
    id: Uuid,
    name: String,
    platform: String,
    engine_state: String,
    handoff_percent: f32,
    local_addresses: Vec<String>,
    layout: Vec<desktop::ScreenInfo>,
    peer_layout: Vec<desktop::ScreenInfo>,
    display_error: Option<String>,
}

struct Backend {
    state: RwLock<AppState>,
    worker: tokio::sync::Mutex<Option<EngineWorker>>,
    capture: std::sync::Mutex<Option<platform_input::InputCapture>>,
    pending_pair: std::sync::Mutex<Option<PendingPair>>,
    session_busy: Arc<AtomicBool>,
}

async fn engine_state(backend: &Backend, message: impl Into<String>) {
    let message = message.into();
    diagnostics::record("state", &message);
    backend.state.write().await.engine_state = message;
}

struct SessionBusy(Arc<AtomicBool>);
impl Drop for SessionBusy {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

struct EngineWorker {
    task: tokio::task::JoinHandle<()>,
    stop: tokio::sync::oneshot::Sender<()>,
}

struct PendingPair {
    device_id: Uuid,
    device_name: String,
    platform: String,
    peer_key: Vec<u8>,
    tx: tokio::sync::oneshot::Sender<bool>,
}
#[derive(Clone, Serialize)]
struct PairNotice {
    device_id: Uuid,
    device_name: String,
    platform: String,
    fingerprint: String,
}
#[derive(Deserialize)]
struct StartRequest {
    peer_host: Option<String>,
    approve: Option<Uuid>,
    accept: Option<bool>,
}

#[derive(Deserialize)]
struct LayoutRequest {
    handoff_percent: f32,
}

#[derive(Serialize)]
struct Peer {
    name: String,
    platform: String,
    host: String,
    port: u16,
    id: String,
}

mod desktop;
mod diagnostics;
mod handoff;
mod input;
mod platform_input;
mod session;
#[cfg(target_os = "windows")]
mod windows_motion;

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            if let Err(error) = diagnostics::init(app.path().app_log_dir()?) {
                eprintln!("Cannot initialize diagnostics: {error}");
            }
            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
            std::thread::Builder::new()
                .name("onekeymouse-backend".into())
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()
                        .expect("create backend runtime");
                    runtime.block_on(run_backend(ready_tx));
                })?;
            let port = ready_rx.recv().map_err(std::io::Error::other)?;
            tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::External(format!("http://127.0.0.1:{port}").parse()?),
            )
            .title("OneKeyMouse")
            .inner_size(980.0, 900.0)
            .min_inner_size(720.0, 640.0)
            .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running OneKeyMouse");
}

async fn run_backend(ready: std::sync::mpsc::SyncSender<u16>) {
    let platform = if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "Development"
    };
    let initial_state = AppState {
        id: Uuid::new_v4(),
        name: hostname(),
        platform: platform.into(),
        engine_state: "Остановлен".into(),
        handoff_percent: 42.9,
        local_addresses: local_addresses(),
        layout: desktop::screens().unwrap_or_default(),
        peer_layout: Vec::new(),
        display_error: None,
    };

    advertise_mdns(&initial_state);
    let server_state = initial_state.clone();
    let state = Arc::new(Backend {
        state: RwLock::new(initial_state),
        worker: tokio::sync::Mutex::new(None),
        capture: std::sync::Mutex::new(None),
        pending_pair: std::sync::Mutex::new(None),
        session_busy: Arc::new(AtomicBool::new(false)),
    });
    let input_state = state.clone();
    tokio::spawn(async move {
        input_server(input_state, server_state).await;
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route(
            "/api/diagnostics",
            get(|| async { Json(diagnostics::snapshot()) }),
        )
        .route("/api/peers", get(peers))
        .route("/api/layout", get(layout))
        .route("/api/layout/handoff", post(set_handoff))
        .route("/api/engine/start", post(start_engine))
        .route("/api/engine/stop", post(stop_engine))
        .route("/api/pair/approve", post(approve_pair))
        .route("/api/pair/pending", get(pair_pending))
        .with_state(state);

    // Bind an ephemeral loopback port so multiple user sessions cannot collide.
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind server");
    let port = listener.local_addr().expect("read listener address").port();
    let _ = ready.send(port);
    axum::serve(listener, app).await.expect("serve app");
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}
async fn status(State(state): State<Arc<Backend>>) -> Json<AppState> {
    refresh_display_state(&state).await;
    Json(state.state.read().await.clone())
}
async fn layout(State(state): State<Arc<Backend>>) -> Json<Vec<desktop::ScreenInfo>> {
    refresh_display_state(&state).await;
    Json(state.state.read().await.layout.clone())
}
async fn refresh_display_state(backend: &Backend) {
    let result = desktop::screens();
    let mut state = backend.state.write().await;
    match result {
        Ok(screens) => {
            state.layout = screens;
            state.display_error = None;
        }
        Err(error) => {
            state.layout.clear();
            state.display_error = Some(error);
        }
    }
}
async fn set_handoff(
    State(state): State<Arc<Backend>>,
    Json(request): Json<LayoutRequest>,
) -> impl IntoResponse {
    if !request.handoff_percent.is_finite() || !(0.0..=99.0).contains(&request.handoff_percent) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok":false,"message":"Значение должно быть от 0 до 99%."})),
        )
            .into_response();
    }
    state.state.write().await.handoff_percent = request.handoff_percent;
    platform_input::update_percent(request.handoff_percent);
    Json(serde_json::json!({"ok":true})).into_response()
}
async fn pair_pending(State(state): State<Arc<Backend>>) -> Json<Option<PairNotice>> {
    let pending = state.pending_pair.lock().unwrap();
    Json(pending.as_ref().map(|p| PairNotice {
        device_id: p.device_id,
        device_name: p.device_name.clone(),
        platform: p.platform.clone(),
        fingerprint: hex_string(&sha256(&p.peer_key)),
    }))
}
async fn approve_pair(
    State(state): State<Arc<Backend>>,
    Json(request): Json<StartRequest>,
) -> impl IntoResponse {
    let Some(id) = request.approve else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok":false,"message":"Не выбран запрос на сопряжение."})),
        )
            .into_response();
    };
    let pending = state.pending_pair.lock().unwrap().take();
    if let Some(p) = pending {
        if p.device_id == id {
            let accepted = request.accept.unwrap_or(true);
            let _ = p.tx.send(accepted);
            return Json(serde_json::json!({"ok":true,"message":if accepted{"Компьютер подтверждён."}else{"Запрос отклонён."}})).into_response();
        } else {
            *state.pending_pair.lock().unwrap() = Some(p);
        }
    }
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"ok":false,"message":"Запрос больше не ожидает подтверждения."})),
    )
        .into_response()
}

async fn peers(State(state): State<Arc<Backend>>) -> Json<Vec<Peer>> {
    let id = state.state.read().await.id;
    Json(
        tokio::task::spawn_blocking(move || discover_peers(id))
            .await
            .unwrap_or_default(),
    )
}

async fn start_engine(
    State(backend): State<Arc<Backend>>,
    Json(request): Json<StartRequest>,
) -> impl IntoResponse {
    let snapshot = backend.state.read().await.clone();
    let is_windows = snapshot.platform == "Windows";
    if !is_windows {
        let host = request.peer_host.as_deref().unwrap_or("").trim();
        let valid = host
            .parse::<std::net::Ipv4Addr>()
            .map(|ip| ip.is_private())
            .unwrap_or(false);
        if !valid {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok":false,"message":"Укажи частный IPv4-адрес Windows-компьютера в этой локальной сети."}))).into_response();
        }
    }
    if !is_windows && let Err(message) = desktop::check_receive_permission(true) {
        engine_state(&backend, message.clone()).await;
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"ok":false,"message":message})),
        )
            .into_response();
    }
    if is_windows {
        let capture = match platform_input::start_capture() {
            Ok(c) => c,
            Err(error) => {
                engine_state(&backend, format!("Захват Windows: {error}")).await;
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({"ok":false,"message":error})),
                )
                    .into_response();
            }
        };
        *backend.capture.lock().unwrap() = Some(capture);
        engine_state(&backend, "Ожидание подключения Mac").await;
        return (StatusCode::OK,Json(serde_json::json!({"ok":true,"message":"Захват Windows включён. На Mac укажи IP Windows и нажми «Подключиться». После сопряжения веди курсор вниз через настроенный участок края. Ctrl+Alt+F12 возвращает управление Windows.","port":input::INPUT_PORT}))).into_response();
    }
    let host = request.peer_host.unwrap_or_default();
    let addr = match input::peer_addr(host.trim(), input::INPUT_PORT) {
        Ok(addr) => addr,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"ok":false,"message":message})),
            )
                .into_response();
        }
    };
    let worker_state = backend.clone();
    let device_id = worker_state.state.read().await.id;
    let device_name = hostname();
    let platform = std::env::consts::OS.to_owned();
    {
        let mut worker_slot = backend.worker.lock().await;
        if worker_slot
            .as_ref()
            .is_some_and(|worker| !worker.task.is_finished())
        {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"ok":false,"message":"Подключение уже выполняется."})),
            )
                .into_response();
        }
        *worker_slot = None;
    }
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    engine_state(&backend, format!("Подключение к {addr}")).await;
    let worker = tokio::spawn(async move {
        let outcome: Result<(), String> = async {
            diagnostics::record("connect", &addr.to_string());
            let mut stream =
                tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(addr))
                    .await
                    .map_err(|_| "Таймаут подключения к Windows".to_string())?
                    .map_err(|e| format!("Подключение к Windows: {e}"))?;
            diagnostics::record("tcp_connected", &addr.to_string());
            let local = input::EphemeralKey::generate();
            let challenge = input::random_challenge();
            input::write_frame(
                &mut stream,
                &input::Frame::PairRequest {
                    version: input::PROTOCOL_VERSION,
                    device_id,
                    device_name,
                    platform,
                    public_key: local.public().to_vec(),
                    challenge: challenge.to_vec(),
                },
            )
            .await
            .map_err(|e| format!("Отправка запроса сопряжения: {e}"))?;
            engine_state(&worker_state, "Ожидание подтверждения на Windows").await;
            let accepted =
                tokio::time::timeout(Duration::from_secs(65), input::read_frame(&mut stream))
                    .await
                    .map_err(|_| "Истекло время подтверждения на Windows".to_string())?
                    .map_err(|e| format!("Ответ сопряжения: {e}"))?;
            let (public_key, proof) = match accepted {
                input::Frame::PairAccept {
                    version,
                    public_key,
                    proof,
                    ..
                } if version == input::PROTOCOL_VERSION && public_key.len() == 32 => {
                    (public_key, proof)
                }
                input::Frame::Disconnect { reason } => return Err(format!("Windows: {reason}")),
                _ => return Err("Несовместимый ответ сопряжения; обнови оба приложения".into()),
            };
            let shared = local.agree(public_key.try_into().unwrap())?;
            if proof.as_slice() != input::session_proof(&shared, &challenge).as_slice() {
                return Err("Не прошла проверка Windows".into());
            }
            let mut confirm_context = challenge.to_vec();
            confirm_context.extend_from_slice(b"confirm");
            input::write_frame(
                &mut stream,
                &input::Frame::PairConfirm {
                    proof: input::session_proof(&shared, &confirm_context).to_vec(),
                },
            )
            .await
            .map_err(|e| format!("Подтверждение сопряжения: {e}"))?;
            diagnostics::record("paired", "Windows confirmed; starting receiver");
            session::receiver(stream, shared, stop_rx, worker_state.clone()).await
        }
        .await;
        engine_state(
            &worker_state,
            match outcome {
                Ok(()) => "Остановлен пользователем".into(),
                Err(error) => format!("Остановлен: {error}"),
            },
        )
        .await;
    });
    *backend.worker.lock().await = Some(EngineWorker {
        task: worker,
        stop: stop_tx,
    });
    (StatusCode::OK,Json(serde_json::json!({"ok":true,"message":"Подключаюсь к Windows. Первый запуск запросит разрешение macOS на управление компьютером.","port":input::INPUT_PORT}))).into_response()
}

async fn input_server(backend: Arc<Backend>, snapshot: AppState) {
    let listener =
        match tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], input::INPUT_PORT)))
            .await
        {
            Ok(listener) => listener,
            Err(error) => {
                diagnostics::record("listen_failed", &error.to_string());
                return;
            }
        };
    diagnostics::record("listening", &input::INPUT_PORT.to_string());
    loop {
        let Ok((mut stream, addr)) = listener.accept().await else {
            continue;
        };
        let local = match addr.ip() {
            std::net::IpAddr::V4(ip) => ip.is_private() || ip.is_link_local(),
            std::net::IpAddr::V6(ip) => ip.is_unique_local() || ip.is_unicast_link_local(),
        };
        if !local {
            continue;
        }
        let backend = backend.clone();
        let snapshot = snapshot.clone();
        tokio::spawn(async move {
            use input::Frame;
            diagnostics::record("incoming_connection", &addr.to_string());
            let request =
                match tokio::time::timeout(Duration::from_secs(10), input::read_frame(&mut stream))
                    .await
                {
                    Ok(Ok(Frame::PairRequest {
                        version,
                        device_id,
                        device_name,
                        platform,
                        public_key,
                        challenge,
                    })) if version == input::PROTOCOL_VERSION
                        && public_key.len() == 32
                        && challenge.len() == 32 =>
                    {
                        (device_id, device_name, platform, public_key, challenge)
                    }
                    _ => {
                        diagnostics::record("pair_failed", "Invalid or timed out pairing frame");
                        return;
                    }
                };
            if backend.capture.lock().unwrap().is_none() {
                let _ = input::write_frame(
                    &mut stream,
                    &Frame::Disconnect {
                        reason: {
                            diagnostics::record(
                                "pair_rejected",
                                "Windows input capture is not running",
                            );
                            "Windows input capture is not running".into()
                        },
                    },
                )
                .await;
                return;
            }
            let session_busy = backend.session_busy.clone();
            if session_busy
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                let _ = input::write_frame(
                    &mut stream,
                    &Frame::Disconnect {
                        reason: {
                            diagnostics::record(
                                "pair_rejected",
                                "Windows is already pairing or connected",
                            );
                            "Windows is already pairing or connected".into()
                        },
                    },
                )
                .await;
                return;
            }
            let _session_busy = SessionBusy(session_busy);
            let (device_id, device_name, platform, public_bytes, challenge) =
                (request.0, request.1, request.2, request.3, request.4);
            let pubkey: [u8; 32] = public_bytes.clone().try_into().unwrap();
            let ephemeral = input::EphemeralKey::generate();
            let shared = match ephemeral.agree(pubkey) {
                Ok(k) => k,
                Err(error) => {
                    diagnostics::record("pair_failed", &error.to_string());
                    return;
                }
            };
            let (tx, rx) = tokio::sync::oneshot::channel();
            let already_pending = {
                let mut pending = backend.pending_pair.lock().unwrap();
                if pending.is_some() {
                    true
                } else {
                    *pending = Some(PendingPair {
                        device_id,
                        device_name,
                        platform,
                        peer_key: public_bytes,
                        tx,
                    });
                    false
                }
            };
            if already_pending {
                let _ = input::write_frame(
                    &mut stream,
                    &Frame::Disconnect {
                        reason: {
                            diagnostics::record("pair_rejected", "pairing request already pending");
                            "pairing request already pending".into()
                        },
                    },
                )
                .await;
                return;
            }
            diagnostics::record("pair_pending", "Waiting for Windows approval");
            let approved = matches!(
                tokio::time::timeout(Duration::from_secs(60), rx).await,
                Ok(Ok(true))
            );
            if !approved {
                {
                    let mut pending = backend.pending_pair.lock().unwrap();
                    if pending.as_ref().is_some_and(|p| p.device_id == device_id) {
                        *pending = None;
                    }
                }
                let _ = input::write_frame(
                    &mut stream,
                    &Frame::Disconnect {
                        reason: {
                            diagnostics::record("pair_rejected", "pairing declined or timed out");
                            "pairing declined or timed out".into()
                        },
                    },
                )
                .await;
                return;
            }
            if backend.capture.lock().unwrap().is_none() {
                let _ = input::write_frame(
                    &mut stream,
                    &Frame::Disconnect {
                        reason: {
                            diagnostics::record(
                                "pair_rejected",
                                "Windows input capture was stopped",
                            );
                            "Windows input capture was stopped".into()
                        },
                    },
                )
                .await;
                return;
            }
            {
                let mut pending = backend.pending_pair.lock().unwrap();
                if pending.as_ref().is_some_and(|p| p.device_id == device_id) {
                    *pending = None;
                }
            }
            let proof = input::session_proof(&shared, &challenge);
            if input::write_frame(
                &mut stream,
                &Frame::PairAccept {
                    version: input::PROTOCOL_VERSION,
                    device_id: snapshot.id,
                    public_key: ephemeral.public().to_vec(),
                    proof: proof.to_vec(),
                },
            )
            .await
            .is_err()
            {
                diagnostics::record("pair_failed", "Cannot send pairing acceptance");
                return;
            }
            let confirm =
                match tokio::time::timeout(Duration::from_secs(5), input::read_frame(&mut stream))
                    .await
                {
                    Ok(Ok(Frame::PairConfirm { proof })) => proof,
                    _ => {
                        diagnostics::record("pair_failed", "Invalid or timed out pairing frame");
                        return;
                    }
                };
            let mut confirm_context = challenge.clone();
            confirm_context.extend_from_slice(b"confirm");
            if confirm.as_slice() != input::session_proof(&shared, &confirm_context).as_slice() {
                diagnostics::record("pair_failed", "Invalid Mac confirmation proof");
                return;
            }
            diagnostics::record("paired", "Mac confirmed; starting capture transport");
            let outcome = session::source(stream, shared, backend.clone()).await;
            engine_state(
                &backend,
                match outcome {
                    Ok(()) => "Ожидание подключения Mac".into(),
                    Err(error) => format!("Остановлен: {error}"),
                },
            )
            .await;
        });
    }
}
fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).into()
}
fn hex_string(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn stop_engine(State(backend): State<Arc<Backend>>) -> impl IntoResponse {
    platform_input::stop_capture();
    if let Some(worker) = backend.worker.lock().await.take() {
        let _ = worker.stop.send(());
        let mut task = worker.task;
        if tokio::time::timeout(Duration::from_secs(2), &mut task)
            .await
            .is_err()
        {
            task.abort();
        }
    }
    backend.capture.lock().unwrap().take();
    *backend.pending_pair.lock().unwrap() = None;
    engine_state(&backend, "Остановлен пользователем").await;
    Json(
        serde_json::json!({"ok":true,"message":"OneKeyMouse остановлен, локальные клавиши освобождены."}),
    )
}

fn discover_peers(local_id: Uuid) -> Vec<Peer> {
    let Ok(daemon) = ServiceDaemon::new() else {
        return vec![];
    };
    let Ok(receiver) = daemon.browse(SERVICE_TYPE) else {
        return vec![];
    };
    let mut peers = Vec::new();
    let local_id = local_id.to_string();
    let deadline = std::time::Instant::now() + Duration::from_millis(1200);
    while std::time::Instant::now() < deadline {
        match receiver.recv_timeout(Duration::from_millis(150)) {
            Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                if info.get_property_val_str("id") == Some(local_id.as_str()) {
                    continue;
                }
                let host = info
                    .get_addresses()
                    .iter()
                    .find(|ip| matches!(ip, std::net::IpAddr::V4(v4) if v4.is_private()))
                    .map(|ip| ip.to_string())
                    .unwrap_or_default();
                if !host.is_empty() {
                    peers.push(Peer {
                        name: info.get_hostname().trim_end_matches('.').into(),
                        platform: info
                            .get_property_val_str("platform")
                            .unwrap_or("Unknown")
                            .into(),
                        host,
                        port: info.get_port(),
                        id: info.get_property_val_str("id").unwrap_or("").into(),
                    });
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    peers
}

fn advertise_mdns(snapshot: &AppState) {
    if let Ok(daemon) = ServiceDaemon::new() {
        let instance = format!("OneKeyMouse-{}", snapshot.name.replace(' ', "-"));
        let id = snapshot.id.to_string();
        let properties = [
            ("platform", snapshot.platform.as_str()),
            ("id", id.as_str()),
        ];
        let addresses: Vec<_> = if_addrs::get_if_addrs()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|iface| {
                let ip = iface.ip();
                if iface.is_loopback() || !matches!(ip, std::net::IpAddr::V4(v4) if v4.is_private())
                {
                    None
                } else {
                    Some(ip)
                }
            })
            .collect();
        if let Ok(info) = ServiceInfo::new(
            SERVICE_TYPE,
            &instance,
            &format!("{}.local.", snapshot.name.replace(' ', "-")),
            &addresses[..],
            PORT,
            &properties[..],
        ) {
            let _ = daemon.register(info);
            std::mem::forget(daemon);
        }
    }
}

fn local_addresses() -> Vec<String> {
    let mut addresses = Vec::new();
    for iface in if_addrs::get_if_addrs().unwrap_or_default() {
        let ip = iface.ip();
        if iface.is_loopback() || !matches!(ip, std::net::IpAddr::V4(v4) if v4.is_private()) {
            continue;
        }
        let text = ip.to_string();
        if !addresses.contains(&text) {
            addresses.push(text);
        }
    }
    addresses
}

fn hostname() -> String {
    #[cfg(target_os = "macos")]
    if let Ok(output) = Command::new("scutil")
        .args(["--get", "LocalHostName"])
        .output()
    {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if output.status.success() && !name.is_empty() {
            return name;
        }
    }
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "My Computer".into())
}
