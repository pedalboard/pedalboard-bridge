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
	"sync"
	"time"

	"github.com/gorilla/websocket"
)

//go:embed ui/*
var uiFiles embed.FS

var version = "dev"

func main() {
	addr := flag.String("addr", ":8080", "listen address")
	midiConnect := flag.String("midi", "", "JACK MIDI port to auto-connect (e.g. 'a2j:OpenDeck')")
	showVersion := flag.Bool("version", false, "print version and exit")
	audioConfig := flag.String("audio", "", "audio patch config JSON file (enables mod-host integration)")
	modHostAddr := flag.String("modhost", "localhost:5555", "mod-host TCP address")
	flag.Parse()

	if *showVersion {
		fmt.Println(version)
		os.Exit(0)
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

	// Connect to JACK as a MIDI client
	jackMidi, err := NewJackMidi("pedalboard-bridge")
	if err != nil {
		log.Fatalf("JACK MIDI: %v", err)
	}
	defer jackMidi.Close()

	// Auto-connect to MIDI ports matching pattern (survives device reconnects)
	if *midiConnect != "" {
		jackMidi.AutoConnect(*midiConnect)
	}

	var clientMu sync.Mutex
	var activeConn *websocket.Conn
	var monitorConn *websocket.Conn
	var clientReady bool

	// MIDI processing goroutine — reads from JACK dataCh
	go func() {
		var sysex []byte
		for data := range jackMidi.DataCh() {
			for _, b := range data {
				if b == 0xF0 {
					sysex = []byte{b}
				} else if b == 0xF7 && sysex != nil {
					sysex = append(sysex, b)
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
						continue
					}
					msgType := status & 0xF0
					switch msgType {
					case 0xC0: // Program Change → switch audio patch
						if i+1 < len(data) {
							program := int(data[i+1])
							go audioEngine.SwitchPatch(program)
							i++
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

	// Send helper (writes MIDI back to controller via JACK output port)
	midiSend := func(data []byte) {
		jackMidi.Send(data)
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

	log.Printf("pedalboard-bridge %s listening on %s", version, *addr)
	log.Fatal(http.ListenAndServe(*addr, nil))
}
