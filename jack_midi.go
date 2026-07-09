package main

import (
	"fmt"
	"log"
	"os/exec"
	"strings"
	"time"

	jack "github.com/xthexder/go-jack"
)

// JackMidi manages JACK MIDI input/output for the bridge.
type JackMidi struct {
	client  *jack.Client
	inPort  *jack.Port
	outPort *jack.Port
	dataCh  chan []byte
	outCh   chan []byte
	errCh   chan string
}

// NewJackMidi creates and activates a JACK client with MIDI ports.
func NewJackMidi(clientName string) (*JackMidi, error) {
	client, status := jack.ClientOpen(clientName, jack.NoStartServer)
	if client == nil {
		return nil, fmt.Errorf("JACK client_open failed: %s", jack.StrError(status))
	}

	jm := &JackMidi{
		client: client,
		dataCh: make(chan []byte, 256),
		outCh:  make(chan []byte, 256),
		errCh:  make(chan string, 16),
	}

	// Register MIDI input port (receives from RP2040 / simulator)
	jm.inPort = client.PortRegister("midi_in", jack.DEFAULT_MIDI_TYPE, jack.PortIsInput, 0)
	if jm.inPort == nil {
		client.Close()
		return nil, fmt.Errorf("JACK: failed to register midi_in port")
	}

	// Register MIDI output port (sends SysEx responses to controller)
	jm.outPort = client.PortRegister("midi_out", jack.DEFAULT_MIDI_TYPE, jack.PortIsOutput, 0)
	if jm.outPort == nil {
		client.Close()
		return nil, fmt.Errorf("JACK: failed to register midi_out port")
	}

	// Set the process callback
	if code := client.SetProcessCallback(jm.process); code != 0 {
		client.Close()
		return nil, fmt.Errorf("JACK: set_process_callback failed: %s", jack.StrError(code))
	}

	// Handle JACK shutdown
	client.OnShutdown(func() {
		log.Printf("JACK server shut down")
		close(jm.dataCh)
	})

	// Activate
	if code := client.Activate(); code != 0 {
		client.Close()
		return nil, fmt.Errorf("JACK: activate failed: %s", jack.StrError(code))
	}

	log.Printf("JACK MIDI client '%s' active (ports: midi_in, midi_out)", clientName)
	// Drain write errors from realtime callback
	go func() {
		for msg := range jm.errCh {
			log.Print(msg)
		}
	}()
	return jm, nil
}

// DataCh returns the channel that receives MIDI messages from the input port.
func (jm *JackMidi) DataCh() <-chan []byte {
	return jm.dataCh
}

// Send queues a MIDI message for output on the next process cycle.
func (jm *JackMidi) Send(data []byte) {
	msg := make([]byte, len(data))
	copy(msg, data)
	select {
	case jm.outCh <- msg:
	default:
		log.Printf("JACK MIDI output buffer full, dropping message (%d bytes)", len(data))
	}
}

// Close deactivates and closes the JACK client.
func (jm *JackMidi) Close() {
	if jm.client != nil {
		jm.client.Close()
		jm.client = nil
	}
}

// Connect connects the bridge's MIDI input to a source port.
func (jm *JackMidi) Connect(sourcePort string) error {
	code := jm.client.Connect(sourcePort, jm.client.GetName()+":midi_in")
	if code != 0 {
		return fmt.Errorf("JACK connect %s → midi_in: %s", sourcePort, jack.StrError(code))
	}
	return nil
}

// AutoConnect watches for JACK MIDI ports and connects them to the bridge.
// Runs in a goroutine, polls every 2 seconds. Connects to any MIDI port
// that isn't our own and isn't the Midi-Through loopback device.
// If a pattern is provided, it's used as an additional filter (case-insensitive
// substring match on the canonical port name).
func (jm *JackMidi) AutoConnect(pattern string) {
	inTarget := jm.client.GetName() + ":midi_in"
	outSource := jm.client.GetName() + ":midi_out"
	lowerPattern := strings.ToLower(pattern)
	go func() {
		connectedIn := ""
		connectedOut := ""
		for {
			time.Sleep(2 * time.Second)
			if jm.client == nil {
				return
			}
			ownPrefix := strings.ToLower(jm.client.GetName() + ":")

			// Connect input: find MIDI output ports (capture) → our midi_in
			outPorts := jm.client.GetPorts("", jack.DEFAULT_MIDI_TYPE, jack.PortIsOutput)
			for _, port := range outPorts {
				lowerPort := strings.ToLower(port)
				if strings.HasPrefix(lowerPort, ownPrefix) {
					continue
				}
				if isExcludedPort(lowerPort) {
					continue
				}
				if lowerPattern != "" && lowerPattern != "auto" && !strings.Contains(lowerPort, lowerPattern) {
					continue
				}
				if port == connectedIn {
					continue
				}
				code := jm.client.Connect(port, inTarget)
				if code == 0 {
					log.Printf("MIDI auto-connected: %s → %s", port, inTarget)
					connectedIn = port
					setPortAlias(port, "pedalboard:capture")
				}
			}

			// Connect output: find MIDI input ports (playback) → our midi_out
			inPorts := jm.client.GetPorts("", jack.DEFAULT_MIDI_TYPE, jack.PortIsInput)
			for _, port := range inPorts {
				lowerPort := strings.ToLower(port)
				if strings.HasPrefix(lowerPort, ownPrefix) {
					continue
				}
				if isExcludedPort(lowerPort) {
					continue
				}
				if lowerPattern != "" && lowerPattern != "auto" && !strings.Contains(lowerPort, lowerPattern) {
					continue
				}
				if port == connectedOut {
					continue
				}
				code := jm.client.Connect(outSource, port)
				if code == 0 {
					log.Printf("MIDI auto-connected: %s → %s", outSource, port)
					connectedOut = port
					setPortAlias(port, "pedalboard:playback")
				}
			}

			// Check if connected input port still exists
			if connectedIn != "" {
				found := false
				for _, port := range outPorts {
					if port == connectedIn {
						found = true
						break
					}
				}
				if !found {
					log.Printf("MIDI input disconnected: %s (waiting for reconnect...)", connectedIn)
					connectedIn = ""
				}
			}

			// Check if connected output port still exists
			if connectedOut != "" {
				found := false
				for _, port := range inPorts {
					if port == connectedOut {
						found = true
						break
					}
				}
				if !found {
					log.Printf("MIDI output disconnected: %s (waiting for reconnect...)", connectedOut)
					connectedOut = ""
				}
			}
		}
	}()
}

// setPortAlias assigns a well-known alias to a JACK port so that other clients
// (effects, scripts) can connect to a stable name regardless of port renumbering.
func setPortAlias(port, alias string) {
	if err := exec.Command("jack_alias", port, alias).Run(); err != nil {
		log.Printf("jack_alias %s %s: %v", port, alias, err)
	}
}

// isExcludedPort returns true for ports that should never be auto-connected
// (loopback devices, mod-host internal ports, etc.)
func isExcludedPort(lowerPort string) bool {
	excluded := []string{
		"midi-through",
		"midi_through",
		"midi through",
		"midithru",
		"mod-host",
		"mod-monitor",
		"effect_",
	}
	for _, ex := range excluded {
		if strings.Contains(lowerPort, ex) {
			return true
		}
	}
	// Exclude system:midi_capture_1 / system:midi_playback_1 (Midi-Through is always port 1)
	if strings.HasSuffix(lowerPort, "_1") && strings.Contains(lowerPort, "system:midi") {
		return true
	}
	return false
}

// process is the JACK realtime callback.
func (jm *JackMidi) process(nframes uint32) int {
	// --- Input: read MIDI events from device ---
	events := jm.inPort.GetMidiEvents(nframes)
	for _, event := range events {
		if len(event.Buffer) == 0 {
			continue
		}
		// Copy buffer (data is only valid within this callback)
		data := make([]byte, len(event.Buffer))
		copy(data, event.Buffer)

		// Non-blocking send
		select {
		case jm.dataCh <- data:
		default:
		}
	}

	// --- Output: write pending MIDI messages to device ---
	buffer := jm.outPort.MidiClearBuffer(nframes)
	for {
		select {
		case msg := <-jm.outCh:
			event := &jack.MidiData{
				Time:   0,
				Buffer: msg,
			}
			if ret := jm.outPort.MidiEventWrite(event, buffer); ret != 0 {
				select {
				case jm.errCh <- fmt.Sprintf("JACK MIDI write failed: ret=%d, msg_len=%d, nframes=%d", ret, len(msg), nframes):
				default:
				}
			}
		default:
			return 0
		}
	}
}
