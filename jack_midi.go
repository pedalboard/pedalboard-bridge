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
// Discovers the pedalboard device by looking up port aliases (stable across
// reconnects). Falls back to exclusion-based matching if alias lookup fails.
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

			// Discover ports by alias (stable names from JACK -X seq)
			capturePort, playbackPort := findPortsByAlias(lowerPattern)

			// Connect input (device capture → bridge midi_in)
			if capturePort != "" && capturePort != connectedIn {
				code := jm.client.Connect(capturePort, inTarget)
				if code == 0 {
					log.Printf("MIDI auto-connected: %s → %s", capturePort, inTarget)
					connectedIn = capturePort
				}
			}

			// Connect output (bridge midi_out → device playback)
			if playbackPort != "" && playbackPort != connectedOut {
				code := jm.client.Connect(outSource, playbackPort)
				if code == 0 {
					log.Printf("MIDI auto-connected: %s → %s", outSource, playbackPort)
					connectedOut = playbackPort
				}
			}

			// Check if connected ports still exist
			if connectedIn != "" {
				outPorts := jm.client.GetPorts("", jack.DEFAULT_MIDI_TYPE, jack.PortIsOutput)
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

			if connectedOut != "" {
				inPorts := jm.client.GetPorts("", jack.DEFAULT_MIDI_TYPE, jack.PortIsInput)
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

// findPortsByAlias parses `jack_lsp -A` output to find MIDI ports whose alias
// matches the pattern. Returns (capturePort, playbackPort) canonical names.
func findPortsByAlias(pattern string) (string, string) {
	out, err := exec.Command("jack_lsp", "-A", "-t").Output()
	if err != nil {
		return "", ""
	}

	var capturePort, playbackPort string
	lines := strings.Split(string(out), "\n")

	for i := 0; i < len(lines); i++ {
		portName := strings.TrimSpace(lines[i])
		if portName == "" || strings.HasPrefix(portName, "\t") || strings.HasPrefix(portName, " ") {
			continue
		}

		// Check if this is a MIDI port (look for type line)
		isMidi := false
		aliases := []string{}
		for j := i + 1; j < len(lines) && (strings.HasPrefix(lines[j], "   ") || strings.HasPrefix(lines[j], "\t")); j++ {
			trimmed := strings.TrimSpace(lines[j])
			if trimmed == "8 bit raw midi" {
				isMidi = true
			} else if trimmed != "" && !strings.Contains(trimmed, " bit ") {
				aliases = append(aliases, strings.ToLower(trimmed))
			}
		}

		if !isMidi {
			continue
		}

		// Check if any alias matches our pattern
		matched := false
		for _, alias := range aliases {
			if strings.Contains(alias, pattern) {
				matched = true
				break
			}
		}
		if !matched {
			continue
		}

		// Determine direction from port name
		if strings.Contains(portName, "capture") {
			capturePort = portName
		} else if strings.Contains(portName, "playback") {
			playbackPort = portName
		}
	}

	return capturePort, playbackPort
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
