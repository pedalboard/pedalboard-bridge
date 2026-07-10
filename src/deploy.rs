use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use tokio::sync::Mutex;
use tokio_stream::StreamExt;

use pedalboard_config::{
    Setlist,
    convert::{yaml_global_to_protocol, yaml_to_presets},
    validate::validate,
};

use crate::mode::BridgeState;

/// Handler for POST /deploy — accepts a full setlist YAML, validates, and deploys.
/// Returns a streaming text response with progress updates per line.
pub async fn handle_deploy(State(state): State<Arc<Mutex<BridgeState>>>, body: String) -> Response {
    // Parse YAML.
    let setlist: Setlist = match serde_yaml::from_str(&body) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("error: YAML parse error: {e}\n"),
            )
                .into_response();
        }
    };

    // Validate.
    let errors = validate(&setlist);
    if !errors.is_empty() {
        let msg = std::iter::once("error: validation failed\n".to_string())
            .chain(errors.iter().map(|e| format!("  • {e}\n")))
            .collect::<String>();
        return (StatusCode::UNPROCESSABLE_ENTITY, msg).into_response();
    }

    // Stream progress as chunked text response.
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(32);

    // Spawn the deploy work in a background task.
    let body_clone = body.clone();
    tokio::spawn(async move {
        let _ = tx.send("validate: ok\n".to_string()).await;

        // Deploy audio config.
        if setlist.audio.is_some() {
            match crate::audio::AudioConfig::load_from_str(&body_clone) {
                Ok(config) => {
                    let plugin_count = config.plugins.len();
                    let conn_count = config.connections.len();
                    let mut engine = crate::audio::AudioEngine::new(config);

                    let mut bridge = state.lock().await;
                    if bridge.modhost.is_connected() {
                        match engine.load_rig(&mut bridge.modhost).await {
                            Ok(()) => {
                                let _ = tx
                                    .send(format!(
                                        "audio: rig loaded ({plugin_count} plugins, {conn_count} connections)\n"
                                    ))
                                    .await;

                                if let Err(e) = engine
                                    .switch_snapshot_by_index(&mut bridge.modhost, 0)
                                    .await
                                {
                                    let _ = tx
                                        .send(format!("audio: snapshot switch failed: {e}\n"))
                                        .await;
                                } else {
                                    let name =
                                        engine.active_snapshot_name().unwrap_or("?").to_string();
                                    let _ =
                                        tx.send(format!("audio: snapshot '{name}' active\n")).await;
                                }
                            }
                            Err(e) => {
                                let _ = tx.send(format!("audio: rig load failed: {e}\n")).await;
                            }
                        }
                    } else {
                        let _ = tx
                            .send("audio: mod-host not connected (skipped)\n".to_string())
                            .await;
                    }
                    bridge.audio_engine = Some(engine);
                }
                Err(e) => {
                    let _ = tx.send(format!("audio: parse error: {e}\n")).await;
                }
            }
        } else {
            let _ = tx.send("audio: unchanged\n".to_string()).await;
        }

        // Deploy MIDI presets.
        let presets = yaml_to_presets(&setlist);
        let preset_count = presets.len();

        let bridge = state.lock().await;
        if let (Some(jack), Some(sysex_tx)) = (&bridge.midi_tx, &bridge.sysex_tx) {
            let mut sysex_rx = sysex_tx.subscribe();

            // Upload global config.
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
                    match tokio::time::timeout(Duration::from_secs(5), sysex_rx.recv()).await {
                        Ok(Ok(_)) => {
                            let _ = tx.send("global: ACK ✓\n".to_string()).await;
                        }
                        _ => {
                            let _ = tx.send("global: no reply (timeout)\n".to_string()).await;
                        }
                    }
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
                    match tokio::time::timeout(Duration::from_secs(5), sysex_rx.recv()).await {
                        Ok(Ok(_)) => {
                            let _ = tx
                                .send(format!("preset {idx} \"{}\": ACK ✓\n", preset.name))
                                .await;
                        }
                        _ => {
                            let _ = tx
                                .send(format!(
                                    "preset {idx} \"{}\": no reply (timeout)\n",
                                    preset.name
                                ))
                                .await;
                        }
                    }

                    if presets.len() > 4 {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                }
            }

            // Clear stale presets.
            let max_presets: u8 = 32;
            let mut cleared = 0u8;
            for idx in (preset_count as u8)..max_presets {
                let msg = midi_controller::property_exchange::build_set_inquiry(
                    [0x10, 0x20, 0x30, 0x40],
                    [0x01, 0x02, 0x03, 0x04],
                    idx + 1,
                    idx,
                    &[],
                );
                jack.send(&msg);
                let _ = tokio::time::timeout(Duration::from_millis(500), sysex_rx.recv()).await;
                cleared += 1;
            }
            if cleared > 0 {
                let _ = tx.send(format!("cleared {cleared} stale slot(s)\n")).await;
            }

            let _ = tx
                .send(format!("done: deployed ({preset_count} presets)\n"))
                .await;
        } else {
            let _ = tx
                .send("midi: JACK not available (skipped firmware upload)\n".to_string())
                .await;
            let _ = tx.send("done\n".to_string()).await;
        }
    });

    // Convert channel to a streaming body.
    let stream =
        tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<_, std::convert::Infallible>);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::TRANSFER_ENCODING, "chunked")
        .body(Body::from_stream(stream))
        .unwrap()
}
