package main

import (
	"encoding/hex"
	"log"
	"net/http"
	"sync"

	"github.com/gorilla/websocket"
)

var upgrader = websocket.Upgrader{
	CheckOrigin: func(r *http.Request) bool { return true },
}

// handleConfig handles the /config WebSocket endpoint (legacy, kept for web UI compatibility).
func handleConfig(clientMu *sync.Mutex, activeConn **websocket.Conn, clientReady *bool, midiSend func([]byte)) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		conn, err := upgrader.Upgrade(w, r, nil)
		if err != nil {
			return
		}
		log.Printf("WebSocket client connected")
		clientMu.Lock()
		*activeConn = conn
		*clientReady = false
		clientMu.Unlock()
		defer func() {
			clientMu.Lock()
			if *activeConn == conn {
				*activeConn = nil
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
			*clientReady = true
			clientMu.Unlock()
			midiSend(data)
		}
	}
}

// handleRaw handles the /raw WebSocket endpoint (passthrough).
func handleRaw(clientMu *sync.Mutex, activeConn **websocket.Conn, clientReady *bool, midiSend func([]byte)) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		conn, err := upgrader.Upgrade(w, r, nil)
		if err != nil {
			return
		}
		log.Printf("Raw WebSocket client connected")
		clientMu.Lock()
		*activeConn = conn
		*clientReady = true
		clientMu.Unlock()
		defer func() {
			clientMu.Lock()
			if *activeConn == conn {
				*activeConn = nil
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
	}
}

// handleMonitor handles the /monitor WebSocket endpoint (streams all raw MIDI IN).
func handleMonitor(clientMu *sync.Mutex, monitorConn **websocket.Conn) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		conn, err := upgrader.Upgrade(w, r, nil)
		if err != nil {
			return
		}
		log.Printf("Monitor client connected")
		clientMu.Lock()
		*monitorConn = conn
		clientMu.Unlock()
		defer func() {
			clientMu.Lock()
			if *monitorConn == conn {
				*monitorConn = nil
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
	}
}
