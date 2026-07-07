package main

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
)

func findMidiDevice(portName string) (string, error) {
	return findMidiDeviceFrom("/proc/asound/cards", portName)
}

func findMidiDeviceFrom(cardsPath, portName string) (string, error) {
	data, err := os.ReadFile(cardsPath)
	if err != nil {
		return "", err
	}
	for _, line := range strings.Split(string(data), "\n") {
		if !strings.Contains(line, portName) {
			continue
		}
		// Extract card number from line like " 2 [OpenDeck   ..."
		var cardNum int
		if _, err := fmt.Sscanf(strings.TrimSpace(line), "%d", &cardNum); err != nil {
			continue
		}
		pattern := fmt.Sprintf("/dev/snd/midiC%dD*", cardNum)
		matches, _ := filepath.Glob(pattern)
		if len(matches) > 0 {
			return matches[0], nil
		}
	}
	return "", fmt.Errorf("MIDI device %q not found", portName)
}

type MidiPort struct {
	mu     sync.Mutex
	in     *os.File
	inFd   int
	out    *os.File
	device string
}

func (m *MidiPort) Open(device string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.Close_locked()
	out, err := os.OpenFile(device, os.O_WRONLY, 0)
	if err != nil {
		return fmt.Errorf("open out: %w", err)
	}
	// Use syscall.Open for input to keep the fd in blocking mode.
	// Go's os.OpenFile sets fds to non-blocking which breaks ALSA rawmidi reads.
	inFd, err := syscall.Open(device, syscall.O_RDONLY, 0)
	if err != nil {
		out.Close()
		return fmt.Errorf("open in: %w", err)
	}
	m.out = out
	m.inFd = inFd
	m.in = os.NewFile(uintptr(inFd), device+"-in")
	m.device = device
	return nil
}

func (m *MidiPort) Close_locked() {
	if m.out != nil {
		m.out.Close()
		m.out = nil
	}
	if m.inFd > 0 {
		syscall.Close(m.inFd)
		m.inFd = 0
		m.in = nil
	}
}

func (m *MidiPort) Close() {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.Close_locked()
}

func (m *MidiPort) Send(data []byte) error {
	m.mu.Lock()
	out := m.out
	m.mu.Unlock()
	if out == nil {
		return fmt.Errorf("not connected")
	}
	_, err := out.Write(data)
	return err
}

func (m *MidiPort) IsOpen() bool {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.in != nil
}
