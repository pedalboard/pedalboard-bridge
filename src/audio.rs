use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use tokio::time::sleep;
use tracing::info;

use crate::modhost::ModHostClient;

/// A plugin instance in the audio chain.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginSlot {
    pub uri: String,
    pub id: u32,
    #[serde(default)]
    pub input: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
}

impl PluginSlot {
    pub fn input_port(&self) -> String {
        match &self.input {
            Some(name) => format!("effect_{}:{}", self.id, name),
            None => format!("effect_{}:lv2_audio_in_1", self.id),
        }
    }

    pub fn output_port(&self) -> String {
        match &self.output {
            Some(name) => format!("effect_{}:{}", self.id, name),
            None => format!("effect_{}:lv2_audio_out_1", self.id),
        }
    }
}

/// Parameter value to set on a plugin instance.
#[derive(Debug, Clone, Deserialize)]
pub struct ParamValue {
    pub instance: u32,
    pub param: String,
    pub value: f64,
}

/// Audio patch definition — the plugin chain for a single preset.
#[derive(Debug, Clone, Deserialize)]
pub struct AudioPatch {
    pub name: String,
    pub plugins: Vec<PluginSlot>,
    #[serde(default)]
    pub params: Vec<ParamValue>,
}

/// Top-level audio configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct AudioConfig {
    pub capture_port: String,
    pub playback_port: String,
    pub patches: Vec<AudioPatch>,
}

impl AudioConfig {
    /// Load audio configuration from a JSON file.
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let data = std::fs::read_to_string(path)?;
        let config: AudioConfig = serde_json::from_str(&data)?;
        Ok(config)
    }
}

/// Audio engine — manages patch switching via mod-host.
pub struct AudioEngine {
    pub config: AudioConfig,
    active_patch: i32,
}

impl AudioEngine {
    pub fn new(config: AudioConfig) -> Self {
        Self {
            config,
            active_patch: -1,
        }
    }

    /// Switch to the audio patch for the given preset index.
    pub async fn switch_patch(
        &mut self,
        modhost: &mut ModHostClient,
        preset_idx: usize,
    ) -> Result<(), crate::modhost::Error> {
        if preset_idx >= self.config.patches.len() {
            return Ok(()); // No patch for this preset.
        }
        if preset_idx as i32 == self.active_patch {
            return Ok(()); // Already active.
        }

        let patch = &self.config.patches[preset_idx];
        info!("Audio: switching to patch {} ({})", preset_idx, patch.name);

        // Remove all current plugins.
        modhost.remove_all().await?;
        sleep(Duration::from_millis(100)).await;

        // Load new plugins.
        for plugin in &patch.plugins {
            modhost.add_plugin(&plugin.uri, plugin.id).await?;
        }

        // Connect the chain: capture → plugin 0 → plugin 1 → ... → playback.
        if !patch.plugins.is_empty() {
            // Capture → first plugin.
            modhost
                .connect_ports(&self.config.capture_port, &patch.plugins[0].input_port())
                .await?;

            // Chain plugins in series.
            for i in 0..patch.plugins.len() - 1 {
                modhost
                    .connect_ports(
                        &patch.plugins[i].output_port(),
                        &patch.plugins[i + 1].input_port(),
                    )
                    .await?;
            }

            // Last plugin → playback.
            modhost
                .connect_ports(
                    &patch.plugins.last().unwrap().output_port(),
                    &self.config.playback_port,
                )
                .await?;
        }

        // Set parameters.
        for p in &patch.params {
            modhost.param_set(p.instance, &p.param, p.value).await?;
        }

        self.active_patch = preset_idx as i32;
        info!(
            "Audio: patch {} active ({} plugins)",
            preset_idx,
            patch.plugins.len()
        );
        Ok(())
    }
}
