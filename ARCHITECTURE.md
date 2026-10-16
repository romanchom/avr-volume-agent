# Architecture

This note describes the runtime boundaries and the reasons behind them. It is
not a PipeWire or Denon protocol reference.

## Two Planes

The application keeps audio processing separate from receiver control:

```text
Audio clients
    |
    v
PipeWire virtual sink input ports
    |
    | real-time F32 copy, unity gain
    v
PipeWire virtual sink output ports
    |
    | registry-discovered links by audio.channel
    v
Selected target sink (usually HDMI)

PipeWire volume/mute Props --> control loop --> AVR TCP worker --> Denon AVR
PipeWire volume/mute Props <-- control loop <-- AVR status events <-- Denon AVR
```

The copied samples are never attenuated by the AVR volume setting. Volume and
mute are advertised as hardware Props and translated to Denon commands; the
virtual sink's software gain and mute stay neutral to prevent double processing.

## Startup And PipeWire Graph

`main.rs` resolves an explicit `--target-sink`, or asks `pactl` for the current
default sink once at startup. The PipeWire backend then:

1. Connects a PipeWire context and observes the registry until its initial
   `core.sync` barrier completes.
2. Finds the selected sink's input ports and builds an ordered channel layout
   from their `audio.channel` labels. It rejects empty layouts, duplicate
   labels, or unsupported positions rather than inventing channels or assuming
   six.
3. Queries the target node's active `Format` sample rate. `EnumFormat` is only a
   list of supported alternatives, so it is not used as the current rate. If a
   suspended target has no active `Format`, its `audio.rate` node property is
   used. Startup fails if neither provides a valid rate.
4. Creates the virtual sink with one input/output pair per target channel, the
   same channel map, and the selected sample rate. It then creates PipeWire
   links matching ports by channel label.

PipeWire graph events maintain those links. If the target disappears, its links
are removed while the virtual sink stays available. When the same target layout
returns, the links are recreated. A different or invalid layout cannot be
applied by resizing the active filter's RT-visible ports, so the process exits
with an error and systemd restarts it to discover a fresh layout. The sample
rate is also selected at startup; restart the service after a target format
change.

The Rust `pipewire` crate does not currently provide a safe `pw_filter`
abstraction. The filter calls are therefore isolated in `pipewire_backend.rs`.
The callback state has a stable allocation for the filter lifetime, and all
port vectors are built before activation and remain immutable while processing.

## Real-Time Boundary

The PipeWire process callback is deliberately small: it gets each channel's
buffers and copies F32 frames, or writes silence when an input buffer is absent.
It does not allocate, lock, log, parse Props, perform network I/O, or apply
volume. Volume Props, registry changes, link creation, and AVR event processing
run on the PipeWire control loop instead.

`Runtime` separates immutable RT port handles from `ControlState`. The latter
uses `UnsafeCell` because PipeWire invokes the process callback on its RT thread
while control callbacks run on the loop; only the RT callback reads the port
handles, and only loop callbacks mutate control state.

## AVR Control And Reconciliation

`network.rs` owns the Denon TCP socket on one worker thread. It queries volume
and mute at connection, parses the line protocol, reconnects with bounded
exponential backoff, and sends commands serially. Rapid slider changes are
coalesced to the latest intent. The Denon protocol has no transaction IDs, so
the worker tracks one in-flight target per property and matches status by value;
unexpected reports while a command is pending are not allowed to overwrite a
newer request.

Events cross from the worker to the PipeWire loop through channels. The loop's
`Reconciler` keeps desired state separate from receiver-confirmed state and
filters delayed echoes before updating PipeWire Props. After a command timeout,
a fresh receiver report becomes authoritative instead of leaving the control
pending indefinitely.

`--read-only` keeps this connection and mirrors receiver status into PipeWire,
but does not send control commands. `--no-network` skips the AVR connection
entirely and leaves only the audio passthrough.

## Failure Behavior

- An unreachable AVR does not stop the PipeWire sink; the worker reconnects
  with backoff and retains the latest local intent.
- A missing target at startup causes a bounded discovery error. The systemd
  user service is configured to restart, so it can retry after PipeWire or the
  target appears.
- An HDMI unplug removes links, not the virtual sink. Reappearance with the
  same layout relinks automatically.
- A changed channel layout causes a clean process exit so the filter is rebuilt
  with the new immutable port set.
- An unsupported layout or unavailable sample rate fails clearly rather than
  silently using six channels or a hardcoded 48 kHz rate.

The project is experimental and vibe coded; this architecture describes the
intended invariants, not an independent safety audit.
