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
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::sync::RwLock;
use uuid::Uuid;

const PORT: u16 = 47621;
const SERVICE_TYPE: &str = "_onekeymouse._tcp.local.";

#[derive(Clone, Serialize, Deserialize)]
struct AppState {
    id: Uuid,
    name: String,
    platform: String,
    engine_state: String,
    handoff_percent: f32,
    local_addresses: Vec<String>,
    layout: Vec<ScreenNode>,
}

struct Backend {
    state: RwLock<AppState>,
    engine: tokio::sync::Mutex<Option<Child>>,
}

#[derive(Deserialize)]
struct StartRequest {
    peer_host: Option<String>,
}

#[derive(Deserialize)]
struct LayoutRequest {
    handoff_percent: f32,
}

#[derive(Clone, Serialize, Deserialize)]
struct ScreenNode {
    id: String,
    label: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    device: String,
}

#[derive(Serialize)]
struct Peer {
    name: String,
    platform: String,
    host: String,
    port: u16,
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
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
        layout: vec![
            ScreenNode {
                id: "windows-laptop".into(),
                label: "Ноутбук Windows".into(),
                x: 0,
                y: 0,
                width: 900,
                height: 600,
                device: "windows".into(),
            },
            ScreenNode {
                id: "windows-monitor".into(),
                label: "Монитор Windows".into(),
                x: 900,
                y: 0,
                width: 1200,
                height: 675,
                device: "windows".into(),
            },
            ScreenNode {
                id: "mac-monitor".into(),
                label: "Монитор Mac".into(),
                x: 900,
                y: 675,
                width: 1200,
                height: 675,
                device: "mac".into(),
            },
        ],
    };

    advertise_mdns(&initial_state);
    let state = Arc::new(Backend {
        state: RwLock::new(initial_state),
        engine: tokio::sync::Mutex::new(None),
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/peers", get(peers))
        .route("/api/layout", get(layout))
        .route("/api/layout/handoff", post(set_handoff))
        .route("/api/engine/start", post(start_engine))
        .route("/api/engine/stop", post(stop_engine))
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
    let mut engine = state.engine.lock().await;
    let alive = if let Some(child) = engine.as_mut() {
        match child.try_wait() {
            Ok(None) => true,
            _ => {
                *engine = None;
                false
            }
        }
    } else {
        false
    };
    let mut snapshot = state.state.read().await.clone();
    if !alive {
        snapshot.engine_state = "Остановлен".into();
    }
    Json(snapshot)
}
async fn layout(State(state): State<Arc<Backend>>) -> Json<Vec<ScreenNode>> {
    Json(state.state.read().await.layout.clone())
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
    Json(serde_json::json!({"ok":true})).into_response()
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
    let mut engine = backend.engine.lock().await;
    if let Some(child) = engine.as_mut() {
        if child.try_wait().ok().flatten().is_none() {
            return (
                StatusCode::OK,
                Json(serde_json::json!({"ok":true,"message":"Deskflow уже запущен."})),
            )
                .into_response();
        }
        *engine = None;
    }

    let snapshot = backend.state.read().await.clone();
    let windows_source = snapshot.platform == "Windows";
    if !windows_source && request.peer_host.as_deref().unwrap_or("").trim().is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok":false,"message":"Укажи IP Windows-компьютера, например 192.168.1.20."}))).into_response();
    }
    if !windows_source
        && !request
            .peer_host
            .as_deref()
            .unwrap_or("")
            .trim()
            .parse::<std::net::Ipv4Addr>()
            .map(|ip| ip.is_private())
            .unwrap_or(false)
    {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok":false,"message":"Нужен частный IPv4-адрес Windows-компьютера из этой локальной сети."}))).into_response();
    }
    let Some(executable) = find_deskflow_core() else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"ok":false,"message":"Не найден deskflow-core. Установи Deskflow на этом компьютере и перезапусти OneKeyMouse."}))).into_response();
    };

    let config_dir = config_dir();
    if let Err(error) = std::fs::create_dir_all(&config_dir) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"ok":false,"message":format!("Не удалось создать папку настроек: {error}")}))).into_response();
    }
    let settings_path = deskflow_settings_path(&executable);
    let config_path = config_dir.join("server.conf");
    let mut peer_host = request.peer_host.clone().unwrap_or_default();
    let (mode, computer) = if windows_source {
        let edge = snapshot.handoff_percent;
        let server_config = format!(
            "# Generated by OneKeyMouse. Windows lower edge segment connects to Mac.\nsection: screens\n\twindows:\n\tmac:\nend\nsection: links\n\twindows:\n\t\tdown({edge:.1},100) = mac\n\tmac:\n\t\tup = windows({edge:.1},100)\nend\n"
        );
        if let Err(error) = std::fs::write(&config_path, server_config) {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"ok":false,"message":format!("Не удалось записать конфигурацию: {error}")}))).into_response();
        }
        ("server", "windows")
    } else {
        peer_host = peer_host.trim().to_owned();
        ("client", "mac")
    };
    let updates = if windows_source {
        vec![
            ("core", "coreMode", "2".to_owned()),
            ("core", "computerName", "windows".to_owned()),
            ("core", "port", "24800".to_owned()),
            ("security", "tlsEnabled", "true".to_owned()),
            ("security", "checkPeerFingerprints", "true".to_owned()),
            ("server", "externalConfig", "true".to_owned()),
            (
                "server",
                "externalConfigFile",
                config_path.display().to_string(),
            ),
        ]
    } else {
        vec![
            ("core", "coreMode", "1".to_owned()),
            ("core", "computerName", "mac".to_owned()),
            ("core", "port", "24800".to_owned()),
            ("security", "tlsEnabled", "true".to_owned()),
            ("security", "checkPeerFingerprints", "true".to_owned()),
            ("client", "remoteHost", peer_host.clone()),
        ]
    };
    if let Err(error) = write_ini_values(&settings_path, &updates) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"ok":false,"message":format!("Не удалось записать настройки: {error}")}))).into_response();
    }
    let log_path = config_dir.join("engine.log");
    let log_file = match std::fs::File::create(&log_path) {
        Ok(file) => file,
        Err(error) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"ok":false,"message":format!("Не удалось создать лог: {error}")}))).into_response(),
    };
    let stderr = match log_file.try_clone() {
        Ok(file) => file,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"ok":false,"message":error.to_string()})),
            )
                .into_response();
        }
    };
    match Command::new(executable).arg(mode).arg("--settings").arg(&settings_path).stdin(Stdio::null()).stdout(Stdio::from(log_file)).stderr(Stdio::from(stderr)).spawn() {
        Ok(child) => {
            *engine = Some(child);
            let trust_ui = if let Some(gui) = find_deskflow_gui() {
                Command::new(gui).spawn().is_ok()
            } else { false };
            let message = if windows_source { format!("Deskflow server запущен. На Mac укажи IP этого Windows-компьютера. Лог: {}", log_path.display()) } else { format!("Deskflow client запущен для {}. Лог: {}", peer_host, log_path.display()) };
            backend.state.write().await.engine_state = "Запущен".into();
            let message = if trust_ui { format!("{message} Deskflow GUI открыт для системных разрешений и подтверждения доверия." ) } else { format!("{message} При запросе доверия открой Deskflow GUI.") };
            (StatusCode::OK, Json(serde_json::json!({"ok":true,"message":message,"computer":computer}))).into_response()
        }
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"ok":false,"message":format!("Не удалось запустить Deskflow: {error}")}))).into_response(),
    }
}

async fn stop_engine(State(backend): State<Arc<Backend>>) -> impl IntoResponse {
    let mut engine = backend.engine.lock().await;
    if let Some(mut child) = engine.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    backend.state.write().await.engine_state = "Остановлен".into();
    Json(serde_json::json!({"ok":true,"message":"Deskflow остановлен."}))
}

fn find_deskflow_core() -> Option<PathBuf> {
    let name = if cfg!(target_os = "windows") {
        "deskflow-core.exe"
    } else {
        "deskflow-core"
    };
    if let Ok(path) = std::env::var("PATH") {
        for entry in std::env::split_paths(&path) {
            let candidate = entry.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    #[cfg(target_os = "macos")]
    for path in [
        "/Applications/Deskflow.app/Contents/MacOS/deskflow-core",
        "/opt/homebrew/bin/deskflow-core",
    ] {
        let candidate = PathBuf::from(path);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    #[cfg(target_os = "windows")]
    for root in [
        std::env::var("ProgramFiles").ok(),
        std::env::var("ProgramFiles(x86)").ok(),
    ]
    .into_iter()
    .flatten()
    {
        let candidate = PathBuf::from(root).join("Deskflow").join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn find_deskflow_gui() -> Option<PathBuf> {
    let name = if cfg!(target_os = "windows") {
        "deskflow-gui.exe"
    } else {
        "deskflow-gui"
    };
    if let Ok(path) = std::env::var("PATH") {
        for entry in std::env::split_paths(&path) {
            let candidate = entry.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    #[cfg(target_os = "macos")]
    for path in ["/Applications/Deskflow.app/Contents/MacOS/Deskflow"] {
        let candidate = PathBuf::from(path);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    #[cfg(target_os = "windows")]
    for root in [
        std::env::var("ProgramFiles").ok(),
        std::env::var("ProgramFiles(x86)").ok(),
    ]
    .into_iter()
    .flatten()
    {
        let candidate = PathBuf::from(root).join("Deskflow").join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn config_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Library/Application Support/OneKeyMouse");
    }
    #[cfg(target_os = "windows")]
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata).join("OneKeyMouse");
    }
    std::env::temp_dir().join("OneKeyMouse")
}

fn deskflow_settings_path(core_executable: &std::path::Path) -> PathBuf {
    #[cfg(not(target_os = "windows"))]
    let _ = core_executable;
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Library/Deskflow/OneKeyMouse.conf");
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(portable) = core_executable
            .parent()
            .map(|dir| dir.join("settings/Deskflow.conf"))
        {
            if portable.is_file() {
                return portable;
            }
        }
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join("OneKeyMouse/Deskflow.conf");
        }
    }
    config_dir().join("Deskflow.conf")
}

fn write_ini_values(
    path: &std::path::Path,
    updates: &[(&'static str, &'static str, String)],
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut contents = std::fs::read_to_string(path).unwrap_or_default();
    for (section, key, value) in updates {
        contents = set_ini_value(&contents, section, key, value);
    }
    std::fs::write(path, contents)
}

fn set_ini_value(contents: &str, section: &str, key: &str, value: &str) -> String {
    let mut lines: Vec<String> = contents.lines().map(str::to_owned).collect();
    let header = format!("[{section}]");
    let section_start = lines.iter().position(|line| line.trim() == header);
    if let Some(start) = section_start {
        let end = lines
            .iter()
            .enumerate()
            .skip(start + 1)
            .find(|(_, line)| line.trim().starts_with('['))
            .map(|(i, _)| i)
            .unwrap_or(lines.len());
        if let Some(index) = (start + 1..end).find(|&i| {
            lines[i]
                .split_once('=')
                .map(|(name, _)| name.trim() == key)
                .unwrap_or(false)
        }) {
            lines[index] = format!("{key}={value}");
        } else {
            lines.insert(start + 1, format!("{key}={value}"));
        }
    } else {
        if !lines.is_empty() && !lines.last().unwrap().is_empty() {
            lines.push(String::new());
        }
        lines.push(header);
        lines.push(format!("{key}={value}"));
    }
    let mut output = lines.join("\n");
    output.push('\n');
    output
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
