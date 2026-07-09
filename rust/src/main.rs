mod audio;
mod flash;
mod jack_midi;
mod mode;
mod modhost;
mod websocket;

use std::path::PathBuf;
use std::sync::Arc;

use axum::{routing::{get, post}, Router};
use clap::Parser;
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::{error, info, warn};

use audio::{AudioConfig, AudioEngine};
use jack_midi::JackMidi;
use mode::BridgeState;
use modhost::ModHostClient;
use websocket::WsState;

/// Pedalboard bridge — WebSocket↔MIDI + audio patch switching.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Listen address for HTTP/WebSocket server.
    #[arg(short, long, default_value = ":8080")]
    addr: String,

    /// JACK MIDI port alias pattern for auto-connect.
    #[arg(short, long)]
    midi: Option<String>,

    /// Audio patch config JSON file (enables mod-host integration).
    #[arg(long)]
    audio: Option<PathBuf>,

    /// mod-host TCP address.
    #[arg(long, default_value = "localhost:5555")]
    modhost: String,
}

/// Combined application state.
#[derive(Clone)]
pub struct AppState {
    pub ws: Arc<WsState>,
    pub bridge: Arc<Mutex<BridgeState>>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    // Normalize listen address.
    let listen_addr = if args.addr.starts_with(':') {
        format!("0.0.0.0{}", args.addr)
    } else {
        args.addr.clone()
    };

    // Connect to mod-host (optional, for audio switching).
    let mut modhost_connected = false;
    let modhost = match ModHostClient::connect(&args.modhost).await {
        Ok(client) => {
            info!("mod-host connected: {}", args.modhost);
            modhost_connected = true;
            client
        }
        Err(e) => {
            warn!("mod-host: {e} (audio disabled, will retry)");
            ModHostClient::disconnected()
        }
    };

    // Load audio config.
    let audio_engine = if let Some(ref audio_path) = args.audio {
        match AudioConfig::load(audio_path) {
            Ok(config) => {
                info!("Audio engine enabled: {} patches configured", config.patches.len());
                Some(AudioEngine::new(config))
            }
            Err(e) => {
                warn!("Audio config: {e} (audio disabled)");
                None
            }
        }
    } else {
        None
    };

    // Shared bridge state.
    let bridge_state = Arc::new(Mutex::new(BridgeState {
        modhost,
        audio_engine,
        design_mode: false,
        modhost_addr: args.modhost.clone(),
    }));

    // Switch to patch 0 on startup (non-blocking).
    if modhost_connected {
        let bridge_for_init = bridge_state.clone();
        tokio::spawn(async move {
            let mut state = bridge_for_init.lock().await;
            let BridgeState { ref mut modhost, ref mut audio_engine, .. } = *state;
            if let Some(engine) = audio_engine
                && let Err(e) = engine.switch_patch(modhost, 0).await {
                    tracing::warn!("Initial patch switch failed: {e}");
                }
        });
    }

    // Start JACK MIDI client.
    let (jack, mut midi_rx) = JackMidi::new("pedalboard-bridge")
        .expect("Failed to create JACK MIDI client");

    if let Some(pattern) = &args.midi {
        jack.auto_connect(pattern.clone());
    }

    // WebSocket channels.
    let (monitor_tx, _) = broadcast::channel::<Vec<u8>>(256);
    let (sysex_tx, _) = broadcast::channel::<Vec<u8>>(64);
    let (midi_out_tx, mut midi_out_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    let ws_state = Arc::new(WsState {
        midi_out_tx,
        monitor_tx: monitor_tx.clone(),
        sysex_tx: sysex_tx.clone(),
    });

    // Forward MIDI output channel to JACK.
    let jack = Arc::new(jack);
    let jack_for_output = jack.clone();
    tokio::spawn(async move {
        while let Some(data) = midi_out_rx.recv().await {
            jack_for_output.send(&data);
        }
    });

    // MIDI processing task.
    let bridge_for_midi = bridge_state.clone();
    let monitor_tx_midi = monitor_tx.clone();
    let sysex_tx_midi = sysex_tx.clone();
    tokio::spawn(async move {
        let mut sysex_buf: Vec<u8> = Vec::new();

        while let Some(data) = midi_rx.recv().await {
            // Forward raw bytes to monitor clients.
            let _ = monitor_tx_midi.send(data.clone());

            // Assemble SysEx messages.
            for &b in &data {
                if b == 0xF0 {
                    sysex_buf = vec![b];
                } else if b == 0xF7 && !sysex_buf.is_empty() {
                    sysex_buf.push(b);
                    info!("MIDI IN:  {}", hex_encode(&sysex_buf));
                    let _ = sysex_tx_midi.send(sysex_buf.clone());
                    sysex_buf.clear();
                } else if !sysex_buf.is_empty() {
                    sysex_buf.push(b);
                }
            }

            // Detect Program Change → switch audio patch.
            if data.len() >= 2 {
                let mut i = 0;
                while i < data.len() {
                    let status = data[i];
                    if status & 0x80 == 0 {
                        i += 1;
                        continue;
                    }
                    let msg_type = status & 0xF0;
                    if msg_type == 0xC0 && i + 1 < data.len() {
                        let program = data[i + 1] as usize;
                        let bridge = bridge_for_midi.clone();
                        tokio::spawn(async move {
                            let mut state = bridge.lock().await;
                            if state.design_mode || !state.modhost.is_connected() {
                                return;
                            }
                            let BridgeState { ref mut modhost, ref mut audio_engine, .. } = *state;
                            if let Some(engine) = audio_engine.as_mut()
                                && let Err(e) = engine.switch_patch(modhost, program).await {
                                    error!("Patch switch failed: {e}");
                                }
                        });
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
        }
    });

    // mod-host reconnect loop (background).
    let bridge_for_reconnect = bridge_state.clone();
    let modhost_addr = args.modhost.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let mut state = bridge_for_reconnect.lock().await;
            if state.design_mode || state.modhost.is_connected() {
                continue;
            }
            if let Ok(client) = ModHostClient::connect(&modhost_addr).await {
                info!("mod-host reconnected");
                state.modhost = client;
                let BridgeState { ref mut modhost, ref mut audio_engine, .. } = *state;
                if let Some(engine) = audio_engine.as_mut() {
                    let _ = engine.switch_patch(modhost, 0).await;
                }
            }
        }
    });

    // Build HTTP router.
    let app = Router::new()
        .route("/", get(handle_status))
        .route("/raw", get(websocket::handle_raw).with_state(ws_state.clone()))
        .route("/monitor", get(websocket::handle_monitor).with_state(ws_state))
        .route("/flash", post(flash::handle_flash))
        .route("/mode", get(mode::handle_mode).post(mode::handle_mode))
        .with_state(bridge_state)
        .layer(tower_http::cors::CorsLayer::permissive());

    info!("pedalboard-bridge listening on {}", listen_addr);
    let listener = tokio::net::TcpListener::bind(&listen_addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn handle_status() -> &'static str {
    concat!("pedalboard-bridge ", env!("CARGO_PKG_VERSION"), "\n")
}

fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}
