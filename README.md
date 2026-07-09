# pedalboard-bridge

WebSocket↔MIDI bridge for the pedalboard project. Runs on CM5, connects the CLI/web tools to the controller via JACK MIDI.

## Features

- WebSocket endpoints for MIDI passthrough (`/raw`), monitoring (`/monitor`), and firmware flashing (`/flash`)
- JACK MIDI auto-connect with alias-based device discovery
- Audio engine: mod-host integration with preset↔patch switching via Program Change
- MOD UI mode switching (`/mode` endpoint)
- Embedded web UI

## Usage

```
pedalboard-bridge -midi "pedalboard-midi" -addr ":8080" -audio /etc/pedalboard/audio-patches.json
```

## Architecture

```
CLI/Browser ←WebSocket→ pedalboard-bridge ←JACK MIDI→ pedalboard-midi (RP2040)
                                          ←TCP→ mod-host (LV2 plugins)
```
