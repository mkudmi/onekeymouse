//! Lifecycle diagnostics only: never record keys, typed text, or session secrets.
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

static LOG: OnceLock<Mutex<PathBuf>> = OnceLock::new();
const MAX_BYTES: u64 = 2 * 1024 * 1024;

pub fn init(directory: PathBuf) -> std::io::Result<()> {
    fs::create_dir_all(&directory)?;
    let path = directory.join("native-engine.log");
    OpenOptions::new().create(true).append(true).open(&path)?;
    let _ = LOG.set(Mutex::new(path));
    record(
        "startup",
        &format!(
            "version={} platform={} pid={}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::process::id()
        ),
    );
    Ok(())
}

pub fn record(event: &str, detail: &str) {
    let Some(log) = LOG.get() else {
        return;
    };
    let path = log.lock().unwrap();
    let result = (|| -> std::io::Result<()> {
        if fs::metadata(&*path).map(|m| m.len()).unwrap_or(0) >= MAX_BYTES {
            let previous = path.with_extension("log.previous");
            if previous.exists() {
                fs::remove_file(&previous)?;
            }
            fs::rename(&*path, previous)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&*path)?;
        let epoch_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        writeln!(
            file,
            "{}",
            serde_json::json!({"epoch_ms":epoch_ms,"event":event,"detail":detail})
        )
    })();
    if let Err(error) = result {
        eprintln!("Cannot write diagnostics: {error}");
    }
}

pub fn snapshot() -> serde_json::Value {
    let Some(log) = LOG.get() else {
        return serde_json::json!({"error":"Журнал недоступен"});
    };
    let path = log.lock().unwrap();
    match fs::read_to_string(&*path) {
        Ok(content) => {
            let mut start = content.len().saturating_sub(64 * 1024);
            while !content.is_char_boundary(start) {
                start += 1;
            }
            serde_json::json!({"path":path.to_string_lossy(),"log":&content[start..]})
        }
        Err(error) => serde_json::json!({"path":path.to_string_lossy(),"error":error.to_string()}),
    }
}
