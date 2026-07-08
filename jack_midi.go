package main

import (
	"fmt"
	"log"
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
	return jm, nil
}

// DataCh returns the channel that receives MIDI messages from the input port.
func (jm *JackMidi) DataCh() <-chan []byte {
	return jm.dataCh
}

// Send writes a MIDI message to the output port on the next process cycle.
func (jm *JackMidi) Send(data []byte) {
	// Output is handled via a pending buffer written in process().
	// For now the bridge only reads MIDI — output support can be added later.
	_ = data
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

// AutoConnect watches for JACK MIDI ports matching the pattern and connects them.
// Runs in a goroutine, retries every 2 seconds. Matching is case-insensitive.
func (jm *JackMidi) AutoConnect(pattern string) {
	target := jm.client.GetName() + ":midi_in"
	lowerPattern := strings.ToLower(pattern)
	go func() {
		connected := ""
		for {
			time.Sleep(2 * time.Second)
			if jm.client == nil {
				return
			}
			// Get all MIDI output ports, filter by pattern (case-insensitive)
			ports := jm.client.GetPorts("", jack.DEFAULT_MIDI_TYPE, jack.PortIsOutput)
			ownPrefix := strings.ToLower(jm.client.GetName() + ":")
			for _, port := range ports {
				lowerPort := strings.ToLower(port)
				if strings.HasPrefix(lowerPort, ownPrefix) {
					continue // skip own ports
				}
				if !strings.Contains(lowerPort, lowerPattern) {
					continue
				}
				if port == connected {
					continue
				}
				code := jm.client.Connect(port, target)
				if code == 0 {
					log.Printf("MIDI auto-connected: %s → %s", port, target)
					connected = port
				}
			}
			// Check if connected port still exists
			if connected != "" {
				found := false
				for _, port := range ports {
					if port == connected {
						found = true
						break
					}
				}
				if !found {
					log.Printf("MIDI disconnected: %s (waiting for reconnect...)", connected)
					connected = ""
				}
			}
		}
	}()
}

// process is the JACK realtime callback.
func (jm *JackMidi) process(nframes uint32) int {
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
	return 0
}
