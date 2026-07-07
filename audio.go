package main

import (
	"encoding/json"
	"fmt"
	"log"
	"os"
)

// AudioPatch defines the audio plugin chain for a single preset.
type AudioPatch struct {
	Name    string        `json:"name"`
	Plugins []PluginSlot  `json:"plugins"`
	Params  []ParamValue  `json:"params,omitempty"`
}

// PluginSlot represents a plugin in the chain.
type PluginSlot struct {
	URI    string `json:"uri"`
	ID     int    `json:"id"`
	Input  string `json:"input,omitempty"`  // audio input port name (default: "lv2_audio_in_1")
	Output string `json:"output,omitempty"` // audio output port name (default: "lv2_audio_out_1")
}

func (p PluginSlot) inputPort() string {
	if p.Input != "" {
		return fmt.Sprintf("effect_%d:%s", p.ID, p.Input)
	}
	return fmt.Sprintf("effect_%d:lv2_audio_in_1", p.ID)
}

func (p PluginSlot) outputPort() string {
	if p.Output != "" {
		return fmt.Sprintf("effect_%d:%s", p.ID, p.Output)
	}
	return fmt.Sprintf("effect_%d:lv2_audio_out_1", p.ID)
}

// ParamValue sets a parameter on a plugin instance.
type ParamValue struct {
	Instance int     `json:"instance"`
	Param    string  `json:"param"`
	Value    float64 `json:"value"`
}

// AudioConfig holds the audio patch definitions for all presets.
type AudioConfig struct {
	CapturePort  string       `json:"capture_port"`
	PlaybackPort string       `json:"playback_port"`
	Patches      []AudioPatch `json:"patches"`
}

// LoadAudioConfig reads the audio configuration from a JSON file.
func LoadAudioConfig(path string) (*AudioConfig, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read audio config: %w", err)
	}
	var cfg AudioConfig
	if err := json.Unmarshal(data, &cfg); err != nil {
		return nil, fmt.Errorf("parse audio config: %w", err)
	}
	return &cfg, nil
}

// AudioEngine manages audio patch switching via mod-host.
type AudioEngine struct {
	modhost      *ModHost
	config       *AudioConfig
	activePatch  int
}

// NewAudioEngine creates the audio engine with mod-host connection.
func NewAudioEngine(modhost *ModHost, config *AudioConfig) *AudioEngine {
	return &AudioEngine{
		modhost:     modhost,
		config:      config,
		activePatch: -1,
	}
}

// SwitchPatch switches to the audio patch for the given preset index.
// Removes the current chain and loads the new one.
func (e *AudioEngine) SwitchPatch(presetIdx int) error {
	if e.config == nil || !e.modhost.IsConnected() {
		return nil // audio not configured or not connected
	}
	if presetIdx >= len(e.config.Patches) {
		return nil // no audio patch for this preset
	}
	if presetIdx == e.activePatch {
		return nil // already active
	}

	patch := e.config.Patches[presetIdx]
	log.Printf("Audio: switching to patch %d (%s)", presetIdx, patch.Name)

	// Remove all current plugins
	e.modhost.RemoveAll()

	// Load new plugins
	for _, plugin := range patch.Plugins {
		if err := e.modhost.Add(plugin.URI, plugin.ID); err != nil {
			log.Printf("Audio: failed to add plugin %s: %v", plugin.URI, err)
			return err
		}
	}

	// Connect the chain: capture → plugin 0 → plugin 1 → ... → playback
	if len(patch.Plugins) > 0 {
		// Connect capture to first plugin
		e.modhost.ConnectPorts(e.config.CapturePort, patch.Plugins[0].inputPort())

		// Connect plugins in series
		for i := 0; i < len(patch.Plugins)-1; i++ {
			e.modhost.ConnectPorts(patch.Plugins[i].outputPort(), patch.Plugins[i+1].inputPort())
		}

		// Connect last plugin to playback
		e.modhost.ConnectPorts(patch.Plugins[len(patch.Plugins)-1].outputPort(), e.config.PlaybackPort)
	}

	// Set parameters
	for _, p := range patch.Params {
		e.modhost.ParamSet(p.Instance, p.Param, p.Value)
	}

	e.activePatch = presetIdx
	log.Printf("Audio: patch %d active (%d plugins)", presetIdx, len(patch.Plugins))
	return nil
}

// SetParam sets a parameter on the active patch.
func (e *AudioEngine) SetParam(instanceID int, param string, value float64) {
	if e.modhost.IsConnected() {
		e.modhost.ParamSet(instanceID, param, value)
	}
}
