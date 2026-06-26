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
	"gitlab.com/gomidi/midi/v2"
	_ "gitlab.com/gomidi/midi/v2/drivers/rtmididrv"
)

//go:embed ui/*
var uiFiles embed.FS

var upgrader = websocket.Upgrader{
	CheckOrigin: func(r *http.Request) bool { return true },
}

func main() {
	addr := flag.String("addr", ":8080", "listen address")
	port := flag.String("port", "", "MIDI port name (substring match)")
	uf2Mount := flag.String("uf2-mount", "/media/laenzi/RPI-RP2", "UF2 mount point for RP2040 bootloader")
	flag.Parse()

	if *port == "" {
		log.Println("Available MIDI ports:")
		for _, p := range midi.GetOutPorts() {
			log.Printf("  OUT: %s", p.String())
		}
		for _, p := range midi.GetInPorts() {
			log.Printf("  IN:  %s", p.String())
		}
		log.Fatal("Please specify -port flag")
	}

	// Global MIDI listener - fan out to active WebSocket client
	var clientMu sync.Mutex
	var activeConn *websocket.Conn
	var clientReady bool

	var midiMu sync.Mutex
	var send func(midi.Message) error
	var stopListener func()

	midiListener := func(msg midi.Message, timestampms int32) {
		clientMu.Lock()
		conn := activeConn
		ready := clientReady
		clientMu.Unlock()
		if conn == nil || !ready {
			return
		}
		raw := msg.Bytes()
		log.Printf("MIDI IN:  %s", hex.EncodeToString(raw))
		if len(raw) > 0 && raw[0] != 0xF0 {
			framed := make([]byte, len(raw)+2)
			framed[0] = 0xF0
			copy(framed[1:], raw)
			framed[len(framed)-1] = 0xF7
			conn.WriteMessage(websocket.BinaryMessage, framed)
		} else {
			conn.WriteMessage(websocket.BinaryMessage, raw)
		}
	}

	connectMidi := func() error {
		midiMu.Lock()
		defer midiMu.Unlock()
		if stopListener != nil {
			stopListener()
			stopListener = nil
		}
		outPort, err := midi.FindOutPort(*port)
		if err != nil {
			return fmt.Errorf("find out port: %w", err)
		}
		inPort, err := midi.FindInPort(*port)
		if err != nil {
			return fmt.Errorf("find in port: %w", err)
		}
		s, err := midi.SendTo(outPort)
		if err != nil {
			return fmt.Errorf("open out: %w", err)
		}
		send = s
		stop, err := midi.ListenTo(inPort, midiListener, midi.UseSysEx(), midi.SysExBufferSize(1024))
		if err != nil {
			return fmt.Errorf("listen: %w", err)
		}
		stopListener = stop
		log.Printf("MIDI connected: %s", outPort.String())
		return nil
	}

	if err := connectMidi(); err != nil {
		log.Fatalf("Cannot connect MIDI: %v", err)
	}
	defer func() {
		midiMu.Lock()
		if stopListener != nil {
			stopListener()
		}
		midiMu.Unlock()
	}()

	midiSend := func(msg midi.Message) {
		midiMu.Lock()
		s := send
		midiMu.Unlock()
		if s != nil {
			s(msg)
		}
	}

	// Serve embedded UI with auto-connect injection
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

	// WebSocket MIDI bridge
	http.HandleFunc("/config", func(w http.ResponseWriter, r *http.Request) {
		conn, err := upgrader.Upgrade(w, r, nil)
		if err != nil {
			log.Printf("WebSocket upgrade error: %v", err)
			return
		}
		log.Printf("WebSocket client connected")

		// Set this connection as the active receiver
		clientMu.Lock()
		activeConn = conn
		clientReady = false
		clientMu.Unlock()

		defer func() {
			clientMu.Lock()
			if activeConn == conn {
				activeConn = nil
			}
			clientMu.Unlock()
			midiSend(midi.SysEx([]byte{0x00, 0x53, 0x43, 0x00, 0x00, 0x00}))
			conn.Close()
		}()

		// WebSocket → MIDI Out
		for {
			_, data, err := conn.ReadMessage()
			if err != nil {
				break
			}
			log.Printf("MIDI OUT: %s", hex.EncodeToString(data))
			clientMu.Lock()
			clientReady = true
			clientMu.Unlock()
			if len(data) >= 2 && data[0] == 0xF0 && data[len(data)-1] == 0xF7 {
				inner := data[1 : len(data)-1]
				midiSend(midi.SysEx(inner))
			} else {
				midiSend(data)
			}
		}
	})

	// WebSocket DFU handler - implements OpenDeck Network DFU protocol
	http.HandleFunc("/dfu", func(w http.ResponseWriter, r *http.Request) {
		conn, err := upgrader.Upgrade(w, r, nil)
		if err != nil {
			log.Printf("DFU WebSocket upgrade error: %v", err)
			return
		}
		defer conn.Close()
		log.Printf("DFU client connected")

		const (
			cmdBegin  = 0x01
			cmdChunk  = 0x02
			cmdFinish = 0x03
			cmdAbort  = 0x04
			ack       = 0x81
			statusOk  = 0x00
			statusErr = 0x01
		)

		sendAck := func(cmd byte, status byte, bytesWritten uint32) {
			resp := []byte{
				ack, cmd, status,
				byte(bytesWritten), byte(bytesWritten >> 8),
				byte(bytesWritten >> 16), byte(bytesWritten >> 24),
			}
			conn.WriteMessage(websocket.BinaryMessage, resp)
		}

		var firmware []byte
		var bytesReceived uint32

		for {
			_, data, err := conn.ReadMessage()
			if err != nil {
				log.Printf("DFU connection closed: %v", err)
				return
			}
			if len(data) == 0 {
				continue
			}

			switch data[0] {
			case cmdBegin:
				log.Printf("DFU: BEGIN - entering bootloader")
				firmware = nil
				bytesReceived = 0

				// Check if already in bootloader mode (mount exists)
				alreadyMounted := false
				if _, err := os.Stat(*uf2Mount + "/INFO_UF2.TXT"); err == nil {
					alreadyMounted = true
				}

				if !alreadyMounted {
					// Send bootloader SysEx: handshake then bootloader command
					midiSend(midi.SysEx([]byte{0x00, 0x53, 0x43, 0x00, 0x00, 0x01}))
					<-time.After(500 * time.Millisecond)
					midiSend(midi.SysEx([]byte{0x00, 0x53, 0x43, 0x00, 0x00, 0x55}))

					// Wait for UF2 mount
					for i := 0; i < 30; i++ {
						if _, err := os.Stat(*uf2Mount + "/INFO_UF2.TXT"); err == nil {
							alreadyMounted = true
							break
						}
						<-time.After(1 * time.Second)
					}
				}

				if !alreadyMounted {
					log.Printf("DFU: ERROR - UF2 mount not found at %s", *uf2Mount)
					sendAck(cmdBegin, statusErr, 0)
					return
				}
				log.Printf("DFU: UF2 mounted at %s", *uf2Mount)
				sendAck(cmdBegin, statusOk, 0)

			case cmdChunk:
				if len(data) < 4 {
					sendAck(cmdChunk, statusErr, bytesReceived)
					return
				}
				chunkLen := int(data[1]) | int(data[2])<<8
				payload := data[3:]
				if len(payload) < chunkLen {
					payload = payload[:len(payload)]
				} else {
					payload = payload[:chunkLen]
				}
				firmware = append(firmware, payload...)
				bytesReceived += uint32(len(payload))
				sendAck(cmdChunk, statusOk, bytesReceived)

			case cmdFinish:
				log.Printf("DFU: FINISH - writing %d bytes to %s", len(firmware), *uf2Mount)
				uf2Path := *uf2Mount + "/firmware.uf2"
				if err := os.WriteFile(uf2Path, firmware, 0644); err != nil {
					log.Printf("DFU: ERROR writing UF2: %v", err)
					sendAck(cmdFinish, statusErr, bytesReceived)
					return
				}
				log.Printf("DFU: Flash complete")
				sendAck(cmdFinish, statusOk, bytesReceived)
				// Reconnect MIDI after device reboots
				go func() {
					<-time.After(5 * time.Second)
					for i := 0; i < 10; i++ {
						if err := connectMidi(); err == nil {
							return
						} else {
							log.Printf("DFU: reconnect attempt %d failed: %v", i+1, err)
						}
						<-time.After(1 * time.Second)
					}
					log.Printf("DFU: WARNING - failed to reconnect MIDI after flash")
				}()
				return

			case cmdAbort:
				log.Printf("DFU: ABORT")
				firmware = nil
				sendAck(cmdAbort, statusOk, 0)
				return
			}
		}
	})

	log.Printf("opendeck-bridge listening on %s (MIDI: %s)", *addr, *port)
	log.Fatal(http.ListenAndServe(*addr, nil))
}
