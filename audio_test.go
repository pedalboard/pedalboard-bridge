package main

import (
	"os"
	"path/filepath"
	"testing"
)

func TestLoadAudioConfig_Valid(t *testing.T) {
	content := `{
  "capture_port": "system:capture_1",
  "playback_port": "system:playback_1",
  "patches": [
    {
      "name": "Clean",
      "plugins": [
        {"uri": "http://example.org/eq", "id": 0},
        {"uri": "http://example.org/reverb", "id": 1}
      ],
      "params": [
        {"instance": 0, "param": "gain", "value": 0.5}
      ]
    },
    {
      "name": "Crunch",
      "plugins": [
        {"uri": "http://example.org/amp", "id": 0, "input": "in_left", "output": "out_left"}
      ]
    }
  ]
}`
	tmpDir := t.TempDir()
	path := filepath.Join(tmpDir, "audio.json")
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}

	cfg, err := LoadAudioConfig(path)
	if err != nil {
		t.Fatalf("LoadAudioConfig: %v", err)
	}

	if cfg.CapturePort != "system:capture_1" {
		t.Errorf("CapturePort = %q, want %q", cfg.CapturePort, "system:capture_1")
	}
	if cfg.PlaybackPort != "system:playback_1" {
		t.Errorf("PlaybackPort = %q, want %q", cfg.PlaybackPort, "system:playback_1")
	}
	if len(cfg.Patches) != 2 {
		t.Fatalf("len(Patches) = %d, want 2", len(cfg.Patches))
	}

	// Verify first patch
	p := cfg.Patches[0]
	if p.Name != "Clean" {
		t.Errorf("Patch[0].Name = %q, want %q", p.Name, "Clean")
	}
	if len(p.Plugins) != 2 {
		t.Fatalf("Patch[0].Plugins len = %d, want 2", len(p.Plugins))
	}
	if p.Plugins[0].URI != "http://example.org/eq" {
		t.Errorf("Plugin[0].URI = %q", p.Plugins[0].URI)
	}
	if p.Plugins[0].ID != 0 {
		t.Errorf("Plugin[0].ID = %d, want 0", p.Plugins[0].ID)
	}
	if len(p.Params) != 1 {
		t.Fatalf("Patch[0].Params len = %d, want 1", len(p.Params))
	}
	if p.Params[0].Value != 0.5 {
		t.Errorf("Param[0].Value = %f, want 0.5", p.Params[0].Value)
	}

	// Verify second patch with custom input/output ports
	p2 := cfg.Patches[1]
	if p2.Plugins[0].inputPort() != "effect_0:in_left" {
		t.Errorf("inputPort = %q, want %q", p2.Plugins[0].inputPort(), "effect_0:in_left")
	}
	if p2.Plugins[0].outputPort() != "effect_0:out_left" {
		t.Errorf("outputPort = %q, want %q", p2.Plugins[0].outputPort(), "effect_0:out_left")
	}
}

func TestLoadAudioConfig_DefaultPorts(t *testing.T) {
	content := `{
  "capture_port": "system:capture_1",
  "playback_port": "system:playback_1",
  "patches": [
    {
      "name": "Test",
      "plugins": [{"uri": "http://example.org/plugin", "id": 5}]
    }
  ]
}`
	tmpDir := t.TempDir()
	path := filepath.Join(tmpDir, "audio.json")
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}

	cfg, err := LoadAudioConfig(path)
	if err != nil {
		t.Fatalf("LoadAudioConfig: %v", err)
	}

	plugin := cfg.Patches[0].Plugins[0]
	if plugin.inputPort() != "effect_5:lv2_audio_in_1" {
		t.Errorf("default inputPort = %q, want %q", plugin.inputPort(), "effect_5:lv2_audio_in_1")
	}
	if plugin.outputPort() != "effect_5:lv2_audio_out_1" {
		t.Errorf("default outputPort = %q, want %q", plugin.outputPort(), "effect_5:lv2_audio_out_1")
	}
}

func TestLoadAudioConfig_InvalidJSON(t *testing.T) {
	tmpDir := t.TempDir()
	path := filepath.Join(tmpDir, "bad.json")
	if err := os.WriteFile(path, []byte("{invalid json"), 0o644); err != nil {
		t.Fatal(err)
	}

	_, err := LoadAudioConfig(path)
	if err == nil {
		t.Error("expected error for invalid JSON, got nil")
	}
}

func TestLoadAudioConfig_FileNotFound(t *testing.T) {
	_, err := LoadAudioConfig("/nonexistent/path/audio.json")
	if err == nil {
		t.Error("expected error for missing file, got nil")
	}
}

func TestLoadAudioConfig_EmptyPatches(t *testing.T) {
	content := `{
  "capture_port": "system:capture_1",
  "playback_port": "system:playback_1",
  "patches": []
}`
	tmpDir := t.TempDir()
	path := filepath.Join(tmpDir, "audio.json")
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}

	cfg, err := LoadAudioConfig(path)
	if err != nil {
		t.Fatalf("LoadAudioConfig: %v", err)
	}
	if len(cfg.Patches) != 0 {
		t.Errorf("len(Patches) = %d, want 0", len(cfg.Patches))
	}
}
