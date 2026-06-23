use futures_util::{SinkExt, StreamExt};
use midi2::prelude::*;
use opendeck::config::{Config, FirmwareVersion};
use opendeck::parser::OpenDeckParser;
use opendeck::{SystemHandler, ValueSize};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

struct StubHandler;

impl SystemHandler for StubHandler {
    fn reboot(&self) {
        println!("Reboot requested");
    }
    fn bootloader(&self) {
        println!("Bootloader requested");
    }
    fn factory_reset(&self) {
        println!("Factory reset requested");
    }
}

type StubConfig = Config<32, 10, 2, 2, 10, StubHandler>;

#[tokio::main]
async fn main() {
    let addr = "127.0.0.1:9000";
    let listener = TcpListener::bind(addr).await.expect("Failed to bind");
    println!("OpenDeck stub listening on ws://{}/config", addr);

    let mut config: StubConfig = Config::new(
        FirmwareVersion {
            major: 1,
            minor: 0,
            revision: 0,
        },
        0x123456,
        StubHandler,
    );

    // Process connections (single client, like the real bridge)
    while let Ok((stream, peer)) = listener.accept().await {
        println!("Client connected: {}", peer);
        handle_connection(stream, &mut config).await;
        println!("Client disconnected: {}", peer);
    }
}

async fn handle_connection(stream: tokio::net::TcpStream, config: &mut StubConfig) {
    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("WebSocket handshake error: {}", e);
            return;
        }
    };

    let (mut tx, mut rx) = ws_stream.split();
    let mut buf = [0u8; 256];

    while let Some(msg) = rx.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(_) => break,
        };

        if let Message::Binary(data) = msg {
            // Decode and print request
            let parser = OpenDeckParser::new(ValueSize::TwoBytes);
            let is_config = if let Ok(req) = parser.parse(&data) {
                match &req {
                    opendeck::OpenDeckRequest::Configuration(..) => {
                        println!("← {:?}", req);
                        true
                    }
                    _ => false,
                }
            } else {
                false
            };
            let mut responses = config.process_sysex(&data);
            while let Ok(Some(response)) = responses.next(&mut buf, config) {
                let response_bytes = response.data().to_vec();
                if is_config {
                    let parser = OpenDeckParser::new(ValueSize::TwoBytes);
                    if let Ok(resp) = parser.parse(&response_bytes) {
                        println!("  → {:?}", resp);
                    } else {
                        println!("  → {:02X?}", &response_bytes);
                    }
                }
                if tx.send(Message::Binary(response_bytes)).await.is_err() {
                    return;
                }
            }
        }
    }
}
