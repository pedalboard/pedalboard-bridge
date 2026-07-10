use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Query, State},
    http::{Method, StatusCode},
    response::IntoResponse,
};
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::info;

use crate::audio::AudioEngine;
use crate::modhost::ModHostClient;

/// Shared bridge state for mode management.
pub struct BridgeState {
    pub modhost: ModHostClient,
    pub audio_engine: Option<AudioEngine>,
    pub design_mode: bool,
    pub modhost_addr: String,
    /// JACK MIDI sender for PE SysEx to firmware.
    pub midi_tx: Option<Arc<crate::jack_midi::JackMidi>>,
    /// SysEx broadcast receiver for firmware replies.
    pub sysex_tx: Option<tokio::sync::broadcast::Sender<Vec<u8>>>,
    /// JACK MIDI client reference (for connection status).
    pub jack: Option<Arc<crate::jack_midi::JackMidi>>,
}

#[derive(Deserialize)]
pub struct ModeQuery {
    #[serde(default)]
    pub set: Option<String>,
}

/// Handler for /mode endpoint.
/// GET: returns current mode.
/// POST ?set=design: disconnect mod-host, start MOD UI.
/// POST ?set=live: stop MOD UI, reconnect, restore audio patch.
pub async fn handle_mode(
    method: Method,
    Query(query): Query<ModeQuery>,
    State(state): State<Arc<Mutex<BridgeState>>>,
) -> impl IntoResponse {
    let mut bridge = state.lock().await;

    if method == Method::GET {
        let mode = if bridge.design_mode { "design" } else { "live" };
        return (StatusCode::OK, format!("{mode}\n"));
    }

    let mode = match &query.set {
        Some(m) => m.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "use ?set=design or ?set=live\n".to_string(),
            );
        }
    };

    match mode.as_str() {
        "design" => {
            bridge.design_mode = true;
            // Disconnect from mod-host so MOD UI can connect.
            bridge.modhost = ModHostClient::disconnected();
            tokio::time::sleep(Duration::from_millis(500)).await;
            let _ = Command::new("sudo")
                .args(["systemctl", "start", "pedalboard-modui"])
                .status();
            info!("Mode: design (MOD UI at http://localhost:8888/)");
            (StatusCode::OK, "design\n".to_string())
        }
        "live" => {
            // Stop MOD UI, reconnect bridge to mod-host.
            let _ = Command::new("sudo")
                .args(["systemctl", "stop", "pedalboard-modui"])
                .status();
            bridge.design_mode = false;
            let addr = bridge.modhost_addr.clone();
            match ModHostClient::connect(&addr).await {
                Ok(client) => {
                    bridge.modhost = client;
                    // Restore audio patch 0.
                    let BridgeState {
                        ref mut modhost,
                        ref mut audio_engine,
                        ..
                    } = *bridge;
                    if let Some(engine) = audio_engine.as_mut()
                        && let Err(e) = engine.switch_snapshot_by_index(modhost, 0).await
                    {
                        tracing::warn!("Failed to restore audio patch: {e}");
                    }
                    info!("Mode: live (bridge controls mod-host)");
                    (StatusCode::OK, "live\n".to_string())
                }
                Err(e) => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("mod-host reconnect failed: {e}\n"),
                ),
            }
        }
        _ => (
            StatusCode::BAD_REQUEST,
            "use ?set=design or ?set=live\n".to_string(),
        ),
    }
}
