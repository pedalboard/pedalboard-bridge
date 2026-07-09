use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket},
        State, WebSocketUpgrade,
    },
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};
use tracing::info;

/// Shared state for WebSocket connections.
#[derive(Clone)]
pub struct WsState {
    /// Channel to send MIDI data out via JACK.
    pub midi_out_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Broadcast channel that streams all raw MIDI IN to monitor clients.
    pub monitor_tx: broadcast::Sender<Vec<u8>>,
    /// Channel to forward assembled SysEx messages to the active raw client.
    pub sysex_tx: broadcast::Sender<Vec<u8>>,
}

/// Handler for GET /raw — bidirectional SysEx passthrough.
pub async fn handle_raw(
    ws: WebSocketUpgrade,
    State(state): State<Arc<WsState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| raw_connection(socket, state))
}

async fn raw_connection(socket: WebSocket, state: Arc<WsState>) {
    info!("Raw WebSocket client connected");
    let (mut ws_tx, mut ws_rx) = socket.split();
    let mut sysex_rx = state.sysex_tx.subscribe();

    // Forward SysEx from JACK → WebSocket client.
    let tx_task = tokio::spawn(async move {
        while let Ok(data) = sysex_rx.recv().await {
            if ws_tx.send(Message::Binary(data.into())).await.is_err() {
                break;
            }
        }
    });

    // Forward WebSocket → JACK MIDI output.
    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Binary(data) => {
                info!("RAW OUT: {}", hex_encode(&data));
                let _ = state.midi_out_tx.send(data.to_vec());
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    tx_task.abort();
    info!("Raw WebSocket client disconnected");
}

/// Handler for GET /monitor — streams all raw MIDI IN to the client.
pub async fn handle_monitor(
    ws: WebSocketUpgrade,
    State(state): State<Arc<WsState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| monitor_connection(socket, state))
}

async fn monitor_connection(socket: WebSocket, state: Arc<WsState>) {
    info!("Monitor client connected");
    let (mut ws_tx, mut ws_rx) = socket.split();
    let mut monitor_rx = state.monitor_tx.subscribe();

    // Forward all MIDI IN → WebSocket client.
    let tx_task = tokio::spawn(async move {
        while let Ok(data) = monitor_rx.recv().await {
            if ws_tx.send(Message::Binary(data.into())).await.is_err() {
                break;
            }
        }
    });

    // Keep alive until client disconnects.
    while let Some(Ok(msg)) = ws_rx.next().await {
        if matches!(msg, Message::Close(_)) {
            break;
        }
    }

    tx_task.abort();
    info!("Monitor client disconnected");
}

fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}
