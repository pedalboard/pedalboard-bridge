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

impl AudioConfig {
    /// Load audio configuration from a YAML file.
    /// Supports both standalone audio config and setlist with embedded audio section.
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let data = std::fs::read_to_string(path)?;

        // Try as standalone audio config.
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

        Err(format!("Failed to parse audio config from {}", path.display()).into())
    }

    /// Load audio configuration from a YAML string.
    pub fn load_from_str(data: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // Try as standalone audio config.
        if let Ok(config) = serde_yaml::from_str::<AudioConfig>(data) {
            return Ok(config);
        }

        // Try as a setlist with audio section.
        #[derive(Deserialize)]
        struct SetlistWrapper {
            audio: Option<AudioConfig>,
        }
        if let Ok(wrapper) = serde_yaml::from_str::<SetlistWrapper>(data)
            && let Some(config) = wrapper.audio
        {
            return Ok(config);
        }

        Err("Failed to parse audio config from string".into())
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

    /// Get the name of the currently active snapshot (if any).
    pub fn active_snapshot_name(&self) -> Option<&str> {
        self.active_snapshot.as_deref()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> AudioConfig {
        serde_yaml::from_str(
            r#"
plugins:
  - id: 0
    uri: "http://example.com/distortion"
  - id: 1
    uri: "http://example.com/amp-clean"
    model: "/models/clean.json"
  - id: 2
    uri: "http://example.com/amp-crunch"
    model: "/models/crunch.json"
  - id: 3
    uri: "http://example.com/reverb"
connections:
  - ["system:capture_2", "effect_0:in"]
  - ["effect_0:out", "effect_1:in"]
  - ["effect_0:out", "effect_2:in"]
  - ["effect_1:out", "effect_3:in"]
  - ["effect_2:out", "effect_3:in"]
  - ["effect_3:out", "system:playback_2"]
expression:
  - { name: "Exp1", cc: 7 }
  - { name: "Exp2", cc: 11 }
snapshots:
  - name: "Clean"
    state:
      "0": { bypassed: true }
      "1": { bypassed: false, params: { PREGAIN: 0.3, MASTER: 0.8 } }
      "2": { bypassed: true }
      "3": { bypassed: false }
    expression:
      Exp1: { instance: 1, param: "MASTER", min: 0.0, max: 1.0 }
      Exp2: { instance: 3, param: "decay_time", min: 0.5, max: 5.0 }
  - name: "Crunch"
    state:
      "0": { bypassed: false, params: { DRIVE: 0.6 } }
      "1": { bypassed: true }
      "2": { bypassed: false, params: { PREGAIN: 0.7, MASTER: 0.6 } }
      "3": { bypassed: true }
    expression:
      Exp1: { instance: 2, param: "PREGAIN", min: 0.3, max: 1.0 }
      Exp2: { instance: 0, param: "DRIVE", min: 0.0, max: 1.0 }
"#,
        )
        .unwrap()
    }

    #[test]
    fn parse_yaml_config() {
        let config = sample_config();
        assert_eq!(config.plugins.len(), 4);
        assert_eq!(config.connections.len(), 6);
        assert_eq!(config.snapshots.len(), 2);
        assert_eq!(config.expression.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn parse_plugin_fields() {
        let config = sample_config();
        assert_eq!(config.plugins[0].id, 0);
        assert_eq!(config.plugins[0].uri, "http://example.com/distortion");
        assert!(config.plugins[0].model.is_none());
        assert_eq!(
            config.plugins[1].model.as_deref(),
            Some("/models/clean.json")
        );
    }

    #[test]
    fn parse_snapshot_state() {
        let config = sample_config();
        let clean = &config.snapshots[0];
        assert_eq!(clean.name, "Clean");

        let inst0 = &clean.state["0"];
        assert_eq!(inst0.bypassed, Some(true));
        assert!(inst0.params.is_empty());

        let inst1 = &clean.state["1"];
        assert_eq!(inst1.bypassed, Some(false));
        assert_eq!(inst1.params["PREGAIN"], 0.3);
        assert_eq!(inst1.params["MASTER"], 0.8);
    }

    #[test]
    fn parse_per_snapshot_expression() {
        let config = sample_config();

        // Clean: Exp1 → instance 1 MASTER
        let clean = &config.snapshots[0];
        let exp1 = &clean.expression["Exp1"];
        assert_eq!(exp1.instance, 1);
        assert_eq!(exp1.param, "MASTER");
        assert_eq!(exp1.min, Some(0.0));
        assert_eq!(exp1.max, Some(1.0));

        // Crunch: Exp1 → instance 2 PREGAIN (different target!)
        let crunch = &config.snapshots[1];
        let exp1 = &crunch.expression["Exp1"];
        assert_eq!(exp1.instance, 2);
        assert_eq!(exp1.param, "PREGAIN");
        assert_eq!(exp1.min, Some(0.3));
        assert_eq!(exp1.max, Some(1.0));
    }

    #[test]
    fn parse_pedal_mapping() {
        let config = sample_config();
        let mappings = config.expression.unwrap();
        assert_eq!(mappings[0].name, "Exp1");
        assert_eq!(mappings[0].cc, 7);
        assert_eq!(mappings[1].name, "Exp2");
        assert_eq!(mappings[1].cc, 11);
    }

    #[test]
    fn parse_setlist_with_audio() {
        let yaml = r#"
audio:
  plugins:
    - id: 0
      uri: "http://example.com/amp"
  connections:
    - ["system:capture_2", "effect_0:in"]
  snapshots:
    - name: "Default"
      state:
        "0": { params: { GAIN: 0.5 } }

presets:
  - name: "Song 1"
    audio_snapshot: "Default"
    buttons:
      A: { label: "Test", cc: 80, color: green }
"#;

        // Should parse as setlist wrapper.
        #[derive(Deserialize)]
        struct SetlistWrapper {
            audio: Option<AudioConfig>,
        }
        let wrapper: SetlistWrapper = serde_yaml::from_str(yaml).unwrap();
        let config = wrapper.audio.unwrap();
        assert_eq!(config.plugins.len(), 1);
        assert_eq!(config.snapshots[0].name, "Default");
    }

    #[test]
    fn audio_engine_new() {
        let config = sample_config();
        let engine = AudioEngine::new(config);
        assert!(!engine.rig_loaded);
        assert!(engine.active_snapshot.is_none());
    }

    #[test]
    fn model_bundle_generation() {
        let bundle_path = "/tmp/pedalboard-test-bundle.lv2";
        let _ = std::fs::remove_dir_all(bundle_path);

        AudioEngine::create_model_bundle(
            bundle_path,
            5,
            "http://aidadsp.cc/plugins/aidadsp-bundle/rt-neural-loader",
            "/etc/pedalboard/models/test.json",
        )
        .unwrap();

        let manifest = std::fs::read_to_string(format!("{bundle_path}/manifest.ttl")).unwrap();
        assert!(manifest.contains("http://pedalboard.local/model#5"));
        assert!(manifest.contains("pset:Preset"));
        assert!(manifest.contains("rt-neural-loader"));

        let presets = std::fs::read_to_string(format!("{bundle_path}/presets.ttl")).unwrap();
        assert!(presets.contains("/etc/pedalboard/models/test.json"));
        assert!(presets.contains("rt-neural-loader#json"));

        // Cleanup.
        let _ = std::fs::remove_dir_all(bundle_path);
    }

    #[test]
    fn expression_cc_scaling() {
        // Test the scaling math: CC 0-127 → min-max range.
        let min = 0.5_f64;
        let max = 2.0_f64;

        // CC 0 → min
        let normalized = 0.0 / 127.0;
        let value = min + normalized * (max - min);
        assert!((value - 0.5).abs() < 0.001);

        // CC 127 → max
        let normalized = 127.0 / 127.0;
        let value = min + normalized * (max - min);
        assert!((value - 2.0).abs() < 0.001);

        // CC 64 → midpoint
        let normalized = 64.0 / 127.0;
        let value = min + normalized * (max - min);
        assert!((value - 1.256).abs() < 0.01);
    }
}
