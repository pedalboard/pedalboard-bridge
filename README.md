# pedalboard-bridge

WebSocket↔MIDI bridge for the pedalboard project. Runs on CM5 (Raspberry Pi), connects the CLI/web tools to the controller via JACK MIDI.

Written in Rust. Replaces the previous Go implementation.

## Features

- **WebSocket endpoints**: MIDI passthrough (`/raw`), monitoring (`/monitor`), firmware flashing (`/flash`)
- **JACK MIDI**: auto-connect with alias-based device discovery (polls every 1s)
- **Audio engine**: mod-host integration with preset↔patch switching via MIDI Program Change
- **Mode switching**: `/mode` endpoint toggles between live (bridge controls mod-host) and design (MOD UI controls mod-host)
- **Firmware flash**: POST `/flash` accepts UF2 upload, writes to RP2040 bootloader drive

## Usage

```
pedalboard-bridge --midi pedalboard-midi --addr 0.0.0.0:8080 --audio /etc/pedalboard/audio-patches.json --modhost localhost:5555
```

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--addr` | `:8080` | HTTP/WebSocket listen address |
| `--midi` | — | JACK MIDI port alias pattern for auto-connect |
| `--audio` | — | Audio patch config JSON (enables mod-host) |
| `--modhost` | `localhost:5555` | mod-host TCP address |

## Architecture

```
CLI/Browser ←WebSocket→ pedalboard-bridge ←JACK MIDI→ pedalboard-midi (RP2040)
                                          ←TCP→ mod-host (LV2 plugins)
```

## Build

```bash
cargo build --release
```

## Deploy to CM5

```bash
make deploy
```

## Development

```bash
cargo fmt        # format
cargo clippy     # lint
cargo test       # unit tests (host-side)
cargo test --test modhost_integration -- --ignored  # requires live mod-host
```

## Audio Patches

The `--audio` flag points to a JSON file defining plugin chains per preset:

```json
{
  "capture_port": "system:capture_2",
  "playback_port": "system:playback_2",
  "patches": [
    {
      "name": "Clean + Reverb",
      "plugins": [
        { "uri": "http://aidadsp.cc/plugins/aidadsp-bundle/rt-neural-loader", "id": 0 },
        { "uri": "http://calf.sourceforge.net/plugins/Reverb", "id": 1, "input": "in_l", "output": "out_l" }
      ],
      "params": [
        { "instance": 0, "param": "PREGAIN", "value": 0.3 },
        { "instance": 0, "param": "MASTER", "value": 0.7 }
      ]
    }
  ]
}
```

MIDI Program Change from the controller triggers `switch_patch` → removes current plugins, loads new chain, wires JACK ports.

## License

GPL
