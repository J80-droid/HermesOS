//! Sidecar-proces (Python stdio JSON-RPC): spawn, defensieve I/O-buffering,
//! stderr-tail voor crash-diagnose, en fatal events naar de frontend.

use crate::log_rotator::LogRotator;
use serde::Serialize;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Runtime};
use tauri_plugin_shell::process::CommandChild;
use tauri_plugin_shell::ShellExt;

/// Maximaal aantal bytes per stdout/stderr-regel voordat we truncaten (OOM-bescherming).
pub const MAX_SIDECAR_LINE_BYTES: usize = 256 * 1024;

/// Bewaar de laatste N stderr-regels voor crash-rapportage aan de UI.
pub const STDERR_TAIL_MAX_LINES: usize = 40;

/// Max wachten op vrijwillige sidecar-exit na stdin-control voordat `kill` wordt toegepast.
pub const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// JSON-RPC control naar stdin (newline-terminated). Python-side EOF op stdin volgt in een latere iteratie.
pub const SHUTDOWN_CONTROL_JSON: &[u8] =
    br#"{"type":"control","command":"shutdown"}"#;

#[derive(Clone)]
pub struct SidecarIoState {
    pub stderr_tail: Arc<Mutex<VecDeque<String>>>,
    /// `true` tijdens normale app-afsluiting — onderdrukt valse "crash" events.
    pub shutdown_requested: Arc<AtomicBool>,
}

impl SidecarIoState {
    pub fn new() -> Self {
        Self {
            stderr_tail: Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_MAX_LINES))),
            shutdown_requested: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SidecarFatalPayload {
    pub exit_code: Option<u32>,
    pub signal: Option<i32>,
    pub last_stderr_lines: Vec<String>,
    pub reason: String,
}

pub fn hermes_os_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR must have parent (HermesOS root)")
        .to_path_buf()
}

fn get_sidecar_entry_path() -> PathBuf {
    hermes_os_root().join("scripts").join("sidecar_entry.py")
}

/// Minimum size (bytes) to treat ``binaries/hermes_agent-*.exe`` as a real PyInstaller build,
/// not an empty placeholder from ``make_placeholder_binary``.
const MIN_PACKAGED_SIDECAR_BYTES: u64 = 512 * 1024;

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn packaged_sidecar_exe_name() -> &'static str {
    "hermes_agent-x86_64-pc-windows-msvc.exe"
}

#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
fn packaged_sidecar_exe_name() -> &'static str {
    "hermes_agent-x86_64-unknown-linux-gnu"
}

fn packaged_sidecar_ready(path: &std::path::Path) -> bool {
    path.metadata()
        .ok()
        .map(|m| m.len() >= MIN_PACKAGED_SIDECAR_BYTES)
        .unwrap_or(false)
}

/// Debug builds: packaged exe only if ``HERMES_USE_PACKAGED_SIDECAR`` is set (test installer bits).
/// Release builds: prefer bundled sidecar whenever the file exists and is not a tiny placeholder.
fn should_prefer_packaged_sidecar(debug_build: bool, exe: &std::path::Path) -> bool {
    if !packaged_sidecar_ready(exe) {
        return false;
    }
    if debug_build {
        matches!(
            std::env::var("HERMES_USE_PACKAGED_SIDECAR").as_deref(),
            Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
        )
    } else {
        true
    }
}

#[derive(Clone, Copy)]
enum SidecarSpawnMode {
    Packaged,
    PythonDev,
}

fn resolve_sidecar_spawn(
    entry_script: &std::path::Path,
    packaged_exe: &std::path::Path,
) -> Result<SidecarSpawnMode, String> {
    let debug_build = cfg!(debug_assertions);

    if should_prefer_packaged_sidecar(debug_build, packaged_exe) {
        tracing::info!("spawn_sidecar: using bundled hermes_agent sidecar");
        return Ok(SidecarSpawnMode::Packaged);
    }
    if entry_script.exists() {
        tracing::info!(
            "spawn_sidecar: using Python interpreter (dev); set HERMES_USE_PACKAGED_SIDECAR=1 to force bundled exe"
        );
        return Ok(SidecarSpawnMode::PythonDev);
    }
    if packaged_sidecar_ready(packaged_exe) {
        tracing::warn!(
            "spawn_sidecar: {} missing — falling back to packaged binary",
            entry_script.display()
        );
        return Ok(SidecarSpawnMode::Packaged);
    }

    Err(format!(
        "Sidecar binary missing or too small ({}) and entry script missing ({})",
        packaged_exe.display(),
        entry_script.display()
    ))
}

/// UTF-8-veilige truncatie van één regel (bytes), met zichtbare suffix.
pub fn clamp_line_bytes(bytes: &[u8]) -> String {
    if bytes.len() <= MAX_SIDECAR_LINE_BYTES {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut end = MAX_SIDECAR_LINE_BYTES.saturating_sub(80);
    while end > 0 && std::str::from_utf8(&bytes[..end]).is_err() {
        end -= 1;
    }
    let prefix = String::from_utf8_lossy(&bytes[..end]);
    format!(
        "{} … [truncated, {} bytes total]",
        prefix,
        bytes.len()
    )
}

fn push_stderr_tail(tail: &Arc<Mutex<VecDeque<String>>>, line: String) {
    if let Ok(mut q) = tail.lock() {
        if q.len() >= STDERR_TAIL_MAX_LINES {
            q.pop_front();
        }
        q.push_back(line);
    }
}

fn stderr_tail_snapshot(tail: &Arc<Mutex<VecDeque<String>>>) -> Vec<String> {
    tail.lock()
        .map(|q| q.iter().cloned().collect())
        .unwrap_or_default()
}

/// Start het sidecar-proces (binary of python dev). Idempotent: als er al een child draait → Ok.
pub async fn spawn_sidecar<R: Runtime>(
    app: AppHandle<R>,
    spawn_lock: Arc<tokio::sync::Mutex<()>>,
    sidecar_child: Arc<Mutex<Option<CommandChild>>>,
    log_rotator: Arc<Mutex<LogRotator>>,
    protocol_version: Arc<AtomicU32>,
    hermes_version: Arc<Mutex<String>>,
    io: SidecarIoState,
) -> Result<(), String> {
    let _flight = spawn_lock.lock().await;

    {
        let guard = sidecar_child.lock().map_err(|e| e.to_string())?;
        if guard.is_some() {
            tracing::info!("spawn_sidecar: already running — skipping duplicate start");
            return Ok(());
        }
    }

    io.shutdown_requested
        .store(false, Ordering::SeqCst);

    let entry_script = get_sidecar_entry_path();
    let hermes_root = hermes_os_root();

    let sidecar_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries");
    let packaged_exe = sidecar_dir.join(packaged_sidecar_exe_name());

    let mode = resolve_sidecar_spawn(entry_script.as_path(), packaged_exe.as_path())?;

    let (mut rx, child) = match mode {
        SidecarSpawnMode::Packaged => {
            let mut cmd = app.shell().sidecar("hermes_agent").map_err(|e| e.to_string())?;
            cmd = cmd.current_dir(&hermes_root);
            cmd.spawn().map_err(|e| format!("Could not start sidecar binary: {}", e))?
        }
        SidecarSpawnMode::PythonDev => {
            let mut shell_cmd = app.shell().command("python");
            shell_cmd = shell_cmd.current_dir(&hermes_root);
            if cfg!(target_os = "windows") {
                shell_cmd = shell_cmd
                    .env("PYTHONUTF8", "1")
                    .env("PYTHONIOENCODING", "utf-8");
            }
            shell_cmd
                .args([
                    "-u",
                    entry_script.to_string_lossy().as_ref(),
                ])
                .spawn()
                .map_err(|e| format!("Could not start sidecar (python): {}", e))?
        }
    };

    *sidecar_child.lock().map_err(|e| e.to_string())? = Some(child);

    let app_out = app.clone();
    let rotator = log_rotator.clone();
    let pv = protocol_version.clone();
    let hv = hermes_version.clone();
    let child_slot = sidecar_child.clone();
    let stderr_tail = io.stderr_tail.clone();
    let shutdown_flag = io.shutdown_requested.clone();

    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                tauri_plugin_shell::process::CommandEvent::Stdout(line_bytes) => {
                    let text = clamp_line_bytes(&line_bytes);
                    if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&text) {
                        let method = msg
                            .get("method")
                            .and_then(|v| v.as_str())
                            .or_else(|| msg.get("type").and_then(|v| v.as_str()));

                        if method == Some("sidecar_hello") {
                            let data = msg.get("params").or_else(|| msg.get("data"));
                            if let Some(data) = data {
                                pv.store(
                                    data
                                        .get("protocol_version")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0) as u32,
                                    Ordering::SeqCst,
                                );
                                if let Some(ver) =
                                    data.get("hermes_version").and_then(|v| v.as_str())
                                {
                                    if let Ok(mut g) = hv.lock() {
                                        *g = ver.to_string();
                                    }
                                }
                                let _ = app_out.emit("sidecar-ready", &msg);
                            }
                        }
                        let _ = app_out.emit("agent-data", &msg);
                    } else {
                        let _ = app_out.emit("agent-raw", text);
                    }
                }
                tauri_plugin_shell::process::CommandEvent::Stderr(line_bytes) => {
                    let text = clamp_line_bytes(&line_bytes);
                    push_stderr_tail(&stderr_tail, text.clone());
                    if let Ok(mut rot) = rotator.lock() {
                        let _ = rot.write_line(&text);
                    }
                    let _ = app_out.emit("agent-error", text);
                }
                tauri_plugin_shell::process::CommandEvent::Terminated(status) => {
                    if let Ok(mut g) = child_slot.lock() {
                        *g = None;
                    }

                    let payload = serde_json::json!({
                        "code": status.code,
                        "signal": status.signal,
                    });
                    let _ = app_out.emit("agent-terminated", &payload);

                    let intentional = shutdown_flag.load(Ordering::SeqCst);
                    if !intentional {
                        let fatal = SidecarFatalPayload {
                            exit_code: status.code.map(|c| c.max(0) as u32),
                            signal: status.signal,
                            last_stderr_lines: stderr_tail_snapshot(&stderr_tail),
                            reason: "sidecar_exited_unexpectedly".into(),
                        };
                        let _ = app_out.emit("sidecar-fatal-error", &fatal);
                    }
                }
                _ => {}
            }
        }
    });

    Ok(())
}

/// Normale shutdown: markeer intent zodat Terminated geen fatal-event triggert.
pub fn request_sidecar_shutdown(io: &SidecarIoState) {
    io.shutdown_requested.store(true, Ordering::SeqCst);
}

fn take_sidecar_child(sidecar_child: &Arc<Mutex<Option<CommandChild>>>) -> Option<CommandChild> {
    sidecar_child
        .lock()
        .ok()
        .and_then(|mut g| g.take())
}

/// Graceful afsluiten: shutdown JSON naar stdin, daarna max [`GRACEFUL_SHUTDOWN_TIMEOUT`] wachten tot het
/// proces beeindigd is (child-slot `None`). Bij timeout: OS-level kill.
///
/// Opmerking: `tauri_plugin_shell::CommandChild` biedt geen publieke `close_stdin`; EOF komt alleen als de
/// shell-plugin dat later toevoegt. Tot die tijd is het control-commando het primaire stop-signaal.
pub async fn graceful_shutdown_sidecar(
    sidecar_child: Arc<Mutex<Option<CommandChild>>>,
    io: &SidecarIoState,
) {
    request_sidecar_shutdown(io);

    {
        let mut guard = match sidecar_child.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(child) = guard.as_mut() else {
            return;
        };
        if let Err(e) = child
            .write(SHUTDOWN_CONTROL_JSON)
            .and_then(|_| child.write(b"\n"))
        {
            tracing::warn!("Could not send graceful shutdown line to sidecar stdin: {}", e);
        }
    }

    let wait_slot_cleared = async {
        loop {
            let cleared = sidecar_child
                .lock()
                .ok()
                .map(|g| g.is_none())
                .unwrap_or(true);
            if cleared {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };

    match tokio::time::timeout(GRACEFUL_SHUTDOWN_TIMEOUT, wait_slot_cleared).await {
        Ok(_) => {
            tracing::info!("HermesOS sidecar stopped within graceful shutdown timeout");
        }
        Err(_) => {
            tracing::warn!("Force killed sidecar due to graceful shutdown timeout");
            if let Some(child) = take_sidecar_child(&sidecar_child) {
                let _ = child.kill();
            }
        }
    }
}

/// Child stoppen met korte timeout daarna nogmaals proberen (best-effort).
pub fn kill_sidecar_child(
    sidecar_child: &Arc<Mutex<Option<CommandChild>>>,
    io: &SidecarIoState,
) {
    request_sidecar_shutdown(io);
    if let Some(child) = take_sidecar_child(sidecar_child) {
        let _ = child.kill();
    }
}
