# AVR Volume Agent

> # ~~WARNING: VIBE-CODED, EXPERIMENTAL SOFTWARE~~
> # HUMAN HERE: AI IS TOO MODEST, IT'S AI SLOP
>
> This project is vibe coded and has not received an independent safety,
> security, or code-quality audit. It creates audio graph links and sends
> volume and mute commands to real hardware. Bugs can cause unexpected audio
> behavior or receiver control. Use it only if you are comfortable debugging
> Linux audio and Rust applications, test at a low listening level, and keep
> the AVR control port on a trusted network. Do not expose it to the public
> internet.

This Linux application exposes a PipeWire virtual sink whose channel layout is
discovered from the selected target sink, and synchronizes its volume and mute
controls with a Denon AVR. Audio is copied to the selected HDMI sink at unity
gain and at the target's sample rate; volume and mute commands control the AVR.
For an idle target with no active audio Format, the target's `audio.rate` node
property is used. The PipeWire graph is monitored so links can be recreated
when target ports appear or disappear.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the runtime data flow, synchronization
rules, PipeWire real-time boundary, and recovery behavior.

## Requirements

- Linux with PipeWire and WirePlumber running in the user session.
- A systemd user manager to use the packaged service.
- A Denon AVR reachable over its network control protocol, normally TCP port
  `23`, with network control enabled on the receiver.
- `pactl` and a PipeWire-Pulse server when relying on automatic default-sink
  selection. Alternatively, pass `--target-sink` explicitly.
- Rust 1.85 or newer, Cargo, PipeWire development files, `clang`, and `pkgconf`
  to build from source. The `pipewire` crate uses native PipeWire bindings.

The program currently has no config file. Its default AVR host is
`10.1.92.186`, which is a development default, not a generally valid receiver
address. See [Configuration](#configuration) to override it.

## Run From Source

Build and run with Cargo:

```sh
cargo build --release --locked
cargo run -- --help
cargo run -- --avr-host 192.168.1.50 --target-sink alsa_output.pci-0000_00_1f.3.hdmi-stereo
```

Available options include:

- `--avr-host HOST`: receiver hostname or IP address; defaults to
  `10.1.92.186`.
- `--port PORT`: AVR control port; defaults to `23`.
- `--target-sink NAME`: PipeWire sink to link to. If omitted, the program asks
  `pactl` for the current default sink.
- `--node-name NAME`: virtual sink name; defaults to `avr_volume_agent`.
- `--read-only`: keep the AVR TCP connection and mirror AVR status into
  PipeWire, but never send volume or mute commands back to the AVR.
- `--no-network`: do not connect to the AVR at all; run only the audio
  passthrough, with no AVR status synchronization.

These flags are distinct: use `--read-only` to observe/synchronize the receiver
without controlling it, or `--no-network` when you want the virtual sink with
no AVR connection.

`RUST_LOG` controls log verbosity. For example, `RUST_LOG=debug` enables debug
logs.

## Arch Linux

The current `packaging/arch/PKGBUILD` builds a package from this local source
checkout:

```sh
cd packaging/arch
makepkg -si
```

It installs the executable to `/usr/bin`, the user service to
`/usr/lib/systemd/user/avr-volume-agent.service`, and the license under
`/usr/share/licenses/avr-volume-agent/`. The package does not enable the service
automatically. Enable it for your user after installing:

```sh
systemctl --user daemon-reload
systemctl --user enable --now avr-volume-agent.service
systemctl --user status avr-volume-agent.service
```

View logs with:

```sh
journalctl --user -u avr-volume-agent.service -f
```

The service uses the CLI defaults, including the development AVR host. Override
the service command for your receiver with a systemd drop-in:

```sh
systemctl --user edit avr-volume-agent.service
```

For example, set the following in the editor, replacing the host and sink name:

```ini
[Service]
ExecStart=
ExecStart=/usr/bin/avr-volume-agent --avr-host 192.168.1.50 --target-sink alsa_output.pci-0000_00_1f.3.hdmi-stereo
```

Then apply the change:

```sh
systemctl --user daemon-reload
systemctl --user restart avr-volume-agent.service
```

Uninstall by disabling the service and removing the package:

```sh
systemctl --user disable --now avr-volume-agent.service
sudo pacman -R avr-volume-agent
```

> **Packaging limitation:** this PKGBUILD is local-checkout-only. It has no
> upstream source URL yet and must not be submitted to the AUR as-is. The URL
> is deliberately marked `.invalid`, and the source array is empty; `makepkg`
> copies files from this repository. Once the project is hosted, replace these
> with a pinned upstream source and checksum.

The package currently targets `x86_64`. LTO is disabled because Arch's LTO
settings caused unresolved symbols in `libspa-sys`'s generated C wrappers.

## Development Checks

The Makefile has been removed; use Cargo directly:

```sh
cargo check --all-targets --locked
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

The PipeWire integration tests require a running PipeWire-Pulse server and the
`pactl`, `wpctl`, `parec`, and `paplay` tools. They use local mock AVR and
PipeWire sinks to test 44.1 kHz stereo, 48 kHz six-channel, and 7.1 layouts:

```sh
cargo build
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -p 'test_pipewire_integration.py' -v
```

The GitHub Actions workflow runs formatting, Cargo checks, Clippy, unit tests,
and a local Cargo install smoke test. It does not run the live PipeWire
integration suite.

## Configuration

There is no XDG config file or config command yet. Receiver host, port, target
sink, and node name are currently supplied through command-line options or
their defaults. For the systemd service, use a user drop-in as shown above.

The old `~/.config/avr-volume-bridge/config.ini` belongs to a different bridge
program and is not read by this Rust application. Its `port = 8080` is that
program's HTTP API port. This application connects to the Denon Telnet control
interface on TCP port `23` by default; override it with `--port` if needed.
