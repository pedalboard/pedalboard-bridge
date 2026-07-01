package main

import (
	"embed"
	"encoding/hex"
	"flag"
	"fmt"
	"io"
	"io/fs"
	"log"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/gorilla/websocket"
)

//go:embed ui/*
var uiFiles embed.FS

var upgrader = websocket.Upgrader{
	CheckOrigin: func(r *http.Request) bool { return true },
}

var version = "dev"

func findMidiDevice(portName string) (string, error) {
	data, err := os.ReadFile("/proc/asound/cards")
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
	log.Printf("MIDI connected: %s", device)
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

func main() {
	addr := flag.String("addr", ":8080", "listen address")
	port := flag.String("port", "", "MIDI port name (substring match in /proc/asound/cards)")
	showVersion := flag.Bool("version", false, "print version and exit")
	flag.Parse()

	if *showVersion {
		fmt.Println(version)
		os.Exit(0)
	}

	if *port == "" {
		log.Fatal("Please specify -port flag")
	}

	var midi MidiPort
	var clientMu sync.Mutex
	var activeConn *websocket.Conn
	var monitorConn *websocket.Conn
	var clientReady bool

	// Connect to MIDI device
	connect := func() error {
		device, err := findMidiDevice(*port)
		if err != nil {
			return err
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

	// WebSocket /config endpoint (OpenDeck protocol)
	http.HandleFunc("/config", func(w http.ResponseWriter, r *http.Request) {
		conn, err := upgrader.Upgrade(w, r, nil)
		if err != nil {
			return
		}
		log.Printf("WebSocket client connected")
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
			// Send ConnectionClose
			midiSend([]byte{0xF0, 0x00, 0x53, 0x43, 0x00, 0x00, 0x00, 0xF7})
			conn.Close()
		}()
		for {
			_, data, err := conn.ReadMessage()
			if err != nil {
				break
			}
			log.Printf("MIDI OUT: %s", hex.EncodeToString(data))
			clientMu.Lock()
			clientReady = true
			clientMu.Unlock()
			midiSend(data)
		}
	})

	// WebSocket /raw endpoint (passthrough)
	http.HandleFunc("/raw", func(w http.ResponseWriter, r *http.Request) {
		conn, err := upgrader.Upgrade(w, r, nil)
		if err != nil {
			return
		}
		log.Printf("Raw WebSocket client connected")
		clientMu.Lock()
		activeConn = conn
		clientReady = true
		clientMu.Unlock()
		defer func() {
			clientMu.Lock()
			if activeConn == conn {
				activeConn = nil
			}
			clientMu.Unlock()
			conn.Close()
		}()
		for {
			_, data, err := conn.ReadMessage()
			if err != nil {
				break
			}
			log.Printf("RAW OUT: %s", hex.EncodeToString(data))
			midiSend(data)
		}
	})

	// POST /flash — accepts UF2 file upload, writes to mounted UF2 drive
	http.HandleFunc("/flash", func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost {
			http.Error(w, "POST only", http.StatusMethodNotAllowed)
			return
		}
		file, _, err := r.FormFile("firmware")
		if err != nil {
			http.Error(w, "missing 'firmware' file field", http.StatusBadRequest)
			return
		}
		defer file.Close()

		data, err := io.ReadAll(file)
		if err != nil {
			http.Error(w, "read error", http.StatusInternalServerError)
			return
		}
		log.Printf("Flash: received %d bytes", len(data))

		// Wait for UF2 drive to appear (device should already be in bootloader)
		var uf2Dev string
		for i := 0; i < 30; i++ {
			// Look for RP2040 boot drive by label
			matches, _ := filepath.Glob("/dev/disk/by-label/RPI-RP2*")
			if len(matches) > 0 {
				resolved, err := filepath.EvalSymlinks(matches[0])
				if err == nil {
					uf2Dev = resolved
					break
				}
			}
			// Fallback: check common device paths
			for _, dev := range []string{"/dev/sda1", "/dev/sdb1"} {
				if _, err := os.Stat(dev); err == nil {
					uf2Dev = dev
					break
				}
			}
			if uf2Dev != "" {
				break
			}
			time.Sleep(500 * time.Millisecond)
		}
		if uf2Dev == "" {
			http.Error(w, "UF2 drive not found (is device in bootloader mode?)", http.StatusServiceUnavailable)
			return
		}

		// Mount
		mountPath := "/mnt/uf2"
		os.MkdirAll(mountPath, 0o755)
		if out, err := exec.Command("sudo", "mount", "-o", "uid=1000,gid=1000", uf2Dev, mountPath).CombinedOutput(); err != nil {
			http.Error(w, fmt.Sprintf("mount failed: %s", string(out)), http.StatusInternalServerError)
			return
		}

		// Write UF2
		uf2Path := filepath.Join(mountPath, "firmware.uf2")
		if err := os.WriteFile(uf2Path, data, 0o644); err != nil {
			http.Error(w, fmt.Sprintf("write failed: %v", err), http.StatusInternalServerError)
			return
		}

		// Sync
		exec.Command("sync").Run()
		log.Printf("Flash: written %d bytes to %s", len(data), uf2Path)
		fmt.Fprintf(w, "OK: flashed %d bytes\n", len(data))
	})

	// WebSocket /monitor endpoint (streams all raw MIDI IN)
	http.HandleFunc("/monitor", func(w http.ResponseWriter, r *http.Request) {
		conn, err := upgrader.Upgrade(w, r, nil)
		if err != nil {
			return
		}
		log.Printf("Monitor client connected")
		clientMu.Lock()
		monitorConn = conn
		clientMu.Unlock()
		defer func() {
			clientMu.Lock()
			if monitorConn == conn {
				monitorConn = nil
			}
			clientMu.Unlock()
			conn.Close()
		}()
		// Keep connection alive until client disconnects
		for {
			if _, _, err := conn.ReadMessage(); err != nil {
				break
			}
		}
	})

	log.Printf("pedalboard-bridge %s listening on %s (MIDI: %s)", version, *addr, *port)
	log.Fatal(http.ListenAndServe(*addr, nil))
}
