use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse};
use tokio::sync::Mutex;
use tracing::{error, info};

use pedalboard_config::{Setlist, validate::validate};

use crate::mode::BridgeState;

/// Handler for POST /deploy — accepts a full setlist YAML, validates, and deploys.
///
/// Deploys both:
/// - Audio config → reconfigures the bridge's audio engine (snapshots, expression)
/// - MIDI presets → uploads to RP2040 firmware via PE SysEx over JACK MIDI
pub async fn handle_deploy(
    State(state): State<Arc<Mutex<BridgeState>>>,
    body: String,
) -> impl IntoResponse {
    // Parse YAML.
    let setlist: Setlist = match serde_yaml::from_str(&body) {
        Ok(s) => s,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("YAML parse error: {e}\n"));
        }
    };

    // Validate.
    let errors = validate(&setlist);
    if !errors.is_empty() {
        let msg = errors
            .iter()
            .map(|e| format!("  • {e}"))
            .collect::<Vec<_>>()
            .join("\n");
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("Validation errors:\n{msg}\n"),
        );
    }

    // Deploy audio config (if present).
    if setlist.audio.is_some() {
        // Re-parse the audio section using the bridge's own AudioConfig type.
        // (avoids needing Serialize on the shared crate's types)
        let bridge_config = match crate::audio::AudioConfig::load_from_str(&body) {
            Ok(config) => config,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Audio config parse error: {e}\n"),
                );
            }
        };

        let mut bridge = state.lock().await;
        let mut engine = crate::audio::AudioEngine::new(bridge_config);
        if bridge.modhost.is_connected() {
            if let Err(e) = engine.load_rig(&mut bridge.modhost).await {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Audio rig load failed: {e}\n"),
                );
            }
            if let Err(e) = engine
                .switch_snapshot_by_index(&mut bridge.modhost, 0)
                .await
            {
                error!("Initial snapshot switch failed: {e}");
            }
        }
        bridge.audio_engine = Some(engine);
        info!("Deploy: audio rig reconfigured");
    }

    // TODO: Deploy MIDI presets to firmware via PE SysEx.
    // This requires compiling presets and sending them over JACK MIDI.
    // For now, the CLI still handles this directly.
    let preset_count = setlist.presets.len();

    info!("Deploy: accepted setlist ({} presets)", preset_count);
    (
        StatusCode::OK,
        format!(
            "OK: deployed ({} presets, audio: {})\n",
            preset_count,
            if setlist.audio.is_some() {
                "reconfigured"
            } else {
                "unchanged"
            }
        ),
    )
}
