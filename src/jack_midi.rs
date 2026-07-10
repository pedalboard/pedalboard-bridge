use std::process::Command;

use jack::{Client, ClientOptions, MidiIn, MidiOut, Port, ProcessScope};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Messages received from the JACK MIDI input port.
pub type MidiMessage = Vec<u8>;

/// JACK MIDI client with auto-connect by alias.
pub struct JackMidi {
    _async_client: jack::AsyncClient<(), JackProcess>,
    out_tx: mpsc::UnboundedSender<MidiMessage>,
    client_name: String,
}

/// Channels for communicating with the JACK process callback.
struct JackProcess {
    in_port: Port<MidiIn>,
    out_port: Port<MidiOut>,
    data_tx: mpsc::UnboundedSender<MidiMessage>,
    out_rx: std::sync::Mutex<mpsc::UnboundedReceiver<MidiMessage>>,
}

impl jack::ProcessHandler for JackProcess {
    fn process(&mut self, _client: &Client, scope: &ProcessScope) -> jack::Control {
        // Read MIDI input events.
        for event in self.in_port.iter(scope) {
            let data = event.bytes.to_vec();
            let _ = self.data_tx.send(data);
        }

        // Write pending MIDI output events.
        let mut out_writer = self.out_port.writer(scope);
        if let Ok(ref mut rx) = self.out_rx.lock() {
            while let Ok(msg) = rx.try_recv() {
                let raw = jack::RawMidi {
                    time: 0,
                    bytes: &msg,
                };
                if out_writer.write(&raw).is_err() {
                    // Buffer full — drop message.
                }
            }
        }

        jack::Control::Continue
    }
}

impl JackMidi {
    /// Create and activate a JACK MIDI client.
    /// Returns the client and a receiver for incoming MIDI messages.
    pub fn new(
        client_name: &str,
    ) -> Result<(Self, mpsc::UnboundedReceiver<MidiMessage>), jack::Error> {
        let (client, _status) = Client::new(client_name, ClientOptions::default())?;

        let in_port = client.register_port("midi_in", MidiIn::default())?;
        let out_port = client.register_port("midi_out", MidiOut::default())?;

        let (data_tx, data_rx) = mpsc::unbounded_channel();
        let (out_tx, out_rx) = mpsc::unbounded_channel();

        let process = JackProcess {
            in_port,
            out_port,
            data_tx,
            out_rx: std::sync::Mutex::new(out_rx),
        };

        let async_client = client.activate_async((), process)?;

        let actual_name = async_client.as_client().name().to_string();
        info!(
            "JACK MIDI client '{}' active (ports: midi_in, midi_out)",
            actual_name
        );

        Ok((
            Self {
                _async_client: async_client,
                out_tx,
                client_name: actual_name,
            },
            data_rx,
        ))
    }

    /// Queue a MIDI message for output on the next process cycle.
    pub fn send(&self, data: &[u8]) {
        if self.out_tx.send(data.to_vec()).is_err() {
            warn!("JACK MIDI output channel closed");
        }
    }

    /// Start auto-connect loop: polls for MIDI ports matching `pattern` in aliases.
    /// Spawns a tokio task that reconnects on device reboot.
    pub fn auto_connect(&self, pattern: String) {
        let client_name = self.client_name.clone();
        tokio::spawn(async move {
            let in_target = format!("{client_name}:midi_in");
            let out_source = format!("{client_name}:midi_out");
            let lower_pattern = pattern.to_lowercase();

            let mut connected_in = String::new();
            let mut connected_out = String::new();

            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                let (capture, playback) = find_ports_by_alias(&lower_pattern);

                // Connect input (device capture → bridge midi_in).
                if !capture.is_empty()
                    && capture != connected_in
                    && jack_connect(&capture, &in_target)
                {
                    info!("MIDI auto-connected: {} → {}", capture, in_target);
                    connected_in = capture.clone();
                }

                // Connect output (bridge midi_out → device playback).
                if !playback.is_empty()
                    && playback != connected_out
                    && jack_connect(&out_source, &playback)
                {
                    info!("MIDI auto-connected: {} → {}", out_source, playback);
                    connected_out = playback.clone();
                }

                // Check if connected ports still exist.
                if !connected_in.is_empty() && !port_exists(&connected_in) {
                    info!(
                        "MIDI input disconnected: {} (waiting for reconnect...)",
                        connected_in
                    );
                    connected_in.clear();
                }
                if !connected_out.is_empty() && !port_exists(&connected_out) {
                    info!(
                        "MIDI output disconnected: {} (waiting for reconnect...)",
                        connected_out
                    );
                    connected_out.clear();
                }
            }
        });
    }
}

/// Find MIDI ports by alias using `jack_lsp -A -t`.
fn find_ports_by_alias(pattern: &str) -> (String, String) {
    let output = match Command::new("jack_lsp").args(["-A", "-t"]).output() {
        Ok(o) => o,
        Err(_) => return (String::new(), String::new()),
    };

    let text = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = text.lines().collect();

    let mut capture_port = String::new();
    let mut playback_port = String::new();

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        // Canonical port name: no leading whitespace.
        if line.is_empty() || line.starts_with(' ') || line.starts_with('\t') {
            i += 1;
            continue;
        }
        let port_name = line.trim().to_string();
        i += 1;

        // Collect aliases and type.
        let mut is_midi = false;
        let mut aliases = Vec::new();
        while i < lines.len() && (lines[i].starts_with(' ') || lines[i].starts_with('\t')) {
            let trimmed = lines[i].trim();
            if lines[i].starts_with('\t') {
                if trimmed == "8 bit raw midi" {
                    is_midi = true;
                }
            } else {
                aliases.push(trimmed.to_lowercase());
            }
            i += 1;
        }

        if !is_midi {
            continue;
        }

        let matched = aliases.iter().any(|a| a.contains(pattern));
        if !matched {
            continue;
        }

        if port_name.contains("capture") {
            capture_port = port_name;
        } else if port_name.contains("playback") {
            playback_port = port_name;
        }
    }

    (capture_port, playback_port)
}

/// Connect two JACK ports via `jack_connect` command.
fn jack_connect(from: &str, to: &str) -> bool {
    Command::new("jack_connect")
        .args([from, to])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Check if a JACK port exists via `jack_lsp`.
fn port_exists(port_name: &str) -> bool {
    Command::new("jack_lsp")
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.trim() == port_name)
        })
        .unwrap_or(false)
}
