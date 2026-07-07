package main

import (
	"fmt"
	"net"
	"testing"
	"time"
)

// startMockModHost starts a TCP server that reads a command and responds with
// the given reply followed by a null terminator, matching mod-host behavior.
func startMockModHost(t *testing.T, reply string) (string, func()) {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	go func() {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		defer conn.Close()
		buf := make([]byte, 256)
		for {
			n, err := conn.Read(buf)
			if err != nil {
				return
			}
			_ = n // we don't need to inspect the command for these tests
			// Respond with reply + null terminator
			conn.Write([]byte(reply + "\x00"))
		}
	}()
	return ln.Addr().String(), func() { ln.Close() }
}

func TestModHostSend(t *testing.T) {
	addr, cleanup := startMockModHost(t, "resp 0")
	defer cleanup()

	mh := NewModHost(addr)
	if err := mh.Connect(); err != nil {
		t.Fatalf("Connect: %v", err)
	}
	defer mh.Close()

	resp, err := mh.Send("add http://example.org/plugin 0")
	if err != nil {
		t.Fatalf("Send: %v", err)
	}
	if resp != "resp 0" {
		t.Errorf("got %q, want %q", resp, "resp 0")
	}
}

func TestModHostSendError(t *testing.T) {
	addr, cleanup := startMockModHost(t, "resp -1")
	defer cleanup()

	mh := NewModHost(addr)
	if err := mh.Connect(); err != nil {
		t.Fatalf("Connect: %v", err)
	}
	defer mh.Close()

	err := mh.Add("http://example.org/plugin", 0)
	if err == nil {
		t.Error("expected error for resp -1, got nil")
	}
}

func TestModHostConnectFail(t *testing.T) {
	mh := NewModHost("127.0.0.1:1") // port 1 should refuse connection
	err := mh.Connect()
	if err == nil {
		t.Error("expected connection error, got nil")
	}
}

func TestModHostIsConnected(t *testing.T) {
	addr, cleanup := startMockModHost(t, "resp 0")
	defer cleanup()

	mh := NewModHost(addr)
	if mh.IsConnected() {
		t.Error("should not be connected before Connect()")
	}
	if err := mh.Connect(); err != nil {
		t.Fatalf("Connect: %v", err)
	}
	if !mh.IsConnected() {
		t.Error("should be connected after Connect()")
	}
	mh.Disconnect()
	if mh.IsConnected() {
		t.Error("should not be connected after Disconnect()")
	}
}

func TestModHostMultipleCommands(t *testing.T) {
	addr, cleanup := startMockModHost(t, "resp 0")
	defer cleanup()

	mh := NewModHost(addr)
	if err := mh.Connect(); err != nil {
		t.Fatalf("Connect: %v", err)
	}
	defer mh.Close()

	// Send multiple commands in sequence
	for i := 0; i < 5; i++ {
		resp, err := mh.Send(fmt.Sprintf("remove %d", i))
		if err != nil {
			t.Fatalf("Send %d: %v", i, err)
		}
		if resp != "resp 0" {
			t.Errorf("Send %d: got %q, want %q", i, resp, "resp 0")
		}
	}
}

func TestModHostReconnect(t *testing.T) {
	// Start a server that accepts multiple connections
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()

	go func() {
		for {
			conn, err := ln.Accept()
			if err != nil {
				return
			}
			go func(c net.Conn) {
				defer c.Close()
				buf := make([]byte, 256)
				for {
					_, err := c.Read(buf)
					if err != nil {
						return
					}
					c.Write([]byte("resp 0\x00"))
				}
			}(conn)
		}
	}()

	mh := NewModHost(ln.Addr().String())
	if err := mh.Connect(); err != nil {
		t.Fatalf("Connect: %v", err)
	}

	// Disconnect and reconnect
	mh.Disconnect()
	time.Sleep(10 * time.Millisecond)

	if err := mh.Reconnect(); err != nil {
		t.Fatalf("Reconnect: %v", err)
	}
	if !mh.IsConnected() {
		t.Error("should be connected after Reconnect()")
	}

	resp, err := mh.Send("test")
	if err != nil {
		t.Fatalf("Send after reconnect: %v", err)
	}
	if resp != "resp 0" {
		t.Errorf("got %q, want %q", resp, "resp 0")
	}
	mh.Close()
}
