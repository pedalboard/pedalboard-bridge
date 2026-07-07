package main

import (
	"fmt"
	"log"
	"net/http"
	"os/exec"
	"strings"
	"sync"
	"time"
)

var designMode bool
var designModeMu sync.Mutex

// handleMode handles POST /mode — switch between live and design mode.
func handleMode(audioEngine *AudioEngine) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost {
			// GET returns current mode
			if audioEngine != nil && audioEngine.modhost.IsConnected() {
				fmt.Fprintln(w, "live")
			} else {
				fmt.Fprintln(w, "design")
			}
			return
		}
		mode := r.URL.Query().Get("set")
		if mode == "" {
			// Try reading from body
			buf := make([]byte, 32)
			n, _ := r.Body.Read(buf)
			mode = strings.TrimSpace(string(buf[:n]))
		}
		switch mode {
		case "design":
			if audioEngine != nil {
				designModeMu.Lock()
				designMode = true
				designModeMu.Unlock()
				audioEngine.modhost.Disconnect()
			}
			// Wait for TCP socket to fully close before MOD UI connects
			time.Sleep(500 * time.Millisecond)
			exec.Command("sudo", "systemctl", "start", "pedalboard-modui").Run()
			fmt.Fprintln(w, "design")
			log.Printf("Mode: design (MOD UI at http://localhost:8888/)")
		case "live":
			// Stop MOD UI, reconnect bridge to mod-host
			exec.Command("sudo", "systemctl", "stop", "pedalboard-modui").Run()
			if audioEngine != nil {
				designModeMu.Lock()
				designMode = false
				designModeMu.Unlock()
				if err := audioEngine.modhost.Reconnect(); err != nil {
					http.Error(w, err.Error(), http.StatusServiceUnavailable)
					return
				}
				go audioEngine.SwitchPatch(0)
			}
			fmt.Fprintln(w, "live")
			log.Printf("Mode: live (bridge controls mod-host)")
		default:
			http.Error(w, "use ?set=design or ?set=live", http.StatusBadRequest)
		}
	}
}
