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

/// Operating mode of the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Full stack: audio + MOD UI + SSH + WiFi + avahi + cron.
    #[default]
    Studio,
    /// Minimal audio-only runtime: non-audio services stopped via systemd isolate.
    Gig,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Studio => "studio",
            Mode::Gig => "gig",
        }
    }
}

/// Shared bridge state for mode management.
pub struct BridgeState {
    pub modhost: ModHostClient,
    pub audio_engine: Option<AudioEngine>,
    pub mode: Mode,
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

/// Parse a gig-mode SysEx message from the controller.
///
/// Returns `Some(Mode::Gig)` for `F0 7D 01 01 F7` (enter gig)
/// and `Some(Mode::Studio)` for `F0 7D 01 00 F7` (exit gig).
/// Returns `None` for any other message.
pub fn parse_gig_sysex(data: &[u8]) -> Option<Mode> {
    match data {
        [0xF0, 0x7D, 0x01, 0x01, 0xF7] => Some(Mode::Gig),
        [0xF0, 0x7D, 0x01, 0x00, 0xF7] => Some(Mode::Studio),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_gig_sysex_parsed() {
        assert_eq!(
            parse_gig_sysex(&[0xF0, 0x7D, 0x01, 0x01, 0xF7]),
            Some(Mode::Gig)
        );
    }

    #[test]
    fn exit_gig_sysex_parsed() {
        assert_eq!(
            parse_gig_sysex(&[0xF0, 0x7D, 0x01, 0x00, 0xF7]),
            Some(Mode::Studio)
        );
    }

    #[test]
    fn unrelated_sysex_returns_none() {
        assert_eq!(parse_gig_sysex(&[0xF0, 0x7E, 0x01, 0x01, 0xF7]), None);
    }

    #[test]
    fn pe_sysex_returns_none() {
        // MIDI-CI PE SysEx should not be mistaken for gig commands
        assert_eq!(parse_gig_sysex(&[0xF0, 0x7E, 0x7F, 0x0D, 0x20, 0xF7]), None);
    }

    #[test]
    fn truncated_sysex_returns_none() {
        assert_eq!(parse_gig_sysex(&[0xF0, 0x7D, 0x01]), None);
        assert_eq!(parse_gig_sysex(&[]), None);
    }

    #[test]
    fn extra_bytes_returns_none() {
        assert_eq!(parse_gig_sysex(&[0xF0, 0x7D, 0x01, 0x01, 0x00, 0xF7]), None);
    }

    #[test]
    fn wrong_command_byte_returns_none() {
        assert_eq!(parse_gig_sysex(&[0xF0, 0x7D, 0x01, 0x02, 0xF7]), None);
    }
}
/// Handler for /mode endpoint.
/// GET:             returns current mode ("studio" or "gig").
/// POST ?set=studio: restore pedalboard-dev.target if coming from gig, reconnect mod-host.
/// POST ?set=gig:    isolate to pedalboard-gig.target (minimal runtime, audio only).
pub async fn handle_mode(
    method: Method,
    Query(query): Query<ModeQuery>,
    State(state): State<Arc<Mutex<BridgeState>>>,
) -> impl IntoResponse {
    let mut bridge = state.lock().await;

    if method == Method::GET {
        return (StatusCode::OK, format!("{}\n", bridge.mode.as_str()));
    }

    let requested = match &query.set {
        Some(m) => m.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "use ?set=studio or ?set=gig\n".to_string(),
            );
        }
    };

    match requested.as_str() {
        "studio" => {
            if bridge.mode == Mode::Gig {
                // Restore dev target (re-enables SSH, WiFi, avahi, MOD UI, etc.).
                let result = Command::new("sudo")
                    .args(["systemctl", "isolate", "pedalboard-dev.target"])
                    .status();
                match result {
                    Ok(s) if s.success() => {}
                    Ok(s) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("systemctl isolate failed with exit code: {}\n", s),
                        );
                    }
                    Err(e) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("failed to execute systemctl: {e}\n"),
                        );
                    }
                }
                tokio::time::sleep(Duration::from_millis(1000)).await;
                // Remove the nologin file systemd creates during isolate
                // so SSH logins are allowed again.
                let _ = std::fs::remove_file("/run/nologin");
            }
            // Reconnect bridge to mod-host.
            let addr = bridge.modhost_addr.clone();
            match ModHostClient::connect(&addr).await {
                Ok(client) => {
                    bridge.modhost = client;
                    bridge.mode = Mode::Studio;
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
                    info!("Mode: studio");
                    (StatusCode::OK, "studio\n".to_string())
                }
                Err(e) => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("mod-host reconnect failed: {e}\n"),
                ),
            }
        }
        "gig" => {
            // Isolate to gig target (stops SSH, WiFi, avahi, cron, journald, getty, MOD UI).
            let result = Command::new("sudo")
                .args(["systemctl", "isolate", "pedalboard-gig.target"])
                .status();
            match result {
                Ok(status) if status.success() => {
                    bridge.mode = Mode::Gig;
                    info!("Mode: gig (minimal runtime, non-audio services stopped)");
                    (StatusCode::OK, "gig\n".to_string())
                }
                Ok(status) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("systemctl isolate failed with exit code: {}\n", status),
                ),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to execute systemctl: {e}\n"),
                ),
            }
        }
        _ => (
            StatusCode::BAD_REQUEST,
            "use ?set=studio or ?set=gig\n".to_string(),
        ),
    }
}
