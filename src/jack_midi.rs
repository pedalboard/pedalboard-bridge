use std::sync::Arc;

use jack::{Client, ClientOptions, MidiIn, MidiOut, Port, PortFlags, PortId, ProcessScope};
use tokio::sync::{Notify, mpsc};
use tracing::{info, warn};

/// Messages received from the JACK MIDI input port.
pub type MidiMessage = Vec<u8>;

const MIDI_TYPE: &str = "8 bit raw midi";

/// JACK MIDI client with event-driven auto-connect.
pub struct JackMidi {
    async_client: jack::AsyncClient<PortNotifier, JackProcess>,
    out_tx: mpsc::UnboundedSender<MidiMessage>,
    client_name: String,
    /// Fires when JACK ports are registered/unregistered.
    port_change: Arc<Notify>,
}

/// Notification handler — signals when ports change.
pub struct PortNotifier {
    port_change: Arc<Notify>,
}

impl jack::NotificationHandler for PortNotifier {
    fn port_registration(&mut self, _: &Client, _port_id: PortId, _is_registered: bool) {
        self.port_change.notify_one();
    }
}

/// Process handler — reads/writes MIDI in the realtime callback.
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

        let port_change = Arc::new(Notify::new());

        let notifier = PortNotifier {
            port_change: port_change.clone(),
        };

        let process = JackProcess {
            in_port,
            out_port,
            data_tx,
            out_rx: std::sync::Mutex::new(out_rx),
        };

        let async_client = client.activate_async(notifier, process)?;

        let actual_name = async_client.as_client().name().to_string();
        info!(
            "JACK MIDI client '{}' active (ports: midi_in, midi_out)",
            actual_name
        );

        Ok((
            Self {
                async_client,
                out_tx,
                client_name: actual_name,
                port_change,
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

    /// Get the client name (as assigned by JACK).
    pub fn name(&self) -> &str {
        &self.client_name
    }

    /// Find MIDI ports matching a pattern in their aliases.
    /// Returns (capture_port_name, playback_port_name).
    fn find_ports_by_alias(&self, pattern: &str) -> (String, String) {
        let client = self.async_client.as_client();
        let lower_pattern = pattern.to_lowercase();

        let mut capture_port = String::new();
        let mut playback_port = String::new();

        // MIDI output ports = device capture (we receive from these).
        for port_name in client.ports(None, Some(MIDI_TYPE), PortFlags::IS_OUTPUT) {
            if let Some(port) = client.port_by_name(&port_name)
                && let Ok(aliases) = port.aliases()
                && aliases
                    .iter()
                    .any(|a| a.to_lowercase().contains(&lower_pattern))
            {
                capture_port = port_name;
                break;
            }
        }

        // MIDI input ports = device playback (we send to these).
        for port_name in client.ports(None, Some(MIDI_TYPE), PortFlags::IS_INPUT) {
            if let Some(port) = client.port_by_name(&port_name)
                && let Ok(aliases) = port.aliases()
                && aliases
                    .iter()
                    .any(|a| a.to_lowercase().contains(&lower_pattern))
            {
                playback_port = port_name;
                break;
            }
        }

        (capture_port, playback_port)
    }

    /// Check if a port still exists.
    fn port_exists(&self, port_name: &str) -> bool {
        self.async_client
            .as_client()
            .port_by_name(port_name)
            .is_some()
    }

    /// Connect two ports by name.
    fn connect(&self, from: &str, to: &str) -> bool {
        self.async_client
            .as_client()
            .connect_ports_by_name(from, to)
            .is_ok()
    }

    /// Start event-driven auto-connect.
    ///
    /// Uses JACK's port registration callback for instant detection when the
    /// device appears/disappears. Falls back to a 10s poll as safety net.
    pub fn auto_connect(self: &Arc<Self>, pattern: String) {
        let jack = Arc::clone(self);
        tokio::spawn(async move {
            let in_target = format!("{}:midi_in", jack.client_name);
            let out_source = format!("{}:midi_out", jack.client_name);

            let mut connected_in = String::new();
            let mut connected_out = String::new();

            // Do an initial check immediately.
            jack.port_change.notify_one();

            loop {
                // Wait for port change event OR timeout (safety net).
                tokio::select! {
                    _ = jack.port_change.notified() => {}
                    _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {}
                }

                // Small delay to let JACK finish registering all ports for a device.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;

                let (capture, playback) = jack.find_ports_by_alias(&pattern);

                // Connect input (device capture → bridge midi_in).
                if !capture.is_empty()
                    && capture != connected_in
                    && jack.connect(&capture, &in_target)
                {
                    info!("MIDI auto-connected: {} → {}", capture, in_target);
                    connected_in = capture.clone();
                }

                // Connect output (bridge midi_out → device playback).
                if !playback.is_empty()
                    && playback != connected_out
                    && jack.connect(&out_source, &playback)
                {
                    info!("MIDI auto-connected: {} → {}", out_source, playback);
                    connected_out = playback.clone();
                }

                // Check if connected ports disappeared.
                if !connected_in.is_empty() && !jack.port_exists(&connected_in) {
                    info!(
                        "MIDI input disconnected: {} (waiting for reconnect...)",
                        connected_in
                    );
                    connected_in.clear();
                }
                if !connected_out.is_empty() && !jack.port_exists(&connected_out) {
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
