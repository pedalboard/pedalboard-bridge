use std::sync::Arc;
use std::time::Duration;

use axum::{extract::State, http::StatusCode, response::IntoResponse};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use pedalboard_config::{
    Setlist,
    convert::{yaml_global_to_protocol, yaml_to_presets},
    validate::validate,
};

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

    // Deploy MIDI presets to firmware via PE SysEx over JACK MIDI.
    let presets = yaml_to_presets(&setlist);
    let preset_count = presets.len();

    {
        let bridge = state.lock().await;
        if let (Some(jack), Some(sysex_tx)) = (&bridge.midi_tx, &bridge.sysex_tx) {
            let mut sysex_rx = sysex_tx.subscribe();

            // Upload global config if present.
            if let Some(ref global_yaml) = setlist.global {
                let gc = yaml_global_to_protocol(global_yaml);
                if let Ok(serialized) = postcard::to_allocvec(&gc) {
                    let msg = midi_controller::property_exchange::build_set_inquiry(
                        [0x10, 0x20, 0x30, 0x40],
                        [0x01, 0x02, 0x03, 0x04],
                        0x7F,
                        midi_controller::config::GLOBAL_CONFIG_RESOURCE,
                        &serialized,
                    );
                    jack.send(&msg);
                    // Wait for ACK.
                    let _ = tokio::time::timeout(Duration::from_secs(5), sysex_rx.recv()).await;
                }
            }

            // Upload presets.
            for (idx, preset) in presets.iter().enumerate() {
                if let Ok(serialized) = postcard::to_allocvec(preset) {
                    let msg = midi_controller::property_exchange::build_set_inquiry(
                        [0x10, 0x20, 0x30, 0x40],
                        [0x01, 0x02, 0x03, 0x04],
                        idx as u8 + 1,
                        idx as u8,
                        &serialized,
                    );
                    jack.send(&msg);
                    // Wait for ACK.
                    let _ = tokio::time::timeout(Duration::from_secs(5), sysex_rx.recv()).await;

                    // Brief delay between presets for firmware flash writes.
                    if presets.len() > 4 {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                }
            }

            // Clear stale presets.
            let max_presets: u8 = 32;
            for idx in (preset_count as u8)..max_presets {
                let msg = midi_controller::property_exchange::build_set_inquiry(
                    [0x10, 0x20, 0x30, 0x40],
                    [0x01, 0x02, 0x03, 0x04],
                    idx + 1,
                    idx,
                    &[], // empty = delete
                );
                jack.send(&msg);
                let _ = tokio::time::timeout(Duration::from_millis(500), sysex_rx.recv()).await;
            }

            info!("Deploy: uploaded {} presets to firmware", preset_count);
        } else {
            warn!("Deploy: JACK MIDI not available, skipping firmware upload");
        }
    }

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
