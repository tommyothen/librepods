//! Local IPC for external tooling (status bars, shell plugins, scripts).
//!
//! State: a JSON snapshot is written to `$XDG_RUNTIME_DIR/librepods/state.json`
//! after every AACP event, so tools can watch or poll one small file instead of
//! speaking the AirPods protocol themselves (only one process can hold the
//! L2CAP connection).
//!
//! Control: `$XDG_RUNTIME_DIR/librepods/control.sock` accepts newline-delimited
//! JSON commands and replies with one JSON line per command. The `librepods
//! ctl` subcommand is a thin client for this socket.

use crate::bluetooth::aacp::{
    AACPManager, BatteryComponent, BatteryStatus, ControlCommandIdentifiers, EarDetectionStatus,
};
use crate::bluetooth::managers::DeviceManagers;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::RwLock;

/// Bump when the state.json schema changes incompatibly.
const STATE_VERSION: u32 = 1;

fn ipc_dir() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(base).join("librepods")
}

pub fn state_path() -> PathBuf {
    ipc_dir().join("state.json")
}

pub fn socket_path() -> PathBuf {
    ipc_dir().join("control.sock")
}

#[derive(Debug, Clone, Serialize)]
struct BatterySnapshot {
    component: &'static str,
    level: u8,
    charging: bool,
    connected: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct StateSnapshot {
    version: u32,
    connected: bool,
    address: Option<String>,
    name: Option<String>,
    battery: Vec<BatterySnapshot>,
    ear_detection: Vec<&'static str>,
    noise_control_mode: Option<&'static str>,
    allowed_noise_control_modes: Vec<&'static str>,
    conversation_awareness: Option<bool>,
    personalized_volume: Option<bool>,
}

impl StateSnapshot {
    fn disconnected() -> Self {
        StateSnapshot {
            version: STATE_VERSION,
            connected: false,
            address: None,
            name: None,
            battery: Vec::new(),
            ear_detection: Vec::new(),
            noise_control_mode: None,
            allowed_noise_control_modes: Vec::new(),
            conversation_awareness: None,
            personalized_volume: None,
        }
    }
}

fn noise_mode_name(byte: u8) -> Option<&'static str> {
    match byte {
        0x01 => Some("off"),
        0x02 => Some("noise_cancellation"),
        0x03 => Some("transparency"),
        0x04 => Some("adaptive"),
        _ => None,
    }
}

fn noise_mode_byte(name: &str) -> Option<u8> {
    match name {
        "off" => Some(0x01),
        "noise_cancellation" | "anc" | "nc" => Some(0x02),
        "transparency" => Some(0x03),
        "adaptive" => Some(0x04),
        _ => None,
    }
}

async fn build_snapshot(manager: &AACPManager) -> StateSnapshot {
    let state = manager.state.lock().await;

    let battery = state
        .battery_info
        .iter()
        .map(|b| BatterySnapshot {
            component: match b.component {
                BatteryComponent::Headphone => "headphone",
                BatteryComponent::Left => "left",
                BatteryComponent::Right => "right",
                BatteryComponent::Case => "case",
            },
            level: b.level,
            charging: b.status == BatteryStatus::Charging,
            connected: b.status != BatteryStatus::Disconnected,
        })
        .collect();

    let ear_detection = state
        .ear_detection_status
        .iter()
        .map(|s| match s {
            EarDetectionStatus::InEar => "in_ear",
            EarDetectionStatus::OutOfEar => "out_of_ear",
            EarDetectionStatus::InCase => "in_case",
            EarDetectionStatus::Disconnected => "disconnected",
        })
        .collect();

    let find_command = |id: ControlCommandIdentifiers| {
        state
            .control_command_status_list
            .iter()
            .find(|s| s.identifier == id)
            .and_then(|s| s.value.first().copied())
    };

    let noise_control_mode =
        find_command(ControlCommandIdentifiers::ListeningMode).and_then(noise_mode_name);
    let allow_off = find_command(ControlCommandIdentifiers::AllowOffOption) == Some(0x01);
    let mut allowed_noise_control_modes =
        vec!["noise_cancellation", "transparency", "adaptive"];
    if allow_off {
        allowed_noise_control_modes.insert(0, "off");
    }
    // These report 0x01 = enabled, 0x02 = disabled; absent until the AirPods
    // send their initial status dump.
    let toggle = |id| find_command(id).map(|v| v == 0x01);

    let address = state.airpods_mac.map(|a| a.to_string());
    let name = address
        .as_ref()
        .and_then(|a| state.devices.get(a))
        .map(|d| d.name.clone());

    StateSnapshot {
        version: STATE_VERSION,
        connected: state.sender.is_some(),
        address,
        name,
        battery,
        ear_detection,
        noise_control_mode,
        allowed_noise_control_modes,
        conversation_awareness: toggle(ControlCommandIdentifiers::ConversationDetectConfig),
        personalized_volume: toggle(ControlCommandIdentifiers::AdaptiveVolumeConfig),
    }
}

fn write_snapshot(snapshot: &StateSnapshot) {
    let dir = ipc_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!("Failed to create IPC dir {}: {}", dir.display(), e);
        return;
    }
    let json = match serde_json::to_string(snapshot) {
        Ok(j) => j,
        Err(e) => {
            warn!("Failed to serialize IPC state: {}", e);
            return;
        }
    };
    // Write-then-rename so readers never observe a half-written file.
    let tmp = dir.join("state.json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, state_path());
    }
}

/// Snapshot the given device's state to state.json. Called from the AACP
/// event loop, so the file tracks every battery/ear/control change.
pub async fn publish_state(manager: &AACPManager) {
    let snapshot = build_snapshot(manager).await;
    write_snapshot(&snapshot);
}

/// Overwrite state.json with a disconnected snapshot (startup and disconnect).
pub fn publish_disconnected() {
    write_snapshot(&StateSnapshot::disconnected());
}

#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
enum Command {
    GetState,
    SetNoiseControl { mode: String },
    SetConversationAwareness { enabled: bool },
}

async fn first_aacp_manager(
    device_managers: &Arc<RwLock<HashMap<String, DeviceManagers>>>,
) -> Option<Arc<AACPManager>> {
    let managers = device_managers.read().await;
    managers.values().find_map(|m| m.get_aacp())
}

async fn handle_command(
    line: &str,
    device_managers: &Arc<RwLock<HashMap<String, DeviceManagers>>>,
) -> String {
    let command: Command = match serde_json::from_str(line) {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"ok":false,"error":"invalid command: {}"}}"#, e),
    };

    match command {
        Command::GetState => {
            let snapshot = match first_aacp_manager(device_managers).await {
                Some(manager) => build_snapshot(&manager).await,
                None => StateSnapshot::disconnected(),
            };
            serde_json::to_string(&snapshot)
                .unwrap_or_else(|e| format!(r#"{{"ok":false,"error":"{}"}}"#, e))
        }
        Command::SetNoiseControl { mode } => {
            let Some(byte) = noise_mode_byte(&mode) else {
                return r#"{"ok":false,"error":"unknown mode; expected off|noise_cancellation|transparency|adaptive"}"#.to_string();
            };
            send_control(device_managers, ControlCommandIdentifiers::ListeningMode, byte).await
        }
        Command::SetConversationAwareness { enabled } => {
            let byte = if enabled { 0x01 } else { 0x02 };
            send_control(
                device_managers,
                ControlCommandIdentifiers::ConversationDetectConfig,
                byte,
            )
            .await
        }
    }
}

async fn send_control(
    device_managers: &Arc<RwLock<HashMap<String, DeviceManagers>>>,
    identifier: ControlCommandIdentifiers,
    value: u8,
) -> String {
    let Some(manager) = first_aacp_manager(device_managers).await else {
        return r#"{"ok":false,"error":"no connected device"}"#.to_string();
    };
    match manager.send_control_command(identifier, &[value]).await {
        Ok(()) => r#"{"ok":true}"#.to_string(),
        Err(e) => format!(r#"{{"ok":false,"error":"{}"}}"#, e),
    }
}

/// Listen on the control socket, one line per command, one JSON line back.
pub async fn start_control_server(
    device_managers: Arc<RwLock<HashMap<String, DeviceManagers>>>,
) {
    let path = socket_path();
    if let Err(e) = std::fs::create_dir_all(ipc_dir()) {
        error!("Failed to create IPC dir: {}", e);
        return;
    }
    // Remove a stale socket from a previous run; bind fails on EADDRINUSE otherwise.
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind control socket {}: {}", path.display(), e);
            return;
        }
    };
    info!("IPC control socket listening on {}", path.display());

    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let device_managers = device_managers.clone();
        tokio::spawn(async move {
            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let mut response = handle_command(&line, &device_managers).await;
                response.push('\n');
                if write_half.write_all(response.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
    }
}

/// Blocking client used by the `librepods ctl` subcommand. Sends one command
/// and returns the server's single-line JSON reply.
pub fn ctl_request(command: serde_json::Value) -> Result<String, String> {
    use std::io::{BufRead, BufReader, Write};

    let path = socket_path();
    let mut stream = std::os::unix::net::UnixStream::connect(&path).map_err(|e| {
        format!(
            "cannot connect to {} ({}); is librepods running?",
            path.display(),
            e
        )
    })?;
    writeln!(stream, "{}", command).map_err(|e| e.to_string())?;
    let mut response = String::new();
    BufReader::new(&stream)
        .read_line(&mut response)
        .map_err(|e| e.to_string())?;
    Ok(response.trim_end().to_string())
}
