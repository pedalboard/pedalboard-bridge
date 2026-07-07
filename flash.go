package main

import (
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"time"
)

// handleFlash handles POST /flash — accepts UF2 file upload, writes to mounted UF2 drive.
func handleFlash() http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
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
	}
}
