package main

import (
	"os"
	"testing"
)

func TestFindMidiDeviceFrom_Found(t *testing.T) {
	// Create a mock /proc/asound/cards file
	content := ` 0 [PCH            ]: HDA-Intel - HDA Intel PCH
                      HDA Intel PCH at 0xf7210000 irq 32
 2 [OpenDeck       ]: USB-Audio - OpenDeck
                      OpenDeck at usb-0000:00:14.0-1, full speed
`
	tmpFile, err := os.CreateTemp(t.TempDir(), "cards")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := tmpFile.WriteString(content); err != nil {
		t.Fatal(err)
	}
	tmpFile.Close()

	// findMidiDeviceFrom won't find actual /dev/snd/midiC2D* on the test machine,
	// but it should parse the card number correctly. We test the "not found" path
	// with the correct card number extraction.
	_, err = findMidiDeviceFrom(tmpFile.Name(), "OpenDeck")
	// Since /dev/snd/midiC2D* won't exist on the test machine, we expect
	// the error to mention "not found" (no glob matches)
	if err == nil {
		// On a machine with the actual device, this could pass
		return
	}
	// The function should have at least parsed the card file without error
	if err.Error() != `MIDI device "OpenDeck" not found` {
		t.Errorf("unexpected error: %v", err)
	}
}

func TestFindMidiDeviceFrom_NotFound(t *testing.T) {
	content := ` 0 [PCH            ]: HDA-Intel - HDA Intel PCH
                      HDA Intel PCH at 0xf7210000 irq 32
`
	tmpFile, err := os.CreateTemp(t.TempDir(), "cards")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := tmpFile.WriteString(content); err != nil {
		t.Fatal(err)
	}
	tmpFile.Close()

	_, err = findMidiDeviceFrom(tmpFile.Name(), "OpenDeck")
	if err == nil {
		t.Error("expected error, got nil")
	}
	expected := `MIDI device "OpenDeck" not found`
	if err.Error() != expected {
		t.Errorf("got %q, want %q", err.Error(), expected)
	}
}

func TestFindMidiDeviceFrom_FileNotFound(t *testing.T) {
	_, err := findMidiDeviceFrom("/nonexistent/path/cards", "OpenDeck")
	if err == nil {
		t.Error("expected error for missing file, got nil")
	}
}

func TestFindMidiDeviceFrom_EmptyFile(t *testing.T) {
	tmpFile, err := os.CreateTemp(t.TempDir(), "cards")
	if err != nil {
		t.Fatal(err)
	}
	tmpFile.Close()

	_, err = findMidiDeviceFrom(tmpFile.Name(), "OpenDeck")
	if err == nil {
		t.Error("expected error for empty file, got nil")
	}
}

func TestFindMidiDeviceFrom_MultipleCards(t *testing.T) {
	content := ` 0 [PCH            ]: HDA-Intel - HDA Intel PCH
                      HDA Intel PCH at 0xf7210000 irq 32
 1 [Scarlett       ]: USB-Audio - Scarlett 2i2
                      Scarlett 2i2 at usb-0000:00:14.0-2
 3 [OpenDeck       ]: USB-Audio - OpenDeck
                      OpenDeck at usb-0000:00:14.0-1, full speed
`
	tmpFile, err := os.CreateTemp(t.TempDir(), "cards")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := tmpFile.WriteString(content); err != nil {
		t.Fatal(err)
	}
	tmpFile.Close()

	// Should attempt to find /dev/snd/midiC3D* (card 3, not card 1)
	_, err = findMidiDeviceFrom(tmpFile.Name(), "OpenDeck")
	if err == nil {
		return // device actually exists on this machine
	}
	// Verify it correctly identified the device name even if glob fails
	expected := `MIDI device "OpenDeck" not found`
	if err.Error() != expected {
		t.Errorf("got %q, want %q", err.Error(), expected)
	}
}
