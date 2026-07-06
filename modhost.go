package main

import (
	"fmt"
	"log"
	"net"
	"strings"
	"sync"
	"time"
)

// ModHost manages the TCP connection to mod-host and sends commands.
type ModHost struct {
	mu   sync.Mutex
	conn net.Conn
	addr string
}

// NewModHost creates a mod-host client. Does not connect immediately.
func NewModHost(addr string) *ModHost {
	return &ModHost{addr: addr}
}

// Connect establishes the TCP connection to mod-host.
func (m *ModHost) Connect() error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.conn != nil {
		m.conn.Close()
	}
	conn, err := net.DialTimeout("tcp", m.addr, 2*time.Second)
	if err != nil {
		return fmt.Errorf("mod-host connect: %w", err)
	}
	m.conn = conn
	log.Printf("mod-host connected: %s", m.addr)
	return nil
}

// Send sends a command to mod-host and returns the response.
func (m *ModHost) Send(cmd string) (string, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.conn == nil {
		return "", fmt.Errorf("mod-host not connected")
	}
	_, err := fmt.Fprintf(m.conn, "%s\n", cmd)
	if err != nil {
		m.conn.Close()
		m.conn = nil
		return "", fmt.Errorf("mod-host send: %w", err)
	}
	// Read response (null-terminated)
	m.conn.SetReadDeadline(time.Now().Add(5 * time.Second))
	buf := make([]byte, 256)
	n, err := m.conn.Read(buf)
	m.conn.SetReadDeadline(time.Time{})
	if err != nil {
		return "", fmt.Errorf("mod-host read: %w", err)
	}
	resp := strings.TrimRight(string(buf[:n]), "\x00\n\r")
	return resp, nil
}

// SendNoReply sends a command without waiting for a response.
func (m *ModHost) SendNoReply(cmd string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.conn == nil {
		return fmt.Errorf("mod-host not connected")
	}
	_, err := fmt.Fprintf(m.conn, "%s\n", cmd)
	if err != nil {
		m.conn.Close()
		m.conn = nil
		return fmt.Errorf("mod-host send: %w", err)
	}
	return nil
}

// Close closes the connection.
func (m *ModHost) Close() {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.conn != nil {
		m.conn.Close()
		m.conn = nil
	}
}

// IsConnected returns true if the connection is established.
func (m *ModHost) IsConnected() bool {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.conn != nil
}

// --- High-level commands ---

// RemoveAll removes all loaded plugin instances.
func (m *ModHost) RemoveAll() error {
	resp, err := m.Send("remove -1")
	if err != nil {
		return err
	}
	if !strings.HasPrefix(resp, "resp 0") {
		log.Printf("mod-host remove_all: %s", resp)
	}
	return nil
}

// Add loads a plugin by URI with the given instance ID.
func (m *ModHost) Add(uri string, instanceID int) error {
	resp, err := m.Send(fmt.Sprintf("add %s %d", uri, instanceID))
	if err != nil {
		return err
	}
	if !strings.HasPrefix(resp, "resp 0") {
		return fmt.Errorf("mod-host add failed: %s", resp)
	}
	return nil
}

// ConnectPorts connects two JACK ports via mod-host.
func (m *ModHost) ConnectPorts(from, to string) error {
	resp, err := m.Send(fmt.Sprintf("connect %s %s", from, to))
	if err != nil {
		return err
	}
	if !strings.HasPrefix(resp, "resp 0") {
		log.Printf("mod-host connect %s → %s: %s", from, to, resp)
	}
	return nil
}

// ParamSet sets a plugin parameter value.
func (m *ModHost) ParamSet(instanceID int, param string, value float64) error {
	resp, err := m.Send(fmt.Sprintf("param_set %d %s %f", instanceID, param, value))
	if err != nil {
		return err
	}
	if !strings.HasPrefix(resp, "resp 0") {
		log.Printf("mod-host param_set %d %s=%f: %s", instanceID, param, value, resp)
	}
	return nil
}

// Bypass sets bypass state for a plugin instance.
func (m *ModHost) Bypass(instanceID int, bypass bool) error {
	val := 0
	if bypass {
		val = 1
	}
	resp, err := m.Send(fmt.Sprintf("bypass %d %d", instanceID, val))
	if err != nil {
		return err
	}
	if !strings.HasPrefix(resp, "resp 0") {
		log.Printf("mod-host bypass %d %d: %s", instanceID, val, resp)
	}
	return nil
}
