use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Errors returned by the mod-host client.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("connection failed: {0}")]
    Connect(#[from] std::io::Error),
    #[error("mod-host error {code}: {message}")]
    ModHost { code: i32, message: String },
    #[error("timeout waiting for response")]
    Timeout,
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    #[error("not connected")]
    NotConnected,
}

/// A TCP client for the mod-host LV2 plugin host.
///
/// mod-host protocol:
/// - Commands are newline-terminated UTF-8 strings.
/// - Responses are null-byte terminated: `resp <status> [value]\0`
/// - Only one client connection at a time.
pub struct ModHostClient {
    stream: Option<TcpStream>,
    timeout: Duration,
}

/// Parsed response from mod-host.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: i32,
    pub value: Option<String>,
}

impl ModHostClient {
    /// Connect to mod-host at the given address (e.g., "localhost:5555").
    pub async fn connect(addr: &str) -> Result<Self, Error> {
        let stream = TcpStream::connect(addr).await?;
        Ok(Self {
            stream: Some(stream),
            timeout: Duration::from_secs(30),
        })
    }

    /// Create a disconnected client (for design mode).
    pub fn disconnected() -> Self {
        Self {
            stream: None,
            timeout: Duration::from_secs(30),
        }
    }

    /// Returns true if this client has an active connection.
    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    /// Set the response timeout (default: 30s, needed for heavy plugin loads).
    pub fn set_timeout(&mut self, duration: Duration) {
        self.timeout = duration;
    }

    /// Send a raw command and return the parsed response.
    pub async fn send(&mut self, command: &str) -> Result<Response, Error> {
        let stream = self.stream.as_mut().ok_or(Error::NotConnected)?;

        stream
            .write_all(format!("{command}\n").as_bytes())
            .await?;

        let mut buf = Vec::with_capacity(256);
        let result = timeout(self.timeout, async {
            loop {
                let mut chunk = [0u8; 4096];
                let n = stream.read(&mut chunk).await?;
                if n == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "connection closed",
                    ));
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.contains(&0) {
                    break;
                }
            }
            Ok(())
        })
        .await;

        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(Error::Connect(e)),
            Err(_) => return Err(Error::Timeout),
        }

        // Strip null terminator and parse.
        let raw = String::from_utf8_lossy(&buf)
            .trim_end_matches('\0')
            .trim()
            .to_string();

        Self::parse_response(&raw)
    }

    fn parse_response(raw: &str) -> Result<Response, Error> {
        let raw = raw.trim();
        if !raw.starts_with("resp ") {
            return Err(Error::InvalidResponse(raw.to_string()));
        }

        let rest = &raw[5..];
        let mut parts = rest.splitn(2, ' ');

        let status: i32 = parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| Error::InvalidResponse(raw.to_string()))?;

        let value = parts.next().map(|s| s.to_string());

        if status < 0 {
            return Err(Error::ModHost {
                code: status,
                message: error_message(status).to_string(),
            });
        }

        Ok(Response { status, value })
    }

    // --- High-level commands ---

    /// Add an LV2 plugin instance.
    pub async fn add_plugin(&mut self, uri: &str, instance_id: u32) -> Result<(), Error> {
        self.send(&format!("add {uri} {instance_id}")).await?;
        Ok(())
    }

    /// Remove a plugin instance.
    pub async fn remove_plugin(&mut self, instance_id: u32) -> Result<(), Error> {
        self.send(&format!("remove {instance_id}")).await?;
        Ok(())
    }

    /// Remove all plugin instances.
    /// Note: mod-host may not respond when no plugins exist, so we use a short timeout.
    pub async fn remove_all(&mut self) -> Result<(), Error> {
        let stream = self.stream.as_mut().ok_or(Error::NotConnected)?;
        stream.write_all(b"remove -1\n").await?;
        // Don't wait for response — mod-host doesn't reliably respond to remove -1.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // Drain any response that may have arrived.
        let mut buf = [0u8; 256];
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            stream.read(&mut buf),
        )
        .await;
        Ok(())
    }

    /// Load a preset by URI onto a plugin instance.
    pub async fn preset_load(&mut self, instance_id: u32, preset_uri: &str) -> Result<(), Error> {
        self.send(&format!("preset_load {instance_id} {preset_uri}"))
            .await?;
        Ok(())
    }

    /// Set a parameter value on a plugin instance.
    pub async fn param_set(
        &mut self,
        instance_id: u32,
        symbol: &str,
        value: f64,
    ) -> Result<(), Error> {
        self.send(&format!("param_set {instance_id} {symbol} {value}"))
            .await?;
        Ok(())
    }

    /// Get a parameter value from a plugin instance.
    pub async fn param_get(
        &mut self,
        instance_id: u32,
        symbol: &str,
    ) -> Result<f64, Error> {
        let resp = self
            .send(&format!("param_get {instance_id} {symbol}"))
            .await?;
        resp.value
            .as_deref()
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| Error::InvalidResponse("missing value".into()))
    }

    /// Toggle bypass on a plugin instance.
    pub async fn bypass(&mut self, instance_id: u32, bypassed: bool) -> Result<(), Error> {
        let val = if bypassed { 1 } else { 0 };
        self.send(&format!("bypass {instance_id} {val}")).await?;
        Ok(())
    }

    /// Connect two JACK ports.
    pub async fn connect_ports(&mut self, from: &str, to: &str) -> Result<(), Error> {
        self.send(&format!("connect {from} {to}")).await?;
        Ok(())
    }

    /// Disconnect two JACK ports.
    pub async fn disconnect_ports(&mut self, from: &str, to: &str) -> Result<(), Error> {
        self.send(&format!("disconnect {from} {to}")).await?;
        Ok(())
    }

    /// Register an LV2 bundle (makes presets available for preset_load).
    pub async fn bundle_add(&mut self, path: &str) -> Result<(), Error> {
        self.send(&format!("bundle_add {path}")).await?;
        Ok(())
    }

    /// Unregister an LV2 bundle.
    pub async fn bundle_remove(&mut self, path: &str) -> Result<(), Error> {
        self.send(&format!("bundle_remove {path} \"\"")).await?;
        Ok(())
    }

    /// Get current JACK CPU load.
    pub async fn cpu_load(&mut self) -> Result<f64, Error> {
        let resp = self.send("cpu_load").await?;
        resp.value
            .as_deref()
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| Error::InvalidResponse("missing cpu_load value".into()))
    }
}

fn error_message(code: i32) -> &'static str {
    match code {
        -1 => "instance invalid",
        -2 => "instance already exists",
        -3 => "instance does not exist",
        -4 => "instance unlicensed",
        -101 => "invalid LV2 URI",
        -102 => "LV2 instantiation failed",
        -103 => "invalid parameter symbol",
        -104 => "invalid preset URI",
        -105 => "can't load state",
        -201 => "JACK client creation failed",
        -202 => "JACK client activation failed",
        -203 => "JACK client deactivation failed",
        -204 => "JACK port register failed",
        -205 => "JACK port connection failed",
        -206 => "JACK port disconnection failed",
        -207 => "JACK value out of range",
        -901 => "memory allocation failed",
        -902 => "invalid operation",
        _ => "unknown error",
    }
}
