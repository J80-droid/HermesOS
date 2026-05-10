use regex::Regex;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, Runtime, State};
use tauri_plugin_shell::process::CommandChild;
use tauri_plugin_shell::ShellExt;
use tauri_plugin_updater::UpdaterExt;

pub mod log_rotator;
use log_rotator::LogRotator;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct AppState {
    sidecar_child: Arc<Mutex<Option<CommandChild>>>,
    log_rotator: Arc<Mutex<LogRotator>>,
    protocol_version: Arc<AtomicU32>,
    hermes_version: Arc<Mutex<String>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: String,
    pub source: String,
    pub model: Option<String>,
    pub title: Option<String>,
    pub started_at: f64,
    pub ended_at: Option<f64>,
    pub last_active: f64,
    pub is_active: bool,
    pub message_count: i32,
    pub tool_call_count: i32,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub estimated_cost_usd: Option<f64>,
    pub preview: Option<String>,
    pub parent_session_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaginatedSessions {
    pub sessions: Vec<SessionInfo>,
    pub total: usize,
    pub limit: usize,
    pub offset: usize,
}

// ---------------------------------------------------------------------------
// HERMES_HOME path resolution
// ---------------------------------------------------------------------------

pub fn resolve_hermes_home() -> PathBuf {
    if let Ok(home) = std::env::var("HERMES_HOME") {
        return PathBuf::from(home);
    }
    if let Some(user) = dirs::home_dir() {
        return user.join(".hermes");
    }
    PathBuf::from(".hermes")
}

/// Mirrors ``hermes_constants.get_default_hermes_root()`` — anchor for ``profiles/`` listing.
pub fn default_hermes_root() -> PathBuf {
    let native_home = dirs::home_dir()
        .map(|h| h.join(".hermes"))
        .unwrap_or_else(|| PathBuf::from(".hermes"));

    let env_home = std::env::var("HERMES_HOME").unwrap_or_default();
    let trimmed = env_home.trim();
    if trimmed.is_empty() {
        return native_home;
    }
    let env_path = PathBuf::from(trimmed);

    if env_path.starts_with(&native_home) {
        return native_home;
    }

    if env_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        == Some("profiles")
    {
        return env_path
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
            .unwrap_or(env_path);
    }

    env_path
}

pub fn hermes_config_path() -> PathBuf {
    resolve_hermes_home().join("config.yaml")
}

pub fn hermes_env_path() -> PathBuf {
    resolve_hermes_home().join(".env")
}

pub fn hermes_logs_dir() -> PathBuf {
    resolve_hermes_home().join("logs")
}

pub fn hermes_skills_dir() -> PathBuf {
    resolve_hermes_home().join("skills")
}

// ---------------------------------------------------------------------------
// Sidecar management
// ---------------------------------------------------------------------------

fn get_sidecar_entry_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("scripts")
        .join("sidecar_entry.py")
}

async fn internal_start_agent<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let entry_script = get_sidecar_entry_path();

    let sidecar_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries");
    let sidecar_exe = sidecar_dir.join("hermes_agent-x86_64-pc-windows-msvc.exe");
    let has_real_sidecar = sidecar_exe.exists()
        && sidecar_exe
            .metadata()
            .map(|m| m.len() > 10_000_000)
            .unwrap_or(false);

    let (mut rx, child) = if has_real_sidecar {
        tracing::info!("Starting compiled sidecar binary");
        app.shell()
            .sidecar("hermes_agent")
            .unwrap()
            .spawn()
            .map_err(|e| format!("Could not start sidecar binary: {}", e))?
    } else if entry_script.exists() {
        tracing::info!("Starting sidecar via python (dev mode)");
        let mut shell_cmd = app.shell().command("python");
        if cfg!(target_os = "windows") {
            shell_cmd = shell_cmd
                .env("PYTHONUTF8", "1")
                .env("PYTHONIOENCODING", "utf-8");
        }
        shell_cmd
            .arg(entry_script.to_string_lossy().to_string())
            .spawn()
            .map_err(|e| format!("Could not start sidecar (python): {}", e))?
    } else {
        return Err(format!(
            "Sidecar binary not found and entry script missing: {:?}",
            entry_script
        ));
    };

    *state.sidecar_child.lock().unwrap() = Some(child);

    let app_out = app.clone();
    let rotator = state.log_rotator.clone();
    let pv = state.protocol_version.clone();
    let hv = state.hermes_version.clone();
    let child_for_terminate = state.sidecar_child.clone();

    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                tauri_plugin_shell::process::CommandEvent::Stdout(line_bytes) => {
                    let text = String::from_utf8_lossy(&line_bytes);
                    if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&text) {
                        // Support both legacy "type" and JSON-RPC 2.0 "method"
                        let method = msg
                            .get("method")
                            .and_then(|v| v.as_str())
                            .or_else(|| msg.get("type").and_then(|v| v.as_str()));

                        if method == Some("sidecar_hello") {
                            // In JSON-RPC 2.0, hello params are in "params", in legacy they are in "data"
                            let data = msg.get("params").or_else(|| msg.get("data"));
                            if let Some(data) = data {
                                pv.store(
                                    data.get("protocol_version")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0) as u32,
                                    Ordering::SeqCst,
                                );
                                if let Some(ver) =
                                    data.get("hermes_version").and_then(|v| v.as_str())
                                {
                                    *hv.lock().unwrap() = ver.to_string();
                                }
                                let _ = app_out.emit("sidecar-ready", &msg);
                            }
                        }
                        let _ = app_out.emit("agent-data", &msg);
                    } else {
                        let _ = app_out.emit("agent-raw", text.to_string());
                    }
                }
                tauri_plugin_shell::process::CommandEvent::Stderr(line_bytes) => {
                    let text = String::from_utf8_lossy(&line_bytes);
                    if let Ok(mut rot) = rotator.lock() {
                        let _ = rot.write_line(&text);
                    }
                    let _ = app_out.emit("agent-error", text.to_string());
                }
                tauri_plugin_shell::process::CommandEvent::Terminated(status) => {
                    let _ = app_out.emit(
                        "agent-terminated",
                        serde_json::json!({
                            "code": status.code,
                            "signal": status.signal,
                        }),
                    );
                    *child_for_terminate.lock().unwrap() = None;
                }
                _ => {}
            }
        }
    });

    Ok(())
}

#[tauri::command]
async fn start_agent_sidecar<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    internal_start_agent(app, state).await
}

#[tauri::command]
async fn send_agent_query(
    query: String,
    id: Option<String>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut guard = state.sidecar_child.lock().unwrap();
    if let Some(ref mut child) = *guard {
        let req_id = id.unwrap_or_else(|| {
            format!(
                "q_{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis()
            )
        });

        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "chat",
            "params": { "query": query },
            "id": req_id
        });

        let msg = format!("{}\n", payload);
        child
            .write(msg.as_bytes())
            .map_err(|e| format!("Could not write to sidecar: {}", e))
            .map(|_| ())
    } else {
        Err("Sidecar agent is not started".to_string())
    }
}

async fn internal_kill_agent(state: State<'_, AppState>) -> Result<(), String> {
    let mut guard = state.sidecar_child.lock().unwrap();
    if let Some(child) = guard.take() {
        child
            .kill()
            .map_err(|e| format!("Could not stop sidecar agent: {}", e))
            .map(|_| ())
    } else {
        Ok(())
    }
}

#[tauri::command]
async fn kill_agent(state: State<'_, AppState>) -> Result<(), String> {
    internal_kill_agent(state).await
}

#[tauri::command]
async fn quit_app<R: Runtime>(app: AppHandle<R>, state: State<'_, AppState>) -> Result<(), String> {
    internal_kill_agent(state).await?;
    app.exit(0);
    Ok(())
}

#[tauri::command]
async fn restart_gateway<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    tracing::info!("Restarting agent (gateway mode)...");
    let _ = internal_kill_agent(state.clone()).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    internal_start_agent(app, state).await
}

/// Gebruikersvriendelijke fout voor de Tauri desktop-updater (niet `hermes update`).
fn desktop_updater_error_message(raw: &str) -> String {
    if raw.contains("Could not fetch a valid release JSON") {
        format!(
            "Desktop-update niet beschikbaar\n\n\
Deze knop zoekt een bestand \"latest.json\" op internet om deze desktop-app bij te werken. \
Op de standaard-URL staat dat vaak nog niet (bijv. 404), of er is geen verbinding.\n\n\
Dat is iets anders dan Hermes Agent bijwerken:\n\
• Agent (code + Python-omgeving): gebruik in een terminal: hermes update\n\
  Documentatie: https://hermes-agent.nousresearch.com/docs/getting-started/updating\n\n\
• Wél automatische desktop-app-updates: publiceer een geldige latest.json en pas \
plugins.updater.endpoints aan in HermesOS/src-tauri/tauri.conf.json \
(zie HermesOS/README.md → Updater).\n\n\
Technische melding: {}",
            raw
        )
    } else {
        format!(
            "Desktop-update mislukt.\n\n\
Probeer voor de Hermes-installatie zelf: hermes update (in een terminal).\n\n\
Technische melding: {}",
            raw
        )
    }
}

#[tauri::command]
async fn update_hermes<R: Runtime>(app: AppHandle<R>) -> Result<String, String> {
    let updater = app
        .updater()
        .map_err(|e| desktop_updater_error_message(&e.to_string()))?;
    match updater.check().await {
        Ok(Some(update)) => {
            let version = update.version.clone();
            // In a production app, we might want to return info to the UI first
            // for approval, but here we'll follow the "update now" intent.
            update
                .download_and_install(
                    |_chunk_length, _content_length| {
                        // Progress callback
                    },
                    || {
                        // Finished callback
                    },
                )
                .await
                .map_err(|e| desktop_updater_error_message(&format!("download/install: {}", e)))?;

            Ok(format!(
                "Desktop-update geïnstalleerd (v{}). Start de app opnieuw om te voltooien.",
                version
            ))
        }
        Ok(None) => Ok("Je gebruikt al de nieuwste versie van deze desktop-app.".to_string()),
        Err(e) => Err(desktop_updater_error_message(&e.to_string())),
    }
}

#[tauri::command]
fn get_diagnostics(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let home = resolve_hermes_home();
    let config_path = hermes_config_path();
    let db_path = home.join("state.db");

    let db_status = if db_path.exists() {
        match rusqlite::Connection::open(&db_path) {
            Ok(_) => "accessible".to_string(),
            Err(e) => format!("error: {}", e),
        }
    } else {
        "missing".to_string()
    };

    let sidecar_status = get_sidecar_status(state)?;

    Ok(serde_json::json!({
        "system": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "tauri_version": "2.0.0",
        },
        "hermes": {
            "home": home.to_string_lossy(),
            "config_exists": config_path.exists(),
            "db_status": db_status,
        },
        "sidecar": {
            "running": sidecar_status.running,
            "protocol_version": sidecar_status.protocol_version,
            "hermes_version": sidecar_status.hermes_version,
        },
    }))
}

/// Dashboard-achtige status voor HermesOS (geen echte gateway-task metingen).
#[tauri::command]
fn get_app_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let home = resolve_hermes_home();
    let home_s = home.to_string_lossy().to_string();
    let cfg_ver = load_config_yaml()
        .ok()
        .and_then(|c| {
            c.get("_config_version")
                .and_then(|v| v.as_u64())
                .or_else(|| {
                    c.get("_config_version")
                        .and_then(|v| v.as_i64())
                        .map(|n| n as u64)
                })
        })
        .unwrap_or(0);

    let active_sessions: i64 = {
        let db_path = home.join("state.db");
        if !db_path.exists() {
            0
        } else if let Ok(conn) = rusqlite::Connection::open(&db_path) {
            conn.query_row(
                "SELECT COUNT(*) FROM sessions WHERE ended_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0)
        } else {
            0
        }
    };

    let sidecar = get_sidecar_status(state)?;

    Ok(serde_json::json!({
        "active_sessions": active_sessions,
        "config_path": format!("{}/config.yaml", home_s.replace('\\', "/")),
        "config_version": cfg_ver,
        "env_path": format!("{}/.env", home_s.replace('\\', "/")),
        "gateway_exit_reason": serde_json::Value::Null,
        "gateway_health_url": serde_json::Value::Null,
        "gateway_pid": serde_json::Value::Null,
        "gateway_platforms": serde_json::json!({}),
        "gateway_running": sidecar.running,
        "gateway_state": serde_json::Value::Null,
        "gateway_updated_at": serde_json::Value::Null,
        "hermes_home": home_s.replace('\\', "/"),
        "latest_config_version": cfg_ver,
        "release_date": serde_json::Value::Null,
        "version": env!("CARGO_PKG_VERSION"),
        "sidecar_hermes_version": sidecar.hermes_version,
    }))
}

#[tauri::command]
fn get_action_status(name: String) -> Result<serde_json::Value, String> {
    Ok(serde_json::json!({
        "name": name,
        "status": "idle",
        "progress": 0,
        "running": false,
        "exit_code": 0,
        "lines": [],
        "pid": null,
    }))
}

// ---------------------------------------------------------------------------
// Config commands
// ---------------------------------------------------------------------------

/// Lees `config.yaml` als JSON **zonder** web-normalisatie (alleen voor merge bij save).
fn read_raw_config_json() -> Result<serde_json::Value, String> {
    let path = hermes_config_path();
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }
    let raw =
        std::fs::read_to_string(&path).map_err(|e| format!("Could not read config: {}", e))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(serde_json::json!({}));
    }
    let yaml: serde_yaml::Value =
        serde_yaml::from_str(trimmed).map_err(|e| format!("Invalid YAML in config.yaml: {}", e))?;
    serde_json::to_value(yaml).map_err(|e| e.to_string())
}

/// Alleen gehele JSON-getallen; anders 0 — zelfde idee als `isinstance(..., int)` in web_server.
fn json_integer_nonneg(v: &serde_json::Value) -> i64 {
    match v {
        serde_json::Value::Number(n) if n.is_i64() => n.as_i64().unwrap().max(0),
        serde_json::Value::Number(n) if n.is_u64() => {
            n.as_u64().unwrap().min(i64::MAX as u64) as i64
        }
        _ => 0,
    }
}

/// Zie `hermes_cli.web_server._normalize_config_for_web`.
fn normalize_config_for_web(mut config: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(ref mut obj) = config else {
        return config;
    };
    match obj.remove("model") {
        Some(serde_json::Value::Object(m)) => {
            let ctx_len = m
                .get("context_length")
                .map(json_integer_nonneg)
                .unwrap_or(0);
            let model_str = m
                .get("default")
                .or_else(|| m.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            obj.insert("model".into(), serde_json::Value::String(model_str));
            obj.insert("model_context_length".into(), serde_json::json!(ctx_len));
        }
        Some(other) => {
            obj.insert("model".into(), other);
            obj.insert("model_context_length".into(), serde_json::json!(0));
        }
        None => {
            obj.insert("model_context_length".into(), serde_json::json!(0));
        }
    }
    config
}

/// Zie `hermes_cli.web_server._denormalize_config_from_web`.
fn denormalize_config_from_web(mut config: serde_json::Value) -> Result<serde_json::Value, String> {
    let serde_json::Value::Object(ref mut obj) = config else {
        return Ok(config);
    };
    obj.remove("_model_meta");

    let ctx_override = obj
        .remove("model_context_length")
        .map(|v| json_integer_nonneg(&v))
        .unwrap_or(0);

    let model_val = obj.get("model").cloned();
    if let Some(serde_json::Value::String(s)) = model_val {
        if s.is_empty() {
            return Ok(config);
        }
        let disk = read_raw_config_json().unwrap_or_else(|_| serde_json::json!({}));
        match disk.get("model").cloned() {
            Some(serde_json::Value::Object(mut dm)) => {
                dm.insert("default".into(), serde_json::json!(s.clone()));
                if ctx_override > 0 {
                    dm.insert("context_length".into(), serde_json::json!(ctx_override));
                } else {
                    dm.remove("context_length");
                }
                obj.insert("model".into(), serde_json::Value::Object(dm));
            }
            _ => {
                if ctx_override > 0 {
                    obj.insert(
                        "model".into(),
                        serde_json::json!({
                            "default": s,
                            "context_length": ctx_override,
                        }),
                    );
                }
                // Anders: model blijft platte string in `obj`.
            }
        }
    }

    Ok(config)
}

fn strip_internal_config_keys(config: &mut serde_json::Map<String, serde_json::Value>) {
    config.retain(|k, _| !k.starts_with('_'));
}

#[tauri::command]
fn get_config() -> Result<serde_json::Value, String> {
    let mut val = read_raw_config_json()?;
    val = normalize_config_for_web(val);
    if let serde_json::Value::Object(ref mut m) = val {
        strip_internal_config_keys(m);
    }
    Ok(val)
}

#[tauri::command]
fn save_config(config: serde_json::Value) -> Result<(), String> {
    let denorm = denormalize_config_from_web(config)?;
    let yaml: serde_yaml::Value =
        serde_yaml::to_value(&denorm).map_err(|e| format!("Could not serialize config: {}", e))?;
    let path = hermes_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Could not create directory: {}", e))?;
    }
    let s = serde_yaml::to_string(&yaml).map_err(|e| format!("Could not emit YAML: {}", e))?;
    std::fs::write(&path, s).map_err(|e| format!("Could not write config: {}", e))
}

#[tauri::command]
fn get_config_raw() -> Result<String, String> {
    let path = hermes_config_path();
    if path.exists() {
        std::fs::read_to_string(&path).map_err(|e| format!("Could not read config: {}", e))
    } else {
        Ok(String::new())
    }
}

#[tauri::command]
fn save_config_raw(config_yaml: String) -> Result<(), String> {
    let trimmed = config_yaml.trim();
    if !trimmed.is_empty() {
        serde_yaml::from_str::<serde_yaml::Value>(trimmed)
            .map_err(|e| format!("Invalid YAML: {}", e))?;
    }
    let path = hermes_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Could not create directory: {}", e))?;
    }
    std::fs::write(&path, config_yaml).map_err(|e| format!("Could not write config: {}", e))
}

#[tauri::command]
fn get_hermes_home() -> Result<String, String> {
    Ok(resolve_hermes_home().to_string_lossy().to_string())
}

const CONFIG_METADATA_EMBED: &str = include_str!("../../src/generated/config-metadata.json");
const ENV_METADATA_EMBED: &str = include_str!("../../src/generated/env-metadata.json");

#[tauri::command]
fn get_defaults() -> Result<serde_json::Value, String> {
    let root: serde_json::Value =
        serde_json::from_str(CONFIG_METADATA_EMBED).map_err(|e| e.to_string())?;
    root.get("defaults")
        .cloned()
        .ok_or_else(|| "config-metadata.json ontbreekt 'defaults'".into())
}

#[tauri::command]
fn get_schema() -> Result<serde_json::Value, String> {
    let root: serde_json::Value =
        serde_json::from_str(CONFIG_METADATA_EMBED).map_err(|e| e.to_string())?;
    root.get("schema")
        .cloned()
        .ok_or_else(|| "config-metadata.json ontbreekt 'schema'".into())
}

// ---------------------------------------------------------------------------
// Sessions commands
// ---------------------------------------------------------------------------

/// Pariteit met ``SessionDB._sanitize_fts5_query`` (``hermes_state.py``).
fn sanitize_fts5_query(query: &str) -> String {
    let preserve_re = Regex::new(r#""[^"]*""#).expect("fts preserve quoted");
    let mut quoted_parts: Vec<String> = Vec::new();
    let sanitized: Cow<'_, str> = preserve_re.replace_all(query, |caps: &regex::Captures<'_>| {
        let full = caps.get(0).unwrap().as_str().to_string();
        let idx = quoted_parts.len();
        quoted_parts.push(full);
        Cow::Owned(format!("\x00Q{idx}\x00"))
    });

    let strip_re = Regex::new(r#"[+{}()\"^]"#).expect("fts strip");
    let mut sanitized = strip_re.replace_all(&sanitized, " ").into_owned();

    let star_collapse = Regex::new(r"\*+").expect("fts stars");
    sanitized = star_collapse.replace_all(&sanitized, "*").into_owned();
    let star_lead = Regex::new(r"(^|\s)\*").expect("fts star lead");
    sanitized = star_lead.replace_all(&sanitized, "$1").into_owned();

    let mut sanitized = sanitized.trim().to_string();
    let trim_bo_start = Regex::new(r"(?i)^(AND|OR|NOT)\b\s*").expect("fts bool start");
    sanitized = trim_bo_start.replace(&sanitized, "").into_owned();
    let trim_bo_end = Regex::new(r"(?i)\s+(AND|OR|NOT)\s*$").expect("fts bool end");
    sanitized = trim_bo_end.replace(sanitized.trim_end(), "").into_owned();

    let dotted = Regex::new(r#"\b(\w+(?:[._-]\w+)+)\b"#).expect("fts dotted");
    let mut sanitized = dotted
        .replace_all(&sanitized, |caps: &regex::Captures<'_>| {
            format!("\"{}\"", &caps[1])
        })
        .into_owned();

    for (i, quoted) in quoted_parts.iter().enumerate() {
        sanitized = sanitized.replace(&format!("\x00Q{i}\x00"), quoted);
    }

    sanitized.trim().to_string()
}

#[tauri::command]
async fn search_sessions(q: String) -> Result<serde_json::Value, String> {
    let needle = q.trim().to_string();
    if needle.is_empty() {
        return Ok(serde_json::json!({ "results": [] }));
    }

    let home = resolve_hermes_home();
    let db_path = home.join("state.db");

    if !db_path.exists() {
        return Ok(serde_json::json!({ "results": [] }));
    }

    let conn = rusqlite::Connection::open(&db_path)
        .map_err(|e| format!("Could not open session DB: {}", e))?;

    fn rows_from_stmt(
        stmt: &mut rusqlite::Statement<'_>,
        needle: &str,
    ) -> Result<Vec<serde_json::Value>, rusqlite::Error> {
        let search_iter = stmt.query_map([needle], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "title": row.get::<_, Option<String>>(1)?,
                "model": row.get::<_, Option<String>>(2)?,
                "started_at": row.get::<_, f64>(3)?,
            }))
        })?;
        let mut results = Vec::new();
        for result in search_iter {
            results.push(result?);
        }
        Ok(results)
    }

    let fts_sql = "SELECT DISTINCT s.id, s.title, s.model, s.started_at 
             FROM sessions s
             JOIN messages_fts f ON s.id = (SELECT session_id FROM messages WHERE id = f.rowid)
             WHERE messages_fts MATCH ?
             ORDER BY s.started_at DESC
             LIMIT 50";

    let fts_needle = sanitize_fts5_query(&needle);
    let results = if fts_needle.is_empty() {
        Vec::new()
    } else {
        match conn.prepare(fts_sql) {
            Ok(mut stmt) => rows_from_stmt(&mut stmt, &fts_needle).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    };

    let results = if !results.is_empty() {
        results
    } else {
        let like_sql = "SELECT DISTINCT s.id, s.title, s.model, s.started_at
             FROM sessions s
             INNER JOIN messages m ON m.session_id = s.id
             WHERE instr(COALESCE(m.content, ''), ?) > 0
             ORDER BY s.started_at DESC
             LIMIT 50";
        let mut stmt = conn.prepare(like_sql).map_err(|e| e.to_string())?;
        rows_from_stmt(&mut stmt, needle.as_str()).map_err(|e| e.to_string())?
    };

    Ok(serde_json::json!({ "results": results }))
}

// ---------------------------------------------------------------------------
// Analytics + model config (SQLite + YAML)
// ---------------------------------------------------------------------------

const AUX_TASK_SLOTS: &[&str] = &[
    "vision",
    "web_extract",
    "compression",
    "session_search",
    "skills_hub",
    "approval",
    "mcp",
    "title_generation",
    "curator",
];

fn analytics_cutoff_secs(days: u32) -> f64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    now - (days as f64) * 86400.0
}

fn empty_skills_json() -> serde_json::Value {
    serde_json::json!({
        "summary": {
            "total_skill_loads": 0,
            "total_skill_edits": 0,
            "total_skill_actions": 0,
            "distinct_skills_used": 0,
        },
        "top_skills": [],
    })
}

fn parse_usage_timestamp_iso(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp() as f64)
}

/// Hermes schrijft curator-/skill-statistieken naar ``skills/.usage.json``.
fn skills_analytics_from_usage_sidecar() -> serde_json::Value {
    let path = hermes_skills_dir().join(".usage.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return empty_skills_json();
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) else {
        return empty_skills_json();
    };
    let Some(obj) = val.as_object() else {
        return empty_skills_json();
    };

    let mut top_skills: Vec<serde_json::Value> = Vec::new();
    let mut total_loads: i64 = 0;
    let mut total_edits: i64 = 0;
    let mut distinct_used: i64 = 0;

    for (skill_name, rec) in obj {
        let Some(r) = rec.as_object() else {
            continue;
        };
        let use_count = r.get("use_count").and_then(|v| v.as_i64()).unwrap_or(0);
        let view_count = r.get("view_count").and_then(|v| v.as_i64()).unwrap_or(0);
        let patch_count = r.get("patch_count").and_then(|v| v.as_i64()).unwrap_or(0);
        total_loads += use_count;
        total_edits += patch_count;
        let total_count = view_count + use_count + patch_count;
        if total_count <= 0 {
            continue;
        }
        distinct_used += 1;
        let last_used_at = r
            .get("last_used_at")
            .and_then(|v| v.as_str())
            .and_then(parse_usage_timestamp_iso);
        top_skills.push(serde_json::json!({
            "skill": skill_name,
            "view_count": view_count,
            "manage_count": patch_count,
            "total_count": total_count,
            "last_used_at": last_used_at,
        }));
    }

    top_skills.sort_by(|a, b| {
        let ta = a.get("total_count").and_then(|v| v.as_i64()).unwrap_or(0);
        let tb = b.get("total_count").and_then(|v| v.as_i64()).unwrap_or(0);
        tb.cmp(&ta)
    });
    top_skills.truncate(50);

    let total_actions: i64 = top_skills
        .iter()
        .filter_map(|row| row.get("total_count").and_then(|v| v.as_i64()))
        .sum();

    serde_json::json!({
        "summary": {
            "total_skill_loads": total_loads,
            "total_skill_edits": total_edits,
            "total_skill_actions": total_actions,
            "distinct_skills_used": distinct_used,
        },
        "top_skills": top_skills,
    })
}

#[tauri::command]
fn get_analytics(days: Option<u32>) -> Result<serde_json::Value, String> {
    let days = days.unwrap_or(30).clamp(1, 366);
    let cutoff = analytics_cutoff_secs(days);
    let home = resolve_hermes_home();
    let db_path = home.join("state.db");
    if !db_path.exists() {
        return Ok(serde_json::json!({
            "daily": [],
            "by_model": [],
            "totals": {
                "total_input": 0,
                "total_output": 0,
                "total_cache_read": 0,
                "total_reasoning": 0,
                "total_estimated_cost": 0,
                "total_actual_cost": 0,
                "total_sessions": 0,
                "total_api_calls": 0,
            },
            "period_days": days,
            "skills": skills_analytics_from_usage_sidecar(),
        }));
    }

    let conn = rusqlite::Connection::open(&db_path).map_err(|e| e.to_string())?;

    let mut daily = Vec::new();
    let mut stmt = conn
        .prepare(
            "SELECT date(started_at, 'unixepoch') as day,
                    CAST(COALESCE(SUM(input_tokens), 0) AS REAL) as input_tokens,
                    CAST(COALESCE(SUM(output_tokens), 0) AS REAL) as output_tokens,
                    CAST(COALESCE(SUM(cache_read_tokens), 0) AS REAL) as cache_read_tokens,
                    CAST(COALESCE(SUM(reasoning_tokens), 0) AS REAL) as reasoning_tokens,
                    CAST(COALESCE(SUM(estimated_cost_usd), 0) AS REAL) as estimated_cost,
                    CAST(COALESCE(SUM(actual_cost_usd), 0) AS REAL) as actual_cost,
                    COUNT(*) as sessions,
                    CAST(COALESCE(SUM(api_call_count), 0) AS REAL) as api_calls
             FROM sessions WHERE started_at > ?
             GROUP BY day ORDER BY day",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([cutoff], |row| {
            Ok(serde_json::json!({
                "day": row.get::<_, String>(0)?,
                "input_tokens": row.get::<_, f64>(1)?,
                "output_tokens": row.get::<_, f64>(2)?,
                "cache_read_tokens": row.get::<_, f64>(3)?,
                "reasoning_tokens": row.get::<_, f64>(4)?,
                "estimated_cost": row.get::<_, f64>(5)?,
                "actual_cost": row.get::<_, f64>(6)?,
                "sessions": row.get::<_, i64>(7)?,
                "api_calls": row.get::<_, f64>(8)?,
            }))
        })
        .map_err(|e| e.to_string())?;
    for r in rows {
        daily.push(r.map_err(|e| e.to_string())?);
    }

    let mut by_model = Vec::new();
    let mut stmt2 = conn
        .prepare(
            "SELECT model,
                    CAST(COALESCE(SUM(input_tokens), 0) AS REAL) as input_tokens,
                    CAST(COALESCE(SUM(output_tokens), 0) AS REAL) as output_tokens,
                    CAST(COALESCE(SUM(estimated_cost_usd), 0) AS REAL) as estimated_cost,
                    COUNT(*) as sessions,
                    CAST(COALESCE(SUM(api_call_count), 0) AS REAL) as api_calls
             FROM sessions WHERE started_at > ? AND model IS NOT NULL AND trim(model) != ''
             GROUP BY model
             ORDER BY SUM(input_tokens) + SUM(output_tokens) DESC",
        )
        .map_err(|e| e.to_string())?;
    let rows2 = stmt2
        .query_map([cutoff], |row| {
            Ok(serde_json::json!({
                "model": row.get::<_, String>(0)?,
                "input_tokens": row.get::<_, f64>(1)?,
                "output_tokens": row.get::<_, f64>(2)?,
                "estimated_cost": row.get::<_, f64>(3)?,
                "sessions": row.get::<_, i64>(4)?,
                "api_calls": row.get::<_, f64>(5)?,
            }))
        })
        .map_err(|e| e.to_string())?;
    for r in rows2 {
        by_model.push(r.map_err(|e| e.to_string())?);
    }

    let totals = conn
        .query_row(
            "SELECT CAST(COALESCE(SUM(input_tokens), 0) AS REAL) as total_input,
                    CAST(COALESCE(SUM(output_tokens), 0) AS REAL) as total_output,
                    CAST(COALESCE(SUM(cache_read_tokens), 0) AS REAL) as total_cache_read,
                    CAST(COALESCE(SUM(reasoning_tokens), 0) AS REAL) as total_reasoning,
                    CAST(COALESCE(SUM(estimated_cost_usd), 0) AS REAL) as total_estimated_cost,
                    CAST(COALESCE(SUM(actual_cost_usd), 0) AS REAL) as total_actual_cost,
                    COUNT(*) as total_sessions,
                    CAST(COALESCE(SUM(api_call_count), 0) AS REAL) as total_api_calls
             FROM sessions WHERE started_at > ?",
            [cutoff],
            |row| {
                Ok(serde_json::json!({
                    "total_input": row.get::<_, f64>(0)?,
                    "total_output": row.get::<_, f64>(1)?,
                    "total_cache_read": row.get::<_, f64>(2)?,
                    "total_reasoning": row.get::<_, f64>(3)?,
                    "total_estimated_cost": row.get::<_, f64>(4)?,
                    "total_actual_cost": row.get::<_, f64>(5)?,
                    "total_sessions": row.get::<_, i64>(6)?,
                    "total_api_calls": row.get::<_, f64>(7)?,
                }))
            },
        )
        .map_err(|e| e.to_string())?;

    Ok(serde_json::json!({
        "daily": daily,
        "by_model": by_model,
        "totals": totals,
        "period_days": days,
        "skills": skills_analytics_from_usage_sidecar(),
    }))
}

#[tauri::command]
fn get_models_analytics(days: Option<u32>) -> Result<serde_json::Value, String> {
    let days = days.unwrap_or(30).clamp(1, 366);
    let cutoff = analytics_cutoff_secs(days);
    let home = resolve_hermes_home();
    let db_path = home.join("state.db");
    if !db_path.exists() {
        return Ok(serde_json::json!({
            "models": [],
            "totals": {
                "distinct_models": 0,
                "total_input": 0,
                "total_output": 0,
                "total_cache_read": 0,
                "total_reasoning": 0,
                "total_estimated_cost": 0,
                "total_actual_cost": 0,
                "total_sessions": 0,
                "total_api_calls": 0,
            },
            "period_days": days,
        }));
    }

    let conn = rusqlite::Connection::open(&db_path).map_err(|e| e.to_string())?;

    let mut models = Vec::new();
    let mut stmt = conn
        .prepare(
            "SELECT model,
                    billing_provider,
                    CAST(COALESCE(SUM(input_tokens), 0) AS REAL) as input_tokens,
                    CAST(COALESCE(SUM(output_tokens), 0) AS REAL) as output_tokens,
                    CAST(COALESCE(SUM(cache_read_tokens), 0) AS REAL) as cache_read_tokens,
                    CAST(COALESCE(SUM(reasoning_tokens), 0) AS REAL) as reasoning_tokens,
                    CAST(COALESCE(SUM(estimated_cost_usd), 0) AS REAL) as estimated_cost,
                    CAST(COALESCE(SUM(actual_cost_usd), 0) AS REAL) as actual_cost,
                    COUNT(*) as sessions,
                    CAST(COALESCE(SUM(api_call_count), 0) AS REAL) as api_calls,
                    CAST(COALESCE(SUM(tool_call_count), 0) AS REAL) as tool_calls,
                    MAX(started_at) as last_used_at,
                    CAST(COALESCE(AVG(CAST(input_tokens AS REAL) + CAST(output_tokens AS REAL)), 0) AS REAL) as avg_tokens_per_session
             FROM sessions WHERE started_at > ? AND model IS NOT NULL AND trim(model) != ''
             GROUP BY model, billing_provider
             ORDER BY SUM(input_tokens) + SUM(output_tokens) DESC",
        )
        .map_err(|e| e.to_string())?;

    let rows = stmt
        .query_map([cutoff], |row| {
            let provider: Option<String> = row.get(1)?;
            Ok(serde_json::json!({
                "model": row.get::<_, String>(0)?,
                "provider": provider.unwrap_or_default(),
                "input_tokens": row.get::<_, f64>(2)?,
                "output_tokens": row.get::<_, f64>(3)?,
                "cache_read_tokens": row.get::<_, f64>(4)?,
                "reasoning_tokens": row.get::<_, f64>(5)?,
                "estimated_cost": row.get::<_, f64>(6)?,
                "actual_cost": row.get::<_, f64>(7)?,
                "sessions": row.get::<_, i64>(8)?,
                "api_calls": row.get::<_, f64>(9)?,
                "tool_calls": row.get::<_, f64>(10)?,
                "last_used_at": row.get::<_, f64>(11)?,
                "avg_tokens_per_session": row.get::<_, f64>(12)?,
                "capabilities": serde_json::json!({}),
            }))
        })
        .map_err(|e| e.to_string())?;

    for r in rows {
        models.push(r.map_err(|e| e.to_string())?);
    }

    let totals = conn
        .query_row(
            "SELECT COUNT(DISTINCT model) as distinct_models,
                    CAST(COALESCE(SUM(input_tokens), 0) AS REAL) as total_input,
                    CAST(COALESCE(SUM(output_tokens), 0) AS REAL) as total_output,
                    CAST(COALESCE(SUM(cache_read_tokens), 0) AS REAL) as total_cache_read,
                    CAST(COALESCE(SUM(reasoning_tokens), 0) AS REAL) as total_reasoning,
                    CAST(COALESCE(SUM(estimated_cost_usd), 0) AS REAL) as total_estimated_cost,
                    CAST(COALESCE(SUM(actual_cost_usd), 0) AS REAL) as total_actual_cost,
                    COUNT(*) as total_sessions,
                    CAST(COALESCE(SUM(api_call_count), 0) AS REAL) as total_api_calls
             FROM sessions WHERE started_at > ? AND model IS NOT NULL AND trim(model) != ''",
            [cutoff],
            |row| {
                Ok(serde_json::json!({
                    "distinct_models": row.get::<_, i64>(0)?,
                    "total_input": row.get::<_, f64>(1)?,
                    "total_output": row.get::<_, f64>(2)?,
                    "total_cache_read": row.get::<_, f64>(3)?,
                    "total_reasoning": row.get::<_, f64>(4)?,
                    "total_estimated_cost": row.get::<_, f64>(5)?,
                    "total_actual_cost": row.get::<_, f64>(6)?,
                    "total_sessions": row.get::<_, i64>(7)?,
                    "total_api_calls": row.get::<_, f64>(8)?,
                }))
            },
        )
        .map_err(|e| e.to_string())?;

    Ok(serde_json::json!({
        "models": models,
        "totals": totals,
        "period_days": days,
    }))
}

fn load_config_yaml() -> Result<serde_yaml::Value, String> {
    let path = hermes_config_path();
    if !path.exists() {
        return Ok(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    }
    let s = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    serde_yaml::from_str(&s).map_err(|e| e.to_string())
}

fn save_config_yaml(cfg: &serde_yaml::Value) -> Result<(), String> {
    let path = hermes_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let s = serde_yaml::to_string(cfg).map_err(|e| e.to_string())?;
    std::fs::write(&path, s).map_err(|e| e.to_string())
}

/// Schrijft `memory.provider` en/of `context.engine` naar `config.yaml`,
/// analog aan `hermes_cli.plugins_cmd._save_memory_provider` /
/// `_save_context_engine` en `PUT /api/dashboard/plugin-providers` in `web_server.py`.
///
/// Lege `memory_provider`-string = ingebouwde memory (zelfde als dashboard).
pub fn save_plugin_providers_yaml(
    memory_provider: Option<String>,
    context_engine: Option<String>,
) -> Result<(), String> {
    if memory_provider.is_none() && context_engine.is_none() {
        return Ok(());
    }

    let mut cfg = load_config_yaml()?;
    if !matches!(cfg, serde_yaml::Value::Mapping(_)) {
        cfg = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }

    let root = cfg
        .as_mapping_mut()
        .ok_or_else(|| "config.yaml root moet een mapping zijn.".to_string())?;

    if let Some(mp) = memory_provider {
        let mem_key = serde_yaml::Value::String("memory".into());
        let slot = root
            .entry(mem_key)
            .or_insert(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
        let mem_map = match slot {
            serde_yaml::Value::Mapping(m) => m,
            _ => {
                return Err(
                    "config.yaml: sleutel 'memory' moet een mapping zijn (bv. memory: {{ provider: ... }})."
                        .into(),
                );
            }
        };
        mem_map.insert(
            serde_yaml::Value::String("provider".into()),
            serde_yaml::Value::String(mp),
        );
    }

    if let Some(ce) = context_engine {
        let ctx_key = serde_yaml::Value::String("context".into());
        let slot = root
            .entry(ctx_key)
            .or_insert(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
        let ctx_map = match slot {
            serde_yaml::Value::Mapping(m) => m,
            _ => {
                return Err("config.yaml: sleutel 'context' moet een mapping zijn.".into());
            }
        };
        ctx_map.insert(
            serde_yaml::Value::String("engine".into()),
            serde_yaml::Value::String(ce),
        );
    }

    save_config_yaml(&cfg)
}

#[tauri::command]
fn save_plugin_providers(
    memory_provider: Option<String>,
    context_engine: Option<String>,
) -> Result<(), String> {
    save_plugin_providers_yaml(memory_provider, context_engine)
}

fn yaml_as_mapping(v: serde_yaml::Value) -> serde_yaml::Mapping {
    match v {
        serde_yaml::Value::Mapping(m) => m,
        _ => serde_yaml::Mapping::new(),
    }
}

#[tauri::command]
fn get_auxiliary_models() -> Result<serde_json::Value, String> {
    let cfg = load_config_yaml()?;
    let aux_root = cfg.get("auxiliary").and_then(|v| v.as_mapping());
    let mut tasks = Vec::new();
    for slot in AUX_TASK_SLOTS {
        let slot_cfg = aux_root
            .and_then(|m| m.get(serde_yaml::Value::String((*slot).to_string())))
            .and_then(|v| v.as_mapping());
        let (provider, model, base_url) = match slot_cfg {
            Some(m) => {
                let p = m
                    .get(serde_yaml::Value::String("provider".into()))
                    .and_then(|v| v.as_str())
                    .unwrap_or("auto");
                let mo = m
                    .get(serde_yaml::Value::String("model".into()))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let bu = m
                    .get(serde_yaml::Value::String("base_url".into()))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                (
                    if p.is_empty() {
                        "auto".to_string()
                    } else {
                        p.to_string()
                    },
                    mo.to_string(),
                    bu.to_string(),
                )
            }
            _ => ("auto".to_string(), String::new(), String::new()),
        };
        tasks.push(serde_json::json!({
            "task": slot,
            "provider": provider,
            "model": model,
            "base_url": base_url,
        }));
    }

    let main = match cfg.get("model") {
        Some(serde_yaml::Value::String(s)) => serde_json::json!({
            "provider": "",
            "model": s,
        }),
        Some(serde_yaml::Value::Mapping(m)) => {
            let provider = m
                .get(serde_yaml::Value::String("provider".into()))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let model = m
                .get(serde_yaml::Value::String("default".into()))
                .or_else(|| m.get(serde_yaml::Value::String("name".into())))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            serde_json::json!({ "provider": provider, "model": model })
        }
        _ => serde_json::json!({ "provider": "", "model": "" }),
    };

    Ok(serde_json::json!({ "tasks": tasks, "main": main }))
}

#[tauri::command]
fn set_model_assignment(payload: serde_json::Value) -> Result<(), String> {
    let body = payload.get("body").cloned().unwrap_or(payload);
    let scope = body
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let provider = body
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let task = body
        .get("task")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_lowercase();

    if scope != "main" && scope != "auxiliary" {
        return Err("scope must be 'main' or 'auxiliary'".to_string());
    }

    let mut cfg = load_config_yaml()?;
    let root = cfg
        .as_mapping_mut()
        .ok_or_else(|| "config root must be a mapping".to_string())?;

    if scope == "main" {
        if provider.is_empty() || model.is_empty() {
            return Err("provider and model required for main".to_string());
        }
        let mut model_map = yaml_as_mapping(
            root.get(serde_yaml::Value::String("model".into()))
                .cloned()
                .unwrap_or(serde_yaml::Value::Mapping(serde_yaml::Mapping::new())),
        );
        model_map.insert(
            serde_yaml::Value::String("provider".into()),
            serde_yaml::Value::String(provider),
        );
        model_map.insert(
            serde_yaml::Value::String("default".into()),
            serde_yaml::Value::String(model),
        );
        model_map.remove(serde_yaml::Value::String("base_url".into()));
        model_map.remove(serde_yaml::Value::String("context_length".into()));
        root.insert(
            serde_yaml::Value::String("model".into()),
            serde_yaml::Value::Mapping(model_map),
        );
        save_config_yaml(&cfg)?;
        return Ok(());
    }

    let mut aux = yaml_as_mapping(
        root.get(serde_yaml::Value::String("auxiliary".into()))
            .cloned()
            .unwrap_or(serde_yaml::Value::Mapping(serde_yaml::Mapping::new())),
    );

    if task == "__reset__" {
        for slot in AUX_TASK_SLOTS {
            let mut slot_cfg = yaml_as_mapping(
                aux.get(serde_yaml::Value::String((*slot).to_string()))
                    .cloned()
                    .unwrap_or(serde_yaml::Value::Mapping(serde_yaml::Mapping::new())),
            );
            slot_cfg.insert(
                serde_yaml::Value::String("provider".into()),
                serde_yaml::Value::String("auto".into()),
            );
            slot_cfg.insert(
                serde_yaml::Value::String("model".into()),
                serde_yaml::Value::String(String::new()),
            );
            aux.insert(
                serde_yaml::Value::String((*slot).to_string()),
                serde_yaml::Value::Mapping(slot_cfg),
            );
        }
        root.insert(
            serde_yaml::Value::String("auxiliary".into()),
            serde_yaml::Value::Mapping(aux),
        );
        save_config_yaml(&cfg)?;
        return Ok(());
    }

    if provider.is_empty() {
        return Err("provider required for auxiliary".to_string());
    }

    let targets: Vec<&str> = if task.is_empty() {
        AUX_TASK_SLOTS.to_vec()
    } else {
        vec![task.as_str()]
    };

    for slot in targets {
        if !AUX_TASK_SLOTS.contains(&slot) {
            return Err(format!("unknown auxiliary task: {}", slot));
        }
        let mut slot_cfg = yaml_as_mapping(
            aux.get(serde_yaml::Value::String(slot.to_string()))
                .cloned()
                .unwrap_or(serde_yaml::Value::Mapping(serde_yaml::Mapping::new())),
        );
        slot_cfg.insert(
            serde_yaml::Value::String("provider".into()),
            serde_yaml::Value::String(provider.clone()),
        );
        slot_cfg.insert(
            serde_yaml::Value::String("model".into()),
            serde_yaml::Value::String(model.clone()),
        );
        aux.insert(
            serde_yaml::Value::String(slot.to_string()),
            serde_yaml::Value::Mapping(slot_cfg),
        );
    }

    root.insert(
        serde_yaml::Value::String("auxiliary".into()),
        serde_yaml::Value::Mapping(aux),
    );
    save_config_yaml(&cfg)
}

/// Als Python IPC faalt: minimale info uit YAML (geen catalogus-resolutie).
fn model_info_from_yaml_fallback(cfg: &serde_yaml::Value) -> serde_json::Value {
    let caps_empty = serde_json::json!({});
    match cfg.get("model") {
        Some(serde_yaml::Value::Mapping(m)) => {
            let provider = m
                .get(serde_yaml::Value::String("provider".into()))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let model = m
                .get(serde_yaml::Value::String("default".into()))
                .or_else(|| m.get(serde_yaml::Value::String("name".into())))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let config_ctx_int = m
                .get(serde_yaml::Value::String("context_length".into()))
                .and_then(|v| v.as_i64())
                .filter(|&n| n > 0)
                .unwrap_or(0) as i32;
            let auto_ctx = 0;
            let effective = if config_ctx_int > 0 {
                config_ctx_int
            } else {
                auto_ctx
            };
            serde_json::json!({
                "model": model,
                "provider": provider,
                "auto_context_length": auto_ctx,
                "config_context_length": config_ctx_int,
                "effective_context_length": effective,
                "capabilities": caps_empty,
                "resolution_source": "yaml_fallback",
            })
        }
        Some(serde_yaml::Value::String(s)) => serde_json::json!({
            "model": s,
            "provider": "",
            "auto_context_length": 0,
            "config_context_length": 0,
            "effective_context_length": 0,
            "capabilities": caps_empty,
            "resolution_source": "yaml_fallback",
        }),
        _ => serde_json::json!({
            "model": "",
            "provider": "",
            "auto_context_length": 0,
            "config_context_length": 0,
            "effective_context_length": 0,
            "capabilities": caps_empty,
            "resolution_source": "yaml_fallback",
        }),
    }
}

#[tauri::command]
fn get_model_info() -> Result<serde_json::Value, String> {
    let cfg = load_config_yaml()?;
    match run_python_ipc_script("model_info_ipc_helper.py", serde_json::json!({}), true) {
        Ok(v) => Ok(v),
        Err(_) => Ok(model_info_from_yaml_fallback(&cfg)),
    }
}

#[tauri::command]
fn get_model_options() -> Result<serde_json::Value, String> {
    let cfg = load_config_yaml()?;
    let (provider, model) = match cfg.get("model") {
        Some(serde_yaml::Value::Mapping(m)) => (
            m.get(serde_yaml::Value::String("provider".into()))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            m.get(serde_yaml::Value::String("default".into()))
                .or_else(|| m.get(serde_yaml::Value::String("name".into())))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ),
        Some(serde_yaml::Value::String(s)) => ("".to_string(), s.clone()),
        _ => ("".to_string(), "".to_string()),
    };
    Ok(serde_json::json!({
        "providers": [],
        "provider": provider,
        "model": model,
        "desktop_catalog_hint": "Volledige modelcatalogus is in HermesOS desktop niet ingebouwd. Stel provider/modellen in via config.yaml of gebruik het web-dashboard.",
    }))
}

// ---------------------------------------------------------------------------
// OAuth — not implemented on HermesOS desktop (explicit errors)
// ---------------------------------------------------------------------------

#[tauri::command]
fn disconnect_oauth(_id: String) -> Result<(), String> {
    Err("OAuth afmelden is nog niet beschikbaar in HermesOS desktop.".into())
}

#[tauri::command]
fn start_oauth(_id: String) -> Result<serde_json::Value, String> {
    Err(format!(
        "OAuth-login ({}) is nog niet beschikbaar in HermesOS desktop.",
        _id
    ))
}

#[tauri::command]
fn submit_oauth(_id: String, _code: String) -> Result<serde_json::Value, String> {
    Err("OAuth is nog niet beschikbaar in HermesOS desktop.".into())
}

#[tauri::command]
fn poll_oauth(_id: String, _session_id: String) -> Result<serde_json::Value, String> {
    Err("OAuth is nog niet beschikbaar in HermesOS desktop.".into())
}

#[tauri::command]
fn cancel_oauth(_id: String, _session_id: String) -> Result<(), String> {
    Err("OAuth is nog niet beschikbaar in HermesOS desktop.".into())
}

// ---------------------------------------------------------------------------
// Profiles — list from disk (default_hermes_root); mutations via Python IPC
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ProfileListEntry {
    name: String,
    path: String,
    is_default: bool,
    model: Option<String>,
    provider: Option<String>,
    has_env: bool,
    skill_count: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ProfilesListResponse {
    pub profiles: Vec<ProfileListEntry>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ProfileSoulResponse {
    pub content: String,
    pub exists: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ProfileSetupCommandResponse {
    pub command: String,
}

const RESERVED_PROFILE_NAMES: &[&str] = &["hermes", "test", "tmp", "root", "sudo"];

fn normalize_profile_name(name: &str) -> Result<String, String> {
    let stripped = name.trim();
    if stripped.is_empty() {
        return Err("profielnaam mag niet leeg zijn".into());
    }
    if stripped.eq_ignore_ascii_case("default") {
        return Ok("default".into());
    }
    Ok(stripped.to_lowercase())
}

fn is_valid_profile_id(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if name.len() > 64 {
        return false;
    }
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn validate_profile_name_normalized(name: &str) -> Result<(), String> {
    if name == "default" {
        return Ok(());
    }
    if !is_valid_profile_id(name) {
        return Err(format!(
            "Ongeldige profielnaam {name:?}. Moet voldoen aan [a-z0-9][a-z0-9_-]{{0,63}}"
        ));
    }
    if RESERVED_PROFILE_NAMES.contains(&name) {
        return Err(format!("Profielnaam {name:?} is gereserveerd."));
    }
    Ok(())
}

fn profile_home_dir(normalized: &str) -> PathBuf {
    let root = default_hermes_root();
    if normalized == "default" {
        root
    } else {
        root.join("profiles").join(normalized)
    }
}

fn read_profile_model_provider(profile_dir: &std::path::Path) -> (Option<String>, Option<String>) {
    let config_path = profile_dir.join("config.yaml");
    let Ok(content) = std::fs::read_to_string(&config_path) else {
        return (None, None);
    };
    let Ok(cfg) = serde_yaml::from_str::<serde_yaml::Value>(&content) else {
        return (None, None);
    };
    match cfg.get("model") {
        Some(serde_yaml::Value::String(s)) => (Some(s.clone()), None),
        Some(serde_yaml::Value::Mapping(m)) => {
            let model = m
                .get(serde_yaml::Value::String("default".into()))
                .or_else(|| m.get(serde_yaml::Value::String("model".into())))
                .or_else(|| m.get(serde_yaml::Value::String("name".into())))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let provider = m
                .get(serde_yaml::Value::String("provider".into()))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            (model, provider)
        }
        _ => (None, None),
    }
}

fn count_profile_skills(skills_dir: &std::path::Path) -> i64 {
    if !skills_dir.is_dir() {
        return 0;
    }
    let mut count: i64 = 0;
    fn walk(dir: &std::path::Path, count: &mut i64) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                let seg = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if seg == ".hub" || seg == ".git" {
                    continue;
                }
                walk(&p, count);
            } else if p.file_name().and_then(|n| n.to_str()) == Some("SKILL.md") {
                *count += 1;
            }
        }
    }
    walk(skills_dir, &mut count);
    count
}

#[tauri::command]
fn get_profiles() -> Result<ProfilesListResponse, String> {
    let mut profiles = Vec::new();
    let root = default_hermes_root();
    if root.is_dir() {
        let (model, provider) = read_profile_model_provider(&root);
        profiles.push(ProfileListEntry {
            name: "default".into(),
            path: root.to_string_lossy().to_string(),
            is_default: true,
            model,
            provider,
            has_env: root.join(".env").exists(),
            skill_count: count_profile_skills(&root.join("skills")),
        });
    }

    let profiles_root = root.join("profiles");
    if profiles_root.is_dir() {
        let mut dirs: Vec<_> = std::fs::read_dir(&profiles_root)
            .map_err(|e| e.to_string())?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect();
        dirs.sort_by_key(|e| e.file_name());

        for entry in dirs {
            let name = entry.file_name().to_string_lossy().to_string();
            if !is_valid_profile_id(&name) {
                continue;
            }
            let path = entry.path();
            let (model, provider) = read_profile_model_provider(&path);
            profiles.push(ProfileListEntry {
                name,
                path: path.to_string_lossy().to_string(),
                is_default: false,
                model,
                provider,
                has_env: path.join(".env").exists(),
                skill_count: count_profile_skills(&path.join("skills")),
            });
        }
    }

    Ok(ProfilesListResponse { profiles })
}

#[tauri::command]
fn create_profile(payload: serde_json::Value) -> Result<(), String> {
    let body = payload.get("body").cloned().unwrap_or(payload);
    let raw_name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let name = normalize_profile_name(&raw_name)?;
    validate_profile_name_normalized(&name)?;
    if name == "default" {
        return Err("Kan geen profiel 'default' aanmaken.".into());
    }

    let clone_from_default = body
        .get("clone_from_default")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let no_skills = body
        .get("no_skills")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    run_profiles_ipc(serde_json::json!({
        "op": "create",
        "name": name,
        "clone_from_default": clone_from_default,
        "no_skills": no_skills,
    }))?;
    Ok(())
}

#[tauri::command]
fn rename_profile(name: String, new_name: String) -> Result<(), String> {
    let old_n = normalize_profile_name(&name)?;
    let new_n = normalize_profile_name(&new_name)?;
    validate_profile_name_normalized(&old_n)?;
    validate_profile_name_normalized(&new_n)?;
    if old_n == "default" || new_n == "default" {
        return Err("Het standaardprofiel kan niet hernoemd worden.".into());
    }
    run_profiles_ipc(serde_json::json!({
        "op": "rename",
        "old_name": old_n,
        "new_name": new_n,
    }))?;
    Ok(())
}

#[tauri::command]
fn delete_profile(name: String) -> Result<(), String> {
    let n = normalize_profile_name(&name)?;
    validate_profile_name_normalized(&n)?;
    if n == "default" {
        return Err(
            "Het standaardprofiel (~/.hermes) kan niet worden verwijderd. Gebruik `hermes uninstall`."
                .into(),
        );
    }
    run_profiles_ipc(serde_json::json!({
        "op": "delete",
        "name": n,
    }))?;
    Ok(())
}

#[tauri::command]
fn get_profile_setup_command(name: String) -> Result<ProfileSetupCommandResponse, String> {
    let n = normalize_profile_name(&name)?;
    validate_profile_name_normalized(&n)?;
    let dir = profile_home_dir(&n);
    if n != "default" && !dir.is_dir() {
        return Err(format!("Profiel '{n}' bestaat niet."));
    }
    let cmd = if n == "default" {
        "hermes setup".into()
    } else {
        format!("{n} setup")
    };
    Ok(ProfileSetupCommandResponse { command: cmd })
}

#[tauri::command]
fn get_profile_soul(name: String) -> Result<ProfileSoulResponse, String> {
    let n = normalize_profile_name(&name)?;
    validate_profile_name_normalized(&n)?;
    let dir = profile_home_dir(&n);
    if n != "default" && !dir.is_dir() {
        return Err(format!("Profiel '{n}' bestaat niet."));
    }
    let soul_path = dir.join("SOUL.md");
    if soul_path.exists() {
        let content = std::fs::read_to_string(&soul_path).map_err(|e| e.to_string())?;
        Ok(ProfileSoulResponse {
            content,
            exists: true,
        })
    } else {
        Ok(ProfileSoulResponse {
            content: String::new(),
            exists: false,
        })
    }
}

#[tauri::command]
fn update_profile_soul(name: String, content: String) -> Result<(), String> {
    let n = normalize_profile_name(&name)?;
    validate_profile_name_normalized(&n)?;
    let dir = profile_home_dir(&n);
    if n != "default" && !dir.is_dir() {
        return Err(format!("Profiel '{n}' bestaat niet."));
    }
    let soul_path = dir.join("SOUL.md");
    std::fs::write(&soul_path, content).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Sessions commands
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
pub struct SessionMessage {
    pub role: String,
    pub content: Option<String>,
    pub tool_calls: Option<serde_json::Value>,
    pub tool_name: Option<String>,
    pub tool_call_id: Option<String>,
    pub timestamp: Option<f64>,
    pub reasoning: Option<String>,
}

#[derive(Serialize)]
pub struct SessionMessagesResponse {
    pub session_id: String,
    pub messages: Vec<SessionMessage>,
}

#[tauri::command]
async fn get_sessions(
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<PaginatedSessions, String> {
    let home = resolve_hermes_home();
    let db_path = home.join("state.db");
    let limit = limit.unwrap_or(20);
    let offset = offset.unwrap_or(0);

    if !db_path.exists() {
        return Ok(PaginatedSessions {
            sessions: Vec::new(),
            total: 0,
            limit,
            offset,
        });
    }

    let conn = rusqlite::Connection::open(&db_path)
        .map_err(|e| format!("Could not open session DB: {}", e))?;

    let total: usize = conn
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;

    let mut stmt = conn
        .prepare(
            "SELECT s.id, s.source, s.model, s.title, s.started_at, s.ended_at,
                    s.message_count, s.tool_call_count, s.input_tokens, s.output_tokens,
                    s.estimated_cost_usd, s.parent_session_id,
                    COALESCE(
                      (SELECT MAX(m.timestamp) FROM messages m WHERE m.session_id = s.id),
                      s.started_at
                    ) AS last_active,
                    (SELECT m.content FROM messages m WHERE m.session_id = s.id
                     ORDER BY m.timestamp DESC LIMIT 1) AS preview
             FROM sessions s
             ORDER BY s.started_at DESC
             LIMIT ? OFFSET ?",
        )
        .map_err(|e| e.to_string())?;

    let session_iter = stmt
        .query_map([limit, offset], |row| {
            let ended_at: Option<f64> = row.get(5)?;
            Ok(SessionInfo {
                id: row.get(0)?,
                source: row.get(1)?,
                model: row.get(2)?,
                title: row.get(3)?,
                started_at: row.get(4)?,
                ended_at,
                message_count: row.get(6)?,
                tool_call_count: row.get(7)?,
                input_tokens: row.get(8)?,
                output_tokens: row.get(9)?,
                estimated_cost_usd: row.get(10)?,
                parent_session_id: row.get(11)?,
                last_active: row.get(12)?,
                preview: row.get(13)?,
                is_active: ended_at.is_none(),
            })
        })
        .map_err(|e| e.to_string())?;

    let mut sessions = Vec::new();
    for session in session_iter {
        sessions.push(session.map_err(|e| e.to_string())?);
    }

    Ok(PaginatedSessions {
        sessions,
        total,
        limit,
        offset,
    })
}

#[tauri::command]
async fn get_session_messages(session_id: String) -> Result<SessionMessagesResponse, String> {
    let home = resolve_hermes_home();
    let db_path = home.join("state.db");

    if !db_path.exists() {
        return Ok(SessionMessagesResponse {
            session_id,
            messages: Vec::new(),
        });
    }

    let conn = rusqlite::Connection::open(&db_path)
        .map_err(|e| format!("Could not open session DB: {}", e))?;

    let mut stmt = conn
        .prepare(
            "SELECT role, content, tool_calls, tool_name, tool_call_id, timestamp, reasoning 
             FROM messages 
             WHERE session_id = ? 
             ORDER BY timestamp ASC",
        )
        .map_err(|e| e.to_string())?;

    let msg_iter = stmt
        .query_map([session_id.clone()], |row| {
            let tool_calls_json: Option<String> = row.get(2)?;
            let tool_calls = tool_calls_json.and_then(|s| serde_json::from_str(&s).ok());

            Ok(SessionMessage {
                role: row.get(0)?,
                content: row.get(1)?,
                tool_calls,
                tool_name: row.get(3)?,
                tool_call_id: row.get(4)?,
                timestamp: row.get(5)?,
                reasoning: row.get(6)?,
            })
        })
        .map_err(|e| e.to_string())?;

    let mut messages = Vec::new();
    for msg in msg_iter {
        messages.push(msg.map_err(|e| e.to_string())?);
    }

    Ok(SessionMessagesResponse {
        session_id,
        messages,
    })
}

#[tauri::command]
fn delete_session(session_id: String) -> Result<(), String> {
    let sid = session_id.trim();
    if sid.is_empty() {
        return Err("session id required".to_string());
    }
    let home = resolve_hermes_home();
    let db_path = home.join("state.db");
    if !db_path.exists() {
        return Ok(());
    }

    let mut conn = rusqlite::Connection::open(&db_path).map_err(|e| e.to_string())?;
    let tx = conn.transaction().map_err(|e| e.to_string())?;

    // Collect this session and all descendants (parent_session_id FK); delete deepest first.
    let mut stmt = tx
        .prepare(
            "WITH RECURSIVE subtree(id, depth) AS (
                SELECT id, 0 FROM sessions WHERE id = ?1
                UNION ALL
                SELECT s.id, subtree.depth + 1 FROM sessions s
                INNER JOIN subtree ON s.parent_session_id = subtree.id
            )
            SELECT id FROM subtree ORDER BY depth DESC",
        )
        .map_err(|e| e.to_string())?;

    let ids: Vec<String> = stmt
        .query_map([sid], |row| row.get(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    drop(stmt);

    if ids.is_empty() {
        tx.commit().map_err(|e| e.to_string())?;
        return Ok(());
    }

    for id in &ids {
        tx.execute("DELETE FROM messages WHERE session_id = ?", [id])
            .map_err(|e| e.to_string())?;
    }
    for id in &ids {
        tx.execute("DELETE FROM sessions WHERE id = ?", [id])
            .map_err(|e| e.to_string())?;
    }

    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Cron / profiles — Python helpers in HermesOS/scripts/ (repo checkout)
// ---------------------------------------------------------------------------

fn run_python_ipc_script(
    script_name: &str,
    mut payload: serde_json::Value,
    inject_hermes_home: bool,
) -> Result<serde_json::Value, String> {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../scripts/")
        .join(script_name);
    if !script.exists() {
        return Err(format!(
            "Python-helper ontbreekt ({}). Vereist hermes-agent checkout met HermesOS/scripts.",
            script.display()
        ));
    }
    let script_str = script.to_str().ok_or("script pad utf-8")?;
    if inject_hermes_home {
        let map = payload
            .as_object_mut()
            .ok_or("ipc payload moet een JSON-object zijn")?;
        map.insert(
            "hermes_home".to_string(),
            serde_json::Value::String(resolve_hermes_home().to_string_lossy().to_string()),
        );
    }

    let mut attempts: Vec<(String, Vec<String>)> = Vec::new();
    if let Ok(p) = std::env::var("HERMES_PYTHON") {
        let p = p.trim().to_string();
        if !p.is_empty() {
            attempts.push((p, vec!["-u".into(), script_str.into()]));
        }
    }
    attempts.push(("python".into(), vec!["-u".into(), script_str.into()]));
    if cfg!(target_os = "windows") {
        attempts.push((
            "py".into(),
            vec!["-3".into(), "-u".into(), script_str.into()],
        ));
    }
    attempts.push(("python3".into(), vec!["-u".into(), script_str.into()]));

    let mut last_err = "kon Python niet starten".to_string();
    for (prog, args) in attempts {
        let mut cmd = std::process::Command::new(&prog);
        cmd.args(&args);
        if cfg!(target_os = "windows") {
            cmd.env("PYTHONUTF8", "1");
            cmd.env("PYTHONIOENCODING", "utf-8");
        }
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        match cmd.spawn() {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    serde_json::to_writer(&mut stdin, &payload).map_err(|e| e.to_string())?;
                }
                match child.wait_with_output() {
                    Ok(output) => {
                        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                        let parsed: serde_json::Value =
                            serde_json::from_str(&stdout).map_err(|e| {
                                format!(
                                    "{script_name}: ongeldige JSON ({e}). stderr={stderr} stdout={stdout}"
                                )
                            })?;
                        if parsed.get("ok").and_then(|v| v.as_bool()) != Some(true) {
                            return Err(parsed
                                .get("error")
                                .and_then(|v| v.as_str())
                                .unwrap_or("ipc mislukt")
                                .to_string());
                        }
                        return Ok(parsed
                            .get("data")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null));
                    }
                    Err(e) => last_err = e.to_string(),
                }
            }
            Err(e) => last_err = format!("{prog}: {e}"),
        }
    }
    Err(last_err)
}

fn run_cron_ipc(payload: serde_json::Value) -> Result<serde_json::Value, String> {
    run_python_ipc_script("cron_ipc_helper.py", payload, true)
}

fn run_profiles_ipc(payload: serde_json::Value) -> Result<serde_json::Value, String> {
    run_python_ipc_script("profiles_ipc_helper.py", payload, false)
}

fn run_skills_ipc(payload: serde_json::Value) -> Result<serde_json::Value, String> {
    run_python_ipc_script("skills_ipc_helper.py", payload, true)
}

#[tauri::command]
fn get_cron_jobs() -> Result<serde_json::Value, String> {
    run_cron_ipc(serde_json::json!({
        "op": "list",
        "include_disabled": true,
    }))
}

#[tauri::command]
fn create_cron_job(payload: serde_json::Value) -> Result<serde_json::Value, String> {
    let body = payload.get("body").cloned().unwrap_or(payload);
    let prompt = body
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let schedule = body
        .get("schedule")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    let deliver = body
        .get("deliver")
        .and_then(|v| v.as_str())
        .unwrap_or("local")
        .to_string();
    run_cron_ipc(serde_json::json!({
        "op": "create",
        "prompt": prompt,
        "schedule": schedule,
        "name": name,
        "deliver": deliver,
    }))
}

#[tauri::command]
fn pause_cron_job(job_id: String) -> Result<serde_json::Value, String> {
    run_cron_ipc(serde_json::json!({
        "op": "pause",
        "job_id": job_id.trim(),
    }))
}

#[tauri::command]
fn resume_cron_job(job_id: String) -> Result<serde_json::Value, String> {
    run_cron_ipc(serde_json::json!({
        "op": "resume",
        "job_id": job_id.trim(),
    }))
}

#[tauri::command]
fn trigger_cron_job(job_id: String) -> Result<serde_json::Value, String> {
    run_cron_ipc(serde_json::json!({
        "op": "trigger",
        "job_id": job_id.trim(),
    }))
}

#[tauri::command]
fn delete_cron_job(job_id: String) -> Result<(), String> {
    run_cron_ipc(serde_json::json!({
        "op": "delete",
        "job_id": job_id.trim(),
    }))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Logs commands
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct LogsResponse {
    file: String,
    lines: Vec<String>,
}

#[tauri::command]
fn get_logs(file: Option<String>, lines: Option<usize>) -> Result<LogsResponse, String> {
    let max_lines = lines.unwrap_or(200);
    let log_dir = hermes_logs_dir();
    let log_file = file.unwrap_or_else(|| "agent.log".to_string());
    let path = log_dir.join(&log_file);

    if !path.exists() {
        return Ok(LogsResponse {
            file: log_file,
            lines: Vec::new(),
        });
    }

    let content =
        std::fs::read_to_string(&path).map_err(|e| format!("Could not read log: {}", e))?;
    let all_lines: Vec<&str> = content.lines().collect();
    let total = all_lines.len();
    let start = total.saturating_sub(max_lines);
    let lines: Vec<String> = all_lines[start..].iter().map(|s| s.to_string()).collect();

    Ok(LogsResponse {
        file: log_file,
        lines,
    })
}

#[tauri::command]
fn get_sidecar_logs(
    lines: Option<usize>,
    state: State<'_, AppState>,
) -> Result<Vec<String>, String> {
    let max_lines = lines.unwrap_or(200);
    let rotator = state.log_rotator.lock().unwrap();
    Ok(rotator.read_recent(max_lines))
}

// ---------------------------------------------------------------------------
// Sidecar status
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct SidecarStatus {
    pub running: bool,
    pub protocol_version: u32,
    pub hermes_version: String,
}

#[tauri::command]
fn get_sidecar_status(state: State<'_, AppState>) -> Result<SidecarStatus, String> {
    let running = state.sidecar_child.lock().unwrap().is_some();
    let pv = state.protocol_version.load(Ordering::SeqCst);
    let hv = state.hermes_version.lock().unwrap().clone();

    Ok(SidecarStatus {
        running,
        protocol_version: pv,
        hermes_version: hv,
    })
}

// ---------------------------------------------------------------------------
// Env var commands
// ---------------------------------------------------------------------------

fn strip_utf8_bom(s: &str) -> &str {
    s.strip_prefix('\u{FEFF}').unwrap_or(s)
}

/// Zelfde parameters als ``agent.redact.mask_secret`` (zonder ``empty`` / kleur).
fn mask_secret_display(value: &str) -> String {
    let count = value.chars().count();
    if count == 0 {
        return String::new();
    }
    if count < 12 {
        return "***".to_string();
    }
    let head_s: String = value.chars().take(4).collect();
    let tail_s: String = value
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head_s}...{tail_s}")
}

fn load_dotenv_simple(path: &PathBuf) -> Result<HashMap<String, String>, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("Could not read .env: {}", e))?;
    let raw = strip_utf8_bom(&raw);
    let mut map = HashMap::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(eq_pos) = trimmed.find('=') {
            let key = trimmed[..eq_pos].trim().to_string();
            let mut val = trimmed[eq_pos + 1..].trim().to_string();
            if val.len() >= 2
                && ((val.starts_with('"') && val.ends_with('"'))
                    || (val.starts_with('\'') && val.ends_with('\'')))
            {
                val = val[1..val.len() - 1].to_string();
            }
            map.insert(key, val);
        }
    }
    Ok(map)
}

#[derive(Deserialize)]
struct EnvMetadataEmbed {
    vars: serde_json::Map<String, serde_json::Value>,
}

#[tauri::command]
fn get_env_vars() -> Result<serde_json::Value, String> {
    let meta: EnvMetadataEmbed = serde_json::from_str(ENV_METADATA_EMBED)
        .map_err(|e| format!("env-metadata embed: {}", e))?;

    let path = hermes_env_path();
    let disk = if path.exists() {
        load_dotenv_simple(&path)?
    } else {
        HashMap::new()
    };

    let mut out = serde_json::Map::new();
    for (key, meta_val) in meta.vars {
        let value = disk.get(&key);
        let is_set = value.map(|v| !v.is_empty()).unwrap_or(false);
        let redacted_value = match value {
            Some(v) if !v.is_empty() => serde_json::Value::String(mask_secret_display(v)),
            _ => serde_json::Value::Null,
        };

        let description = meta_val
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let url = meta_val
            .get("url")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let category = meta_val
            .get("category")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let is_password = meta_val
            .get("password")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let tools = meta_val
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let advanced = meta_val
            .get("advanced")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        out.insert(
            key,
            serde_json::json!({
                "is_set": is_set,
                "redacted_value": redacted_value,
                "description": description,
                "url": url,
                "category": category,
                "is_password": is_password,
                "tools": tools,
                "advanced": advanced,
            }),
        );
    }

    Ok(serde_json::Value::Object(out))
}

#[tauri::command]
fn set_env_var(key: String, value: String) -> Result<(), String> {
    let path = hermes_env_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Could not create directory: {}", e))?;
    }

    let existing = if path.exists() {
        std::fs::read_to_string(&path).unwrap_or_default()
    } else {
        String::new()
    };

    let mut lines: Vec<String> = existing
        .lines()
        .map(|l| l.to_string())
        .filter(|l| {
            if let Some(eq_pos) = l.find('=') {
                let existing_key = l[..eq_pos].trim();
                existing_key != key
            } else {
                true
            }
        })
        .collect();

    lines.push(format!("{}={}", key, value));

    std::fs::write(&path, lines.join("\n") + "\n")
        .map_err(|e| format!("Could not write .env: {}", e))
}

#[tauri::command]
fn delete_env_var(key: String) -> Result<(), String> {
    let path = hermes_env_path();
    if !path.exists() {
        return Ok(());
    }
    let existing =
        std::fs::read_to_string(&path).map_err(|e| format!("Could not read .env: {}", e))?;
    let key_trim = key.trim();
    let lines: Vec<String> = existing
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return true;
            }
            if let Some(eq_pos) = trimmed.find('=') {
                trimmed[..eq_pos].trim() != key_trim
            } else {
                true
            }
        })
        .map(|l| l.to_string())
        .collect();
    std::fs::write(&path, lines.join("\n") + "\n")
        .map_err(|e| format!("Could not write .env: {}", e))
}

#[tauri::command]
fn reveal_env_var(key: String) -> Result<String, String> {
    Err(format!(
        "Het tonen van ruwe waarden voor '{key}' is niet ingeschakeld in HermesOS desktop (veiligheid)."
    ))
}

// ---------------------------------------------------------------------------
// Skills command
// ---------------------------------------------------------------------------

fn skills_disabled_set(cfg: &serde_yaml::Value) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let Some(skills) = cfg.get("skills") else {
        return out;
    };
    let Some(arr) = skills.get("disabled").and_then(|d| d.as_sequence()) else {
        return out;
    };
    for v in arr {
        if let Some(s) = v.as_str() {
            out.insert(s.to_string());
        }
    }
    out
}

fn toggle_skill_via_yaml(name: String, enabled: bool) -> Result<(), String> {
    let mut cfg = load_config_yaml()?;
    if !cfg.is_mapping() {
        cfg = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }
    let root = cfg.as_mapping_mut().ok_or("config moet een mapping zijn")?;
    let skills_val = root
        .entry(serde_yaml::Value::String("skills".into()))
        .or_insert(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    let skills_map = skills_val
        .as_mapping_mut()
        .ok_or("skills moet een mapping zijn")?;
    let disabled_val = skills_map
        .entry(serde_yaml::Value::String("disabled".into()))
        .or_insert(serde_yaml::Value::Sequence(serde_yaml::Sequence::new()));
    let seq = disabled_val
        .as_sequence_mut()
        .ok_or("skills.disabled moet een lijst zijn")?;

    let mut set: std::collections::HashSet<String> = seq
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    if enabled {
        set.remove(&name);
    } else {
        set.insert(name);
    }
    let mut sorted: Vec<String> = set.into_iter().collect();
    sorted.sort();
    *seq = sorted.into_iter().map(serde_yaml::Value::String).collect();
    save_config_yaml(&cfg)
}

fn get_skills_fallback_scan() -> Result<Vec<SkillInfo>, String> {
    let cfg = load_config_yaml()?;
    let disabled = skills_disabled_set(&cfg);
    let skills_dir = hermes_skills_dir();
    if !skills_dir.exists() {
        return Ok(Vec::new());
    }

    let mut skills = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&skills_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let skill_md = path.join("SKILL.md");
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let description = if skill_md.exists() {
                    if let Ok(content) = std::fs::read_to_string(&skill_md) {
                        content
                            .lines()
                            .next()
                            .unwrap_or(&name)
                            .trim_start_matches('#')
                            .trim()
                            .to_string()
                    } else {
                        name.clone()
                    }
                } else {
                    name.clone()
                };
                skills.push(SkillInfo {
                    name: name.clone(),
                    description,
                    category: "user".to_string(),
                    enabled: !disabled.contains(&name),
                });
            }
        }
    }

    Ok(skills)
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub category: String,
    pub enabled: bool,
}

#[tauri::command]
fn get_skills() -> Result<Vec<SkillInfo>, String> {
    match run_skills_ipc(serde_json::json!({ "op": "list" })) {
        Ok(val) => serde_json::from_value::<Vec<SkillInfo>>(val).map_err(|e| e.to_string()),
        Err(_) => get_skills_fallback_scan(),
    }
}

#[tauri::command]
fn toggle_skill(name: String, enabled: bool) -> Result<(), String> {
    let skill = name.trim().to_string();
    if skill.is_empty() {
        return Err("skill naam leeg".into());
    }
    match run_skills_ipc(serde_json::json!({
        "op": "toggle",
        "name": skill,
        "enabled": enabled,
    })) {
        Ok(_) => Ok(()),
        Err(_) => toggle_skill_via_yaml(name.trim().to_string(), enabled),
    }
}

// ---------------------------------------------------------------------------
// Toolsets command
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ToolsetInfo {
    pub name: String,
    pub label: String,
    pub description: String,
    pub enabled: bool,
    pub configured: bool,
    pub tools: Vec<String>,
}

#[tauri::command]
fn get_toolsets() -> Result<Vec<ToolsetInfo>, String> {
    Ok(vec![
        ToolsetInfo {
            name: "terminal".to_string(),
            label: "Terminal".to_string(),
            description: "Voorbeeld — desktop toont geen volledige toolsetlijst; gebruik `hermes tools` of config.yaml.".to_string(),
            enabled: true,
            configured: true,
            tools: vec!["execute_code".to_string()],
        },
        ToolsetInfo {
            name: "file".to_string(),
            label: "File Operations".to_string(),
            description: "Voorbeeld — desktop toont geen volledige toolsetlijst; gebruik `hermes tools` of config.yaml.".to_string(),
            enabled: true,
            configured: true,
            tools: vec!["read_file".to_string(), "write_file".to_string()],
        },
    ])
}

// ---------------------------------------------------------------------------
// Application entry point
// ---------------------------------------------------------------------------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let log_dir = hermes_logs_dir();
    let _ = std::fs::create_dir_all(&log_dir);

    tauri::Builder::default()
        .manage(AppState {
            sidecar_child: Arc::new(Mutex::new(None)),
            log_rotator: Arc::new(Mutex::new(LogRotator::new(log_dir, 10, 5))),
            protocol_version: Arc::new(AtomicU32::new(0)),
            hermes_version: Arc::new(Mutex::new(String::new())),
        })
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let handle = app.handle().clone();
            if let Some(win) = app.get_webview_window("main") {
                if let Err(e) = win.maximize() {
                    tracing::warn!("Could not maximize main window on startup: {}", e);
                }
                let h = handle.clone();
                win.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = h.emit("app-close-requested", ());
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_agent_sidecar,
            send_agent_query,
            kill_agent,
            quit_app,
            restart_gateway,
            update_hermes,
            get_action_status,
            get_config,
            save_config,
            get_config_raw,
            save_config_raw,
            save_plugin_providers,
            get_hermes_home,
            get_defaults,
            get_schema,
            get_sessions,
            get_session_messages,
            delete_session,
            search_sessions,
            get_cron_jobs,
            create_cron_job,
            pause_cron_job,
            resume_cron_job,
            trigger_cron_job,
            delete_cron_job,
            get_analytics,
            get_models_analytics,
            get_model_info,
            get_model_options,
            get_auxiliary_models,
            set_model_assignment,
            get_profiles,
            create_profile,
            rename_profile,
            delete_profile,
            get_profile_setup_command,
            get_profile_soul,
            update_profile_soul,
            disconnect_oauth,
            start_oauth,
            submit_oauth,
            poll_oauth,
            cancel_oauth,
            get_logs,
            get_sidecar_logs,
            get_env_vars,
            set_env_var,
            delete_env_var,
            reveal_env_var,
            get_skills,
            toggle_skill,
            get_toolsets,
            get_sidecar_status,
            get_diagnostics,
            get_app_status,
        ])
        .build(tauri::generate_context!())
        .expect("error while building hermesos")
        .run(|app_handle, event| {
            if let tauri::RunEvent::Exit = event {
                if let Some(state) = app_handle.try_state::<AppState>() {
                    if let Ok(mut guard) = state.sidecar_child.lock() {
                        if let Some(child) = guard.take() {
                            tracing::info!("Killing sidecar on app exit");
                            let _ = child.kill();
                        }
                    }
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_hermes_home_default() {
        // Clear env var for test
        std::env::remove_var("HERMES_HOME");
        let home = resolve_hermes_home();
        assert!(home.to_string_lossy().contains(".hermes"));
    }

    #[test]
    fn test_config_path_exists() {
        let path = hermes_config_path();
        assert!(path.to_string_lossy().ends_with("config.yaml"));
    }

    #[test]
    fn fts_sanitize_matches_python_reference_cases() {
        assert_eq!(sanitize_fts5_query("hello AND"), "hello");
        assert_eq!(sanitize_fts5_query("a+b"), "a b");
        assert_eq!(sanitize_fts5_query("P2.2"), "\"P2.2\"");
        assert_eq!(sanitize_fts5_query("chat-send"), "\"chat-send\"");
        assert_eq!(sanitize_fts5_query("sp_new"), "\"sp_new\"");
    }

    #[test]
    fn normalize_config_for_web_flattens_model_dict_with_context_length() {
        let cfg = serde_json::json!({
            "model": {
                "default": "anthropic/claude-opus-4.6",
                "provider": "openrouter",
                "context_length": 200000
            }
        });
        let out = normalize_config_for_web(cfg);
        assert_eq!(out["model"], "anthropic/claude-opus-4.6");
        assert_eq!(out["model_context_length"], 200000);
    }

    #[test]
    fn normalize_config_for_web_string_model_has_zero_context_length() {
        let cfg = serde_json::json!({ "model": "anthropic/claude-sonnet-4" });
        let out = normalize_config_for_web(cfg);
        assert_eq!(out["model"], "anthropic/claude-sonnet-4");
        assert_eq!(out["model_context_length"], 0);
    }

    #[test]
    fn denormalize_preserves_provider_from_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var("HERMES_HOME").ok();
        std::env::set_var("HERMES_HOME", dir.path().as_os_str());
        let cfg_yaml = r#"model:
  default: old/model
  provider: openrouter
"#;
        std::fs::write(dir.path().join("config.yaml"), cfg_yaml).unwrap();

        let incoming = serde_json::json!({
            "model": "new/model",
            "model_context_length": 100000,
        });
        let out = denormalize_config_from_web(incoming).unwrap();
        let m = out["model"].as_object().expect("model should be object");
        assert_eq!(m["default"], "new/model");
        assert_eq!(m["provider"], "openrouter");
        assert_eq!(m["context_length"], 100000);

        if let Some(v) = prev {
            std::env::set_var("HERMES_HOME", v);
        } else {
            std::env::remove_var("HERMES_HOME");
        }
    }

    #[test]
    fn normalize_config_for_web_noninteger_context_length_becomes_zero() {
        let cfg = serde_json::json!({
            "model": { "default": "x/y", "context_length": "invalid" }
        });
        let out = normalize_config_for_web(cfg);
        assert_eq!(out["model_context_length"], 0);
    }
}
