#![allow(dead_code)]

mod audio;
mod deploy;
mod flash;
mod jack_midi;
mod mode;
mod modhost;
mod websocket;

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Router,
    routing::{get, post},
};
use clap::Parser;
use tokio::sync::{Mutex, broadcast, mpsc};
use tracing::{error, info, warn};

use audio::{AudioConfig, AudioEngine};
use jack_midi::JackMidi;
use mode::BridgeState;
use modhost::ModHostClient;
use websocket::WsState;

/// Pedalboard bridge — WebSocket↔MIDI + audio patch switching.
#[derive(Parser)]
#[command(version = concat!(env!("CARGO_PKG_VERSION"), "-", env!("GIT_HASH")), about)]
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

    // 1. Start JACK MIDI client FIRST (blocking call, must happen before async work).
    let (jack, mut midi_rx) = tokio::task::spawn_blocking(|| {
        JackMidi::new("pedalboard-bridge").expect("Failed to create JACK MIDI client")
    })
    .await
    .expect("JACK init task panicked");

    let jack = Arc::new(jack);
    if let Some(pattern) = &args.midi {
        jack.auto_connect(pattern.clone());
    }

    // 2. Connect to mod-host (optional, for audio switching).
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

    // 3. Load audio config.
    let audio_engine = if let Some(ref audio_path) = args.audio {
        match AudioConfig::load(audio_path) {
            Ok(config) => {
                info!(
                    "Audio engine enabled: {} patches configured",
                    config.snapshots.len()
                );
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

    // 4. Shared bridge state.
    let bridge_state = Arc::new(Mutex::new(BridgeState {
        modhost,
        audio_engine,
        design_mode: false,
        modhost_addr: args.modhost.clone(),
    }));

    // 5. Switch to patch 0 on startup (non-blocking).
    if modhost_connected {
        let bridge_for_init = bridge_state.clone();
        tokio::spawn(async move {
            let mut state = bridge_for_init.lock().await;
            let BridgeState {
                ref mut modhost,
                ref mut audio_engine,
                ..
            } = *state;
            if let Some(engine) = audio_engine
                && let Err(e) = engine.switch_snapshot_by_index(modhost, 0).await
            {
                tracing::warn!("Initial patch switch failed: {e}");
            }
        });
    }

    // 6. WebSocket channels.
    let (monitor_tx, _) = broadcast::channel::<Vec<u8>>(256);
    let (sysex_tx, _) = broadcast::channel::<Vec<u8>>(64);
    let (midi_out_tx, mut midi_out_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    let ws_state = Arc::new(WsState {
        midi_out_tx,
        monitor_tx: monitor_tx.clone(),
        sysex_tx: sysex_tx.clone(),
    });

    // Forward MIDI output channel to JACK.
    let jack_for_output = jack.clone();
    tokio::spawn(async move {
        while let Some(data) = midi_out_rx.recv().await {
            jack_for_output.send(&data);
        }
    });

    // 7. MIDI processing task.
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

            // Detect Program Change and CC for audio engine.
            if data.len() >= 2 {
                let mut i = 0;
                while i < data.len() {
                    let status = data[i];
                    if status & 0x80 == 0 {
                        i += 1;
                        continue;
                    }
                    let msg_type = status & 0xF0;
                    let channel = (status & 0x0F) + 1;
                    if msg_type == 0xC0 && i + 1 < data.len() {
                        // Program Change → switch snapshot.
                        let program = data[i + 1] as usize;
                        let bridge = bridge_for_midi.clone();
                        tokio::spawn(async move {
                            let mut state = bridge.lock().await;
                            if state.design_mode || !state.modhost.is_connected() {
                                return;
                            }
                            let BridgeState {
                                ref mut modhost,
                                ref mut audio_engine,
                                ..
                            } = *state;
                            if let Some(engine) = audio_engine.as_mut()
                                && let Err(e) =
                                    engine.switch_snapshot_by_index(modhost, program).await
                            {
                                error!("Patch switch failed: {e}");
                            }
                        });
                        i += 2;
                    } else if msg_type == 0xB0 && i + 2 < data.len() {
                        // Control Change → expression pedal routing.
                        let cc = data[i + 1];
                        let value = data[i + 2];
                        let bridge = bridge_for_midi.clone();
                        tokio::spawn(async move {
                            let mut state = bridge.lock().await;
                            if state.design_mode || !state.modhost.is_connected() {
                                return;
                            }
                            let BridgeState {
                                ref mut modhost,
                                ref mut audio_engine,
                                ..
                            } = *state;
                            if let Some(engine) = audio_engine.as_ref() {
                                let _ = engine.handle_cc(modhost, channel, cc, value).await;
                            }
                        });
                        i += 3;
                    } else {
                        i += 1;
                    }
                }
            }
        }
    });

    // 8. mod-host reconnect loop (background).
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
                let BridgeState {
                    ref mut modhost,
                    ref mut audio_engine,
                    ..
                } = *state;
                if let Some(engine) = audio_engine.as_mut() {
                    let _ = engine.switch_snapshot_by_index(modhost, 0).await;
                }
            }
        }
    });

    // 9. Build HTTP router and start listening.
    let app = Router::new()
        .route("/", get(handle_status))
        .route(
            "/raw",
            get(websocket::handle_raw).with_state(ws_state.clone()),
        )
        .route(
            "/monitor",
            get(websocket::handle_monitor).with_state(ws_state),
        )
        .route("/flash", post(flash::handle_flash))
        .route("/deploy", post(deploy::handle_deploy))
        .route("/mode", get(mode::handle_mode).post(mode::handle_mode))
        .with_state(bridge_state)
        .layer(tower_http::cors::CorsLayer::permissive());

    info!("pedalboard-bridge listening on {}", listen_addr);
    let listener = {
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )
        .expect("Failed to create socket");
        socket.set_reuse_address(true).expect("SO_REUSEADDR");
        let addr: std::net::SocketAddr = listen_addr.parse().expect("Invalid listen address");
        socket.bind(&addr.into()).expect("Failed to bind");
        socket.listen(128).expect("Failed to listen");
        socket.set_nonblocking(true).expect("set_nonblocking");
        tokio::net::TcpListener::from_std(socket.into()).expect("Failed to create tokio listener")
    };
    axum::serve(listener, app).await.unwrap();
}

async fn handle_status() -> String {
    format!(
        "pedalboard-bridge {}-{}\n",
        env!("CARGO_PKG_VERSION"),
        env!("GIT_HASH")
    )
}

fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}
