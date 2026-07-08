package main

import (
	"embed"
	"encoding/hex"
	"flag"
	"fmt"
	"io/fs"
	"log"
	"net/http"
	"os"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/gorilla/websocket"
)

//go:embed ui/*
var uiFiles embed.FS

var version = "dev"

func main() {
	addr := flag.String("addr", ":8080", "listen address")
	port := flag.String("port", "", "MIDI port name (substring match in /proc/asound/cards)")
	showVersion := flag.Bool("version", false, "print version and exit")
	audioConfig := flag.String("audio", "", "audio patch config JSON file (enables mod-host integration)")
	modHostAddr := flag.String("modhost", "localhost:5555", "mod-host TCP address")
	flag.Parse()

	if *showVersion {
		fmt.Println(version)
		os.Exit(0)
	}

	if *port == "" {
		log.Fatal("Please specify -port flag")
	}

	// Audio engine (optional)
	var audioEngine *AudioEngine
	if *audioConfig != "" {
		cfg, err := LoadAudioConfig(*audioConfig)
		if err != nil {
			log.Printf("Audio config: %v (audio disabled)", err)
		} else {
			mh := NewModHost(*modHostAddr)
			if err := mh.Connect(); err != nil {
				log.Printf("mod-host: %v (audio disabled, will retry)", err)
				// Start retry goroutine
				// Design mode flag — suppresses mod-host auto-reconnect
				go func() {
					for {
						time.Sleep(5 * time.Second)
						designModeMu.Lock()
						inDesign := designMode
						designModeMu.Unlock()
						if inDesign {
							continue
						}
						if !mh.IsConnected() {
							if err := mh.Connect(); err == nil {
								log.Printf("mod-host reconnected")
								// Load initial patch
								if audioEngine != nil {
									audioEngine.SwitchPatch(0)
								}
							}
						}
					}
				}()
			}
			audioEngine = NewAudioEngine(mh, cfg)
			if mh.IsConnected() {
				go audioEngine.SwitchPatch(0)
			}
			log.Printf("Audio engine enabled: %d patches configured", len(cfg.Patches))
		}
	}

	var midi MidiPort
	var clientMu sync.Mutex
	var activeConn *websocket.Conn
	var monitorConn *websocket.Conn
	var clientReady bool

	// Connect to MIDI device
	connect := func() error {
		var device string
		if strings.HasPrefix(*port, "/") {
			// Direct path to device or FIFO (e.g. /tmp/midi-fifo)
			device = *port
		} else {
			// Search by name in /proc/asound/cards
			var err error
			device, err = findMidiDevice(*port)
			if err != nil {
				return err
			}
		}
		return midi.Open(device)
	}

	for {
		if err := connect(); err == nil {
			break
		}
		log.Printf("Cannot connect MIDI: waiting for device...")
		time.Sleep(2 * time.Second)
	}
	log.Printf("MIDI connected: %s", midi.device)

	// MIDI reader goroutine
	startReader := func() {
		// Blocking read runs in its own goroutine (parks an OS thread).
		// Data is sent via channel to the processing goroutine which
		// handles SysEx parsing and monitor forwarding.
		dataCh := make(chan []byte, 64)

		go func() {
			buf := make([]byte, 1024)
			for {
				midi.mu.Lock()
				fd := midi.inFd
				midi.mu.Unlock()
				if fd == 0 {
					time.Sleep(100 * time.Millisecond)
					continue
				}
				n, err := syscall.Read(fd, buf)
				if err != nil {
					log.Printf("MIDI read error: %v", err)
					midi.Close()
					time.Sleep(100 * time.Millisecond)
					continue
				}
				if n > 0 {
					data := make([]byte, n)
					copy(data, buf[:n])
					dataCh <- data
				}
			}
		}()

		go func() {
			var sysex []byte
			for data := range dataCh {
				for _, b := range data {
					if b == 0xF0 {
						sysex = []byte{b}
					} else if b == 0xF7 && sysex != nil {
						sysex = append(sysex, b)
						// Complete SysEx message
						log.Printf("MIDI IN:  %s", hex.EncodeToString(sysex))
						clientMu.Lock()
						conn := activeConn
						ready := clientReady
						clientMu.Unlock()
						if conn != nil && ready {
							conn.WriteMessage(websocket.BinaryMessage, sysex)
						}
						sysex = nil
					} else if sysex != nil {
						sysex = append(sysex, b)
					}
				}

				// Detect MIDI channel messages for audio engine
				if audioEngine != nil && len(data) >= 2 {
					for i := 0; i < len(data); i++ {
						status := data[i]
						if status&0x80 == 0 {
							continue // not a status byte
						}
						msgType := status & 0xF0
						switch msgType {
						case 0xC0: // Program Change → switch audio patch
							if i+1 < len(data) {
								program := int(data[i+1])
								go audioEngine.SwitchPatch(program)
								i++ // skip data byte
							}
						}
					}
				}

				// Forward raw bytes to monitor
				clientMu.Lock()
				mon := monitorConn
				clientMu.Unlock()
				if mon != nil {
					if err := mon.WriteMessage(websocket.BinaryMessage, data); err != nil {
						log.Printf("Monitor write error: %v", err)
					}
				}
			}
		}()
	}
	startReader()

	// Reconnect watchdog
	go func() {
		for {
			time.Sleep(2 * time.Second)
			if !midi.IsOpen() {
				log.Printf("MIDI disconnected, trying to reconnect...")
				for {
					if err := connect(); err == nil {
						break
					}
					time.Sleep(2 * time.Second)
				}
			} else {
				// Check if device file still exists (detects USB disconnect)
				midi.mu.Lock()
				dev := midi.device
				midi.mu.Unlock()
				if dev != "" {
					if _, err := os.Stat(dev); err != nil {
						log.Printf("MIDI device gone, closing...")
						midi.Close()
					}
				}
			}
		}
	}()

	// Send helper
	midiSend := func(data []byte) {
		if err := midi.Send(data); err != nil {
			log.Printf("MIDI send error: %v", err)
			midi.Close()
		}
	}

	// Serve embedded UI
	uiFS, _ := fs.Sub(uiFiles, "ui")
	fileServer := http.FileServer(http.FS(uiFS))
	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/" || r.URL.Path == "/index.html" {
			indexData, err := uiFiles.ReadFile("ui/index.html")
			if err != nil {
				http.Error(w, "not found", 404)
				return
			}
			autoConnect := `<script>
localStorage.setItem("opendeck-webconfig-address",location.host);
if(!location.hash.includes("/device/")){location.hash="#/device/__webconfig__"+encodeURIComponent(location.host)}
</script>`
			w.Header().Set("Content-Type", "text/html")
			fmt.Fprint(w, autoConnect)
			w.Write(indexData)
			return
		}
		fileServer.ServeHTTP(w, r)
	})

	// Register handlers
	http.HandleFunc("/config", handleConfig(&clientMu, &activeConn, &clientReady, midiSend))
	http.HandleFunc("/raw", handleRaw(&clientMu, &activeConn, &clientReady, midiSend))
	http.HandleFunc("/monitor", handleMonitor(&clientMu, &monitorConn))
	http.HandleFunc("/flash", handleFlash())
	http.HandleFunc("/mode", handleMode(audioEngine))

	log.Printf("pedalboard-bridge %s listening on %s (MIDI: %s)", version, *addr, *port)
	log.Fatal(http.ListenAndServe(*addr, nil))
}
