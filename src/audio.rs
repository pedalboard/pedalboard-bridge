use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::modhost::ModHostClient;

/// A plugin instance in the audio rig.
#[derive(Debug, Clone, Deserialize)]
pub struct AudioPlugin {
    /// mod-host instance ID.
    pub id: u32,
    /// LV2 plugin URI.
    pub uri: String,
    /// AIDA-X model file path (loaded via preset_load at boot).
    #[serde(default)]
    pub model: Option<String>,
}

/// State of a single plugin instance within a snapshot.
#[derive(Debug, Clone, Deserialize)]
pub struct AudioInstanceState {
    /// Whether this instance is bypassed. Default: false (active).
    #[serde(default)]
    pub bypassed: Option<bool>,
    /// Parameter values to set.
    #[serde(default)]
    pub params: HashMap<String, f64>,
}

/// A named audio snapshot — bypass states + params for all instances.
#[derive(Debug, Clone, Deserialize)]
pub struct AudioSnapshot {
    /// Snapshot name (referenced by presets/buttons).
    pub name: String,
    /// Per-instance state. Key is instance ID as string.
    pub state: HashMap<String, AudioInstanceState>,
    /// Per-snapshot expression pedal assignments.
    /// Key is pedal name ("Exp1", "Exp2"), value is the target parameter.
    #[serde(default)]
    pub expression: HashMap<String, ExpressionTarget>,
}

/// Top-level audio rig configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct AudioConfig {
    /// Plugin instances to load at boot.
    pub plugins: Vec<AudioPlugin>,
    /// JACK connections [source, destination].
    pub connections: Vec<[String; 2]>,
    /// Named snapshots for instant tone switching.
    pub snapshots: Vec<AudioSnapshot>,
    /// Physical pedal → CC mapping (which CC number each pedal sends).
    #[serde(default)]
    pub expression: Option<Vec<PedalMapping>>,
}

/// Maps a physical expression pedal to its MIDI CC number.
#[derive(Debug, Clone, Deserialize)]
pub struct PedalMapping {
    /// Pedal name (e.g., "Exp1"). Referenced in snapshot expression assignments.
    pub name: String,
    /// MIDI CC number this pedal sends.
    pub cc: u8,
    /// MIDI channel (1-16). If omitted, matches any channel.
    #[serde(default)]
    pub channel: Option<u8>,
}

/// A plugin parameter controlled by an expression pedal.
#[derive(Debug, Clone, Deserialize)]
pub struct ExpressionTarget {
    /// mod-host instance ID.
    pub instance: u32,
    /// Port symbol name.
    pub param: String,
    /// Minimum value (at CC=0). Default: 0.0.
    #[serde(default)]
    pub min: Option<f64>,
    /// Maximum value (at CC=127). Default: 1.0.
    #[serde(default)]
    pub max: Option<f64>,
}

/// Legacy format (audio-patches.json) for backward compatibility.
#[derive(Debug, Clone, Deserialize)]
pub struct LegacyAudioConfig {
    pub capture_port: String,
    pub playback_port: String,
    pub patches: Vec<LegacyPatch>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyPatch {
    pub name: String,
    pub plugins: Vec<LegacyPlugin>,
    #[serde(default)]
    pub params: Vec<LegacyParam>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyPlugin {
    pub uri: String,
    pub id: u32,
    #[serde(default)]
    pub input: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyParam {
    pub instance: u32,
    pub param: String,
    pub value: f64,
}

impl AudioConfig {
    /// Load audio configuration from a YAML or JSON file.
    /// Supports both new format (plugins + snapshots) and legacy (patches).
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let data = std::fs::read_to_string(path)?;

        // Try new YAML format first (may be embedded in setlist or standalone).
        if let Ok(config) = serde_yaml::from_str::<AudioConfig>(&data) {
            return Ok(config);
        }

        // Try as a setlist with audio section.
        #[derive(Deserialize)]
        struct SetlistWrapper {
            audio: Option<AudioConfig>,
        }
        if let Ok(wrapper) = serde_yaml::from_str::<SetlistWrapper>(&data)
            && let Some(config) = wrapper.audio
        {
            return Ok(config);
        }

        // Fall back to legacy JSON format (audio-patches.json).
        let legacy: LegacyAudioConfig = serde_json::from_str(&data)?;
        Ok(Self::from_legacy(legacy))
    }

    /// Convert legacy audio-patches.json to new format.
    fn from_legacy(legacy: LegacyAudioConfig) -> Self {
        // Convert patches to snapshots (each patch = remove all + reload, not ideal but compatible).
        let snapshots = legacy
            .patches
            .iter()
            .map(|patch| {
                let mut state = HashMap::new();
                for plugin in &patch.plugins {
                    let instance_state = AudioInstanceState {
                        bypassed: Some(false),
                        params: patch
                            .params
                            .iter()
                            .filter(|p| p.instance == plugin.id)
                            .map(|p| (p.param.clone(), p.value))
                            .collect(),
                    };
                    state.insert(plugin.id.to_string(), instance_state);
                }
                AudioSnapshot {
                    name: patch.name.clone(),
                    state,
                    expression: HashMap::new(),
                }
            })
            .collect();

        // Use plugins from the first patch as the base rig.
        let plugins = legacy
            .patches
            .first()
            .map(|p| {
                p.plugins
                    .iter()
                    .map(|lp| AudioPlugin {
                        id: lp.id,
                        uri: lp.uri.clone(),
                        model: None,
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Build connections from first patch.
        let connections = if let Some(patch) = legacy.patches.first() {
            let mut conns = Vec::new();
            if !patch.plugins.is_empty() {
                let first = &patch.plugins[0];
                let first_in = first
                    .input
                    .as_ref()
                    .map(|i| format!("effect_{}:{}", first.id, i))
                    .unwrap_or_else(|| format!("effect_{}:lv2_audio_in_1", first.id));
                conns.push([legacy.capture_port.clone(), first_in]);

                for i in 0..patch.plugins.len() - 1 {
                    let curr = &patch.plugins[i];
                    let next = &patch.plugins[i + 1];
                    let from = curr
                        .output
                        .as_ref()
                        .map(|o| format!("effect_{}:{}", curr.id, o))
                        .unwrap_or_else(|| format!("effect_{}:lv2_audio_out_1", curr.id));
                    let to = next
                        .input
                        .as_ref()
                        .map(|i| format!("effect_{}:{}", next.id, i))
                        .unwrap_or_else(|| format!("effect_{}:lv2_audio_in_1", next.id));
                    conns.push([from, to]);
                }

                let last = patch.plugins.last().unwrap();
                let last_out = last
                    .output
                    .as_ref()
                    .map(|o| format!("effect_{}:{}", last.id, o))
                    .unwrap_or_else(|| format!("effect_{}:lv2_audio_out_1", last.id));
                conns.push([last_out, legacy.playback_port.clone()]);
            }
            conns
        } else {
            Vec::new()
        };

        Self {
            plugins,
            connections,
            snapshots,
            expression: None,
        }
    }
}

/// Audio engine — manages the rig lifecycle and snapshot switching.
pub struct AudioEngine {
    pub config: AudioConfig,
    active_snapshot: Option<String>,
    rig_loaded: bool,
}

impl AudioEngine {
    pub fn new(config: AudioConfig) -> Self {
        Self {
            config,
            active_snapshot: None,
            rig_loaded: false,
        }
    }

    /// Load the full rig at boot: add all plugins, make connections, load models.
    pub async fn load_rig(
        &mut self,
        modhost: &mut ModHostClient,
    ) -> Result<(), crate::modhost::Error> {
        info!("Audio: loading rig ({} plugins)", self.config.plugins.len());

        // Remove any existing plugins.
        modhost.remove_all().await?;
        sleep(Duration::from_millis(100)).await;

        // Add all plugins.
        for plugin in &self.config.plugins {
            modhost.add_plugin(&plugin.uri, plugin.id).await?;
        }

        // Wait for plugins to register JACK ports.
        sleep(Duration::from_millis(200)).await;

        // Make all connections.
        for [from, to] in &self.config.connections {
            if let Err(e) = modhost.connect_ports(from, to).await {
                warn!("Audio: connection {} → {} failed: {}", from, to, e);
            }
        }

        // Load AIDA-X models via preset_load.
        for plugin in &self.config.plugins {
            if let Some(model) = &plugin.model {
                // Generate and register a preset bundle for this model.
                let bundle_path = format!("/tmp/pedalboard-model-{}.lv2", plugin.id);
                Self::create_model_bundle(&bundle_path, plugin.id, &plugin.uri, model)?;
                modhost.bundle_add(&bundle_path).await?;
                let preset_uri = format!("http://pedalboard.local/model#{}", plugin.id);
                modhost.preset_load(plugin.id, &preset_uri).await?;
                info!("Audio: loaded model for instance {}: {}", plugin.id, model);
            }
        }

        // Bypass all instances initially (snapshots will activate the right ones).
        for plugin in &self.config.plugins {
            modhost.bypass(plugin.id, true).await?;
        }

        self.rig_loaded = true;
        info!(
            "Audio: rig loaded ({} plugins, {} connections)",
            self.config.plugins.len(),
            self.config.connections.len()
        );
        Ok(())
    }

    /// Switch to a named snapshot (instant: bypass + param_set only).
    pub async fn switch_snapshot(
        &mut self,
        modhost: &mut ModHostClient,
        snapshot_name: &str,
    ) -> Result<(), crate::modhost::Error> {
        if !self.rig_loaded {
            self.load_rig(modhost).await?;
        }

        let snapshot = match self
            .config
            .snapshots
            .iter()
            .find(|s| s.name == snapshot_name)
        {
            Some(s) => s.clone(),
            None => {
                warn!("Audio: snapshot '{}' not found", snapshot_name);
                return Ok(());
            }
        };

        if self.active_snapshot.as_deref() == Some(snapshot_name) {
            return Ok(()); // Already active.
        }

        info!("Audio: switching to snapshot '{}'", snapshot_name);

        for (id_str, state) in &snapshot.state {
            let id: u32 = match id_str.parse() {
                Ok(id) => id,
                Err(_) => continue,
            };

            // Set bypass state.
            if let Some(bypassed) = state.bypassed {
                modhost.bypass(id, bypassed).await?;
            }

            // Set parameters.
            for (param, value) in &state.params {
                modhost.param_set(id, param, *value).await?;
            }
        }

        self.active_snapshot = Some(snapshot_name.to_string());
        info!("Audio: snapshot '{}' active", snapshot_name);
        Ok(())
    }

    /// Switch snapshot by index (for legacy Program Change → patch index mapping).
    pub async fn switch_snapshot_by_index(
        &mut self,
        modhost: &mut ModHostClient,
        index: usize,
    ) -> Result<(), crate::modhost::Error> {
        if index >= self.config.snapshots.len() {
            return Ok(());
        }
        let name = self.config.snapshots[index].name.clone();
        self.switch_snapshot(modhost, &name).await
    }

    /// Create a minimal LV2 preset bundle for loading an AIDA-X model.
    fn create_model_bundle(
        bundle_path: &str,
        instance_id: u32,
        plugin_uri: &str,
        model_path: &str,
    ) -> Result<(), crate::modhost::Error> {
        use std::fs;

        let _ = fs::create_dir_all(bundle_path);
        let preset_uri = format!("http://pedalboard.local/model#{instance_id}");

        let manifest = format!(
            r#"@prefix lv2: <http://lv2plug.in/ns/lv2core#> .
@prefix pset: <http://lv2plug.in/ns/ext/presets#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

<{preset_uri}>
    a pset:Preset ;
    lv2:appliesTo <{plugin_uri}> ;
    rdfs:seeAlso <presets.ttl> .
"#
        );

        let presets = format!(
            r#"@prefix lv2: <http://lv2plug.in/ns/lv2core#> .
@prefix pset: <http://lv2plug.in/ns/ext/presets#> .
@prefix state: <http://lv2plug.in/ns/ext/state#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

<{preset_uri}>
    a pset:Preset ;
    rdfs:label "model-{instance_id}" ;
    lv2:appliesTo <{plugin_uri}> ;
    state:state [
        <{plugin_uri}#json> <{model_path}>
    ] .
"#
        );

        fs::write(format!("{bundle_path}/manifest.ttl"), manifest)
            .map_err(crate::modhost::Error::Connect)?;
        fs::write(format!("{bundle_path}/presets.ttl"), presets)
            .map_err(crate::modhost::Error::Connect)?;

        Ok(())
    }

    /// Handle an incoming MIDI CC message. Routes to plugin parameters
    /// based on the active snapshot's expression assignments.
    pub async fn handle_cc(
        &self,
        modhost: &mut ModHostClient,
        _channel: u8,
        cc: u8,
        value: u8,
    ) -> Result<(), crate::modhost::Error> {
        // Find which pedal name this CC belongs to.
        let pedal_name = match &self.config.expression {
            Some(mappings) => mappings
                .iter()
                .find(|m| m.cc == cc)
                .map(|m| m.name.as_str()),
            None => None,
        };

        let pedal_name = match pedal_name {
            Some(name) => name,
            None => return Ok(()), // CC not mapped to any pedal.
        };

        // Find the active snapshot's expression assignment for this pedal.
        let snapshot_name = match &self.active_snapshot {
            Some(name) => name.clone(),
            None => return Ok(()),
        };

        let snapshot = match self
            .config
            .snapshots
            .iter()
            .find(|s| s.name == snapshot_name)
        {
            Some(s) => s,
            None => return Ok(()),
        };

        let target = match snapshot.expression.get(pedal_name) {
            Some(t) => t,
            None => return Ok(()), // Pedal not assigned in this snapshot.
        };

        // Scale CC (0-127) to param range (min-max).
        let normalized = value as f64 / 127.0;
        let min = target.min.unwrap_or(0.0);
        let max = target.max.unwrap_or(1.0);
        let param_value = min + normalized * (max - min);

        modhost
            .param_set(target.instance, &target.param, param_value)
            .await?;
        Ok(())
    }
}
