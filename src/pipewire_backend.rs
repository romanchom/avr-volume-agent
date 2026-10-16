//! PipeWire graph, virtual sink, and AVR-facing control properties.
//!
//! The target sink determines the virtual sink's channel map and startup sample
//! rate. Raw `pw_filter` FFI stays in this module because the Rust crate has no
//! safe filter wrapper. Its RT callback only copies buffers; control and graph
//! work stays on the PipeWire loop.

use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::HashMap;
use std::error::Error;
use std::io;
use std::mem::MaybeUninit;
use std::ptr;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pipewire as pw;
use pipewire::loop_::{Signal, Timeout};
use pipewire::properties::PropertiesBox;
use pipewire::spa::pod::builder::Builder;
use tracing::{debug, info, warn};

use crate::denon::{self, ReceiverStatus, StatusFields};
use crate::network::{self, NetworkCommand, NetworkConfig, NetworkEvent};
use crate::sync::Reconciler;

const REGISTRY_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const LAYOUT_CHANGE_QUIET_PERIOD: Duration = Duration::from_millis(100);
const PROP_HARDWARE: u32 = pw::spa::sys::SPA_POD_PROP_FLAG_HARDWARE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Channel {
    name: &'static str,
    id: u32,
}

// Registry globals are unordered, so this table is both the exact SPA label map
// and the deterministic order used to advertise the virtual sink.
const SUPPORTED_CHANNELS: [Channel; 36] = [
    Channel {
        name: "MONO",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_MONO,
    },
    Channel {
        name: "FL",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_FL,
    },
    Channel {
        name: "FR",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_FR,
    },
    Channel {
        name: "RL",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_RL,
    },
    Channel {
        name: "RR",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_RR,
    },
    Channel {
        name: "FC",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_FC,
    },
    Channel {
        name: "LFE",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_LFE,
    },
    Channel {
        name: "SL",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_SL,
    },
    Channel {
        name: "SR",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_SR,
    },
    Channel {
        name: "FLC",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_FLC,
    },
    Channel {
        name: "FRC",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_FRC,
    },
    Channel {
        name: "RC",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_RC,
    },
    Channel {
        name: "TC",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_TC,
    },
    Channel {
        name: "TFL",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_TFL,
    },
    Channel {
        name: "TFC",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_TFC,
    },
    Channel {
        name: "TFR",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_TFR,
    },
    Channel {
        name: "TRL",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_TRL,
    },
    Channel {
        name: "TRC",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_TRC,
    },
    Channel {
        name: "TRR",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_TRR,
    },
    Channel {
        name: "RLC",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_RLC,
    },
    Channel {
        name: "RRC",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_RRC,
    },
    Channel {
        name: "FLW",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_FLW,
    },
    Channel {
        name: "FRW",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_FRW,
    },
    Channel {
        name: "LFE2",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_LFE2,
    },
    Channel {
        name: "FLH",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_FLH,
    },
    Channel {
        name: "FCH",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_FCH,
    },
    Channel {
        name: "FRH",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_FRH,
    },
    Channel {
        name: "TFLC",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_TFLC,
    },
    Channel {
        name: "TFRC",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_TFRC,
    },
    Channel {
        name: "TSL",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_TSL,
    },
    Channel {
        name: "TSR",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_TSR,
    },
    Channel {
        name: "LLFE",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_LLFE,
    },
    Channel {
        name: "RLFE",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_RLFE,
    },
    Channel {
        name: "BC",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_BC,
    },
    Channel {
        name: "BLC",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_BLC,
    },
    Channel {
        name: "BRC",
        id: pw::spa::sys::SPA_AUDIO_CHANNEL_BRC,
    },
];

#[derive(Clone, Debug, Eq, PartialEq)]
struct ChannelLayout {
    channels: Vec<Channel>,
}

impl ChannelLayout {
    fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.channels.iter().map(|channel| channel.name)
    }

    fn ids(&self) -> Vec<u32> {
        self.channels.iter().map(|channel| channel.id).collect()
    }
}

/// Configuration for the PipeWire virtual sink and its AVR control endpoint.
#[derive(Clone, Debug)]
pub struct PipeWireConfig {
    pub node_name: String,
    pub target_sink: String,
    pub avr_host: String,
    pub avr_port: u16,
    pub network_enabled: bool,
    pub write_enabled: bool,
}

struct Runtime {
    filter: *mut pw::sys::pw_filter,
    // Built before filter activation and never resized: the RT callback reads
    // these port handles without taking a lock.
    rt: RtPorts,
    // PipeWire control callbacks mutate this separately from the immutable RT ports.
    control: UnsafeCell<ControlState>,
}

struct ControlState {
    layout: ChannelLayout,
    volume: f32,
    muted: bool,
    write_enabled: bool,
    restore_volume: Option<f32>,
    restore_mute: Option<bool>,
    control_after: Option<Instant>,
    restore_guard_until: Option<Instant>,
    status: ReceiverStatus,
    ready: bool,
    reconciler: Reconciler,
    network_commands: Option<mpsc::Sender<NetworkCommand>>,
    network_events: Option<Receiver<NetworkEvent>>,
}

struct RtPorts {
    input: Vec<*mut std::ffi::c_void>,
    output: Vec<*mut std::ffi::c_void>,
}

struct PortData {
    _channel: usize,
}

struct RegistryState {
    // This state is confined to the PipeWire loop and is not shared with RT audio processing.
    core: *mut pw::sys::pw_core,
    target_sink: String,
    target_global: Option<pw::registry::GlobalObject<PropertiesBox>>,
    node_name: String,
    nodes: HashMap<u32, String>,
    ports: HashMap<u32, PortInfo>,
    link_proxies: HashMap<(u32, u32), *mut pw::sys::pw_proxy>,
    link_globals: HashMap<u32, (u32, u32)>,
    layout: Option<ChannelLayout>,
    layout_check_at: Option<Instant>,
    layout_error: Option<String>,
}

#[derive(Clone)]
struct PortInfo {
    node_id: u32,
    direction: String,
    channel: String,
}

struct NetworkWorkerGuard {
    sender: Option<mpsc::Sender<NetworkCommand>>,
    worker: Option<JoinHandle<()>>,
}

// The enum request uses a client sequence, but PipeWire reports its allocated AsyncSeq.
const CURRENT_FORMAT_QUERY_SEQ: i32 = 1;

struct SampleRateQuery {
    // Prefer Format when the node is active; properties cover suspended targets.
    active_format: Option<i32>,
    node_property: Option<i32>,
    listener: Option<pw::node::NodeListener>,
}

impl Drop for NetworkWorkerGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

impl NetworkWorkerGuard {
    fn stop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(NetworkCommand::Stop);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if !self.filter.is_null() {
            // SAFETY: Runtime owns this filter and outlives every callback registered on it.
            unsafe { pw::sys::pw_filter_destroy(self.filter) };
        }
    }
}

impl Drop for RegistryState {
    fn drop(&mut self) {
        for proxy in self.link_proxies.values().copied() {
            if !proxy.is_null() {
                destroy_link(self.core, proxy);
            }
        }
    }
}

fn discover_target_layout(state: &RegistryState) -> Result<Option<ChannelLayout>, String> {
    let target_nodes: Vec<u32> = state
        .nodes
        .iter()
        .filter_map(|(id, name)| (name == &state.target_sink).then_some(*id))
        .collect();
    if target_nodes.is_empty() {
        return Ok(None);
    }
    if target_nodes.len() > 1 {
        return Err(format!(
            "multiple nodes are named '{}', so its input ports are ambiguous",
            state.target_sink
        ));
    }

    let target_node = target_nodes[0];
    let mut channels = Vec::new();
    for port in state
        .ports
        .values()
        .filter(|port| port.node_id == target_node && port.direction == "in")
    {
        let channel = SUPPORTED_CHANNELS
            .iter()
            .find(|channel| channel.name == port.channel)
            .copied()
            .ok_or_else(|| format!("unsupported audio.channel label '{}'", port.channel))?;
        if channels
            .iter()
            .any(|present: &Channel| present.name == channel.name)
        {
            return Err(format!(
                "duplicate audio.channel label '{}' on target input ports",
                channel.name
            ));
        }
        channels.push(channel);
    }
    if channels.is_empty() {
        return Ok(None);
    }

    // SPA channel order is explicit so registry HashMap iteration cannot affect the layout.
    channels.sort_by_key(|channel| {
        SUPPORTED_CHANNELS
            .iter()
            .position(|supported| supported.name == channel.name)
            .expect("channels were mapped from SUPPORTED_CHANNELS")
    });
    Ok(Some(ChannelLayout { channels }))
}

fn query_active_sample_rate(
    core: &pw::core::CoreBox<'_>,
    mainloop: &pw::main_loop::MainLoopBox,
    target_node: &pw::node::Node,
) -> Result<SampleRateQuery, Box<dyn Error>> {
    // Active Format is preferred over the node property because it reflects the
    // negotiated rate; suspended nodes still report a configured audio.rate below.
    debug!("bound target node for active sample-rate query");
    let active_sample_rate = Rc::new(Cell::new(None));
    let active_rate_callback = Rc::clone(&active_sample_rate);
    let node_sample_rate = Rc::new(Cell::new(None));
    let node_rate_callback = Rc::clone(&node_sample_rate);
    let _node_listener = target_node
        .add_listener_local()
        .info(move |info| {
            let rate = info
                .props()
                .and_then(|props| props.get("audio.rate"))
                .and_then(|rate| rate.parse::<i32>().ok())
                .filter(|rate| *rate > 0);
            if let Some(rate) = rate {
                debug!(rate, "read target sample rate from node properties");
                node_rate_callback.set(Some(rate));
            }
        })
        .param(move |seq, param_type, _index, _next, param| {
            // Do not compare seq with CURRENT_FORMAT_QUERY_SEQ; PipeWire allocates an AsyncSeq.
            debug!(?seq, ?param_type, "received target node format parameter");
            if param_type != pw::spa::param::ParamType::Format {
                return;
            }
            let Some(param) = param else {
                return;
            };
            let Ok((media_type, _media_subtype)) =
                pw::spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != pw::spa::param::format::MediaType::Audio {
                return;
            }

            let rate = param
                .as_object()
                .ok()
                .and_then(|object| {
                    object.find_prop(pw::spa::utils::Id(pw::spa::sys::SPA_FORMAT_AUDIO_rate))
                })
                .and_then(|property| property.value().get_int().ok())
                .filter(|rate| *rate > 0);
            if let Some(rate) = rate {
                debug!(rate, "parsed active target sample rate");
                active_rate_callback.set(Some(rate));
            }
        })
        .register();

    // Format is the current negotiated POD. EnumFormat only advertises possible
    // rates and can select a different rate from the one used by the sink.
    debug!("requesting target node active Format");
    target_node.enum_params(
        CURRENT_FORMAT_QUERY_SEQ,
        Some(pw::spa::param::ParamType::Format),
        0,
        1,
    );
    // This barrier ensures the node's current-format response is delivered
    // before deciding whether a rate was available.
    let sync_seq = core.sync(0)?;
    debug!(?sync_seq, "requested sample-rate query barrier");
    let sync_complete = Rc::new(Cell::new(false));
    let sync_complete_callback = Rc::clone(&sync_complete);
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == sync_seq {
                sync_complete_callback.set(true);
            }
        })
        .register();

    let deadline = Instant::now() + REGISTRY_DISCOVERY_TIMEOUT;
    while !sync_complete.get() && Instant::now() < deadline {
        let result = mainloop
            .loop_()
            .iterate(Timeout::Finite(Duration::from_millis(50)));
        if result < 0 {
            return Err(io::Error::other(format!(
                "PipeWire loop failed while querying active format: {}",
                io::Error::from_raw_os_error(-result)
            ))
            .into());
        }
    }
    debug!(
        sync_complete = sync_complete.get(),
        "completed sample-rate query wait"
    );
    if !sync_complete.get() {
        debug!("timed out querying target sink active Format");
    }
    let active_rate = active_sample_rate.get();
    let node_rate = node_sample_rate.get();
    debug!(?active_rate, ?node_rate, "target sample-rate query results");
    Ok(SampleRateQuery {
        active_format: active_rate,
        node_property: node_rate,
        listener: Some(_node_listener),
    })
}

/// Run the PipeWire main loop until SIGINT or SIGTERM, then stop AVR I/O.
pub fn run(config: PipeWireConfig) -> Result<(), Box<dyn Error>> {
    if config.node_name == config.target_sink {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "target sink cannot be the virtual sink itself",
        )
        .into());
    }
    if config.avr_port == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "AVR port must be nonzero").into());
    }

    pw::init();
    let _pw_guard = PipeWireGuard;
    let mainloop = pw::main_loop::MainLoopBox::new(None)?;
    let context = pw::context::ContextBox::new(mainloop.loop_(), None)?;
    let core = context.connect(None)?;
    let registry = core.get_registry()?;

    let mut runtime = Box::new(Runtime {
        filter: ptr::null_mut(),
        rt: RtPorts {
            input: Vec::new(),
            output: Vec::new(),
        },
        control: UnsafeCell::new(ControlState {
            layout: ChannelLayout {
                channels: Vec::new(),
            },
            volume: 0.216,
            muted: false,
            write_enabled: config.write_enabled,
            restore_volume: None,
            restore_mute: None,
            control_after: None,
            restore_guard_until: None,
            status: ReceiverStatus {
                max_code: 98.0,
                ..ReceiverStatus::default()
            },
            ready: false,
            reconciler: Reconciler::default(),
            network_commands: None,
            network_events: None,
        }),
    });

    if config.network_enabled {
        let (event_sender, event_receiver) = mpsc::channel();
        let (commands, worker) = network::spawn(
            NetworkConfig {
                host: config.avr_host.clone(),
                port: config.avr_port,
                write_enabled: config.write_enabled,
            },
            event_sender,
        )?;
        let control = runtime.control.get_mut();
        control.network_commands = Some(commands);
        control.network_events = Some(event_receiver);
        run_pipewire(config, &mainloop, &core, &registry, runtime, Some(worker))
    } else {
        run_pipewire(config, &mainloop, &core, &registry, runtime, None)
    }
}

fn run_pipewire(
    config: PipeWireConfig,
    mainloop: &pw::main_loop::MainLoopBox,
    core: &pw::core::CoreBox<'_>,
    registry: &pw::registry::RegistryBox<'_>,
    mut runtime: Box<Runtime>,
    network_worker: Option<JoinHandle<()>>,
) -> Result<(), Box<dyn Error>> {
    let mut network_guard = NetworkWorkerGuard {
        sender: runtime.control.get_mut().network_commands.clone(),
        worker: network_worker,
    };
    let registry_state = Rc::new(RefCell::new(RegistryState {
        core: core.as_raw_ptr(),
        target_sink: config.target_sink.clone(),
        target_global: None,
        node_name: config.node_name.clone(),
        nodes: HashMap::new(),
        ports: HashMap::new(),
        link_proxies: HashMap::new(),
        link_globals: HashMap::new(),
        layout: None,
        layout_check_at: None,
        layout_error: None,
    }));
    let state_for_global = Rc::clone(&registry_state);
    let core_ptr = core.as_raw_ptr() as usize;
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            let props = match global.props.as_ref() {
                Some(props) => props,
                None => return,
            };
            let object_type = global.type_.to_str();
            let mut state = state_for_global.borrow_mut();
            let layout_relevant = matches!(
                object_type,
                "PipeWire:Interface:Node" | "PipeWire:Interface:Port"
            );
            match object_type {
                "PipeWire:Interface:Node" => {
                    if let Some(name) = props.get("node.name") {
                        state.nodes.insert(global.id, name.to_owned());
                        if name == state.target_sink {
                            state.target_global = Some(global.to_owned());
                        }
                    }
                }
                "PipeWire:Interface:Port" => {
                    let Some(node_id) = props.get("node.id").and_then(|value| value.parse().ok())
                    else {
                        return;
                    };
                    let Some(direction) = props.get("port.direction") else {
                        return;
                    };
                    let Some(channel) = props.get("audio.channel") else {
                        return;
                    };
                    state.ports.insert(
                        global.id,
                        PortInfo {
                            node_id,
                            direction: direction.to_owned(),
                            channel: channel.to_owned(),
                        },
                    );
                }
                "PipeWire:Interface:Link" => {
                    if let (Some(output), Some(input)) = (
                        props
                            .get("link.output.port")
                            .and_then(|value| value.parse().ok()),
                        props
                            .get("link.input.port")
                            .and_then(|value| value.parse().ok()),
                    ) {
                        let pair = (output, input);
                        state.link_globals.insert(global.id, pair);
                        if let std::collections::hash_map::Entry::Vacant(entry) =
                            state.link_proxies.entry(pair)
                        {
                            // An externally created matching link satisfies this channel pair.
                            entry.insert(ptr::null_mut());
                        }
                    }
                }
                _ => return,
            }
            if layout_relevant && state.layout.is_some() {
                state.layout_check_at = Some(Instant::now() + LAYOUT_CHANGE_QUIET_PERIOD);
            }
            if state.layout_error.is_none() {
                reconcile_links(&mut state, core_ptr as *mut pw::sys::pw_core);
            }
        })
        .global_remove({
            let state = Rc::clone(&registry_state);
            move |id| {
                let mut state = state.borrow_mut();
                state.nodes.remove(&id);
                if state
                    .target_global
                    .as_ref()
                    .is_some_and(|target| target.id == id)
                {
                    state.target_global = None;
                }
                state.ports.remove(&id);
                if let Some(pair) = state.link_globals.remove(&id) {
                    if let Some(proxy) = state.link_proxies.remove(&pair) {
                        if !proxy.is_null() {
                            // The registry link is already gone; release only its client proxy.
                            unsafe { pw::sys::pw_proxy_destroy(proxy) };
                        }
                    }
                }
                if state.layout.is_some() {
                    state.layout_check_at = Some(Instant::now() + LAYOUT_CHANGE_QUIET_PERIOD);
                }
                if state.layout_error.is_none() {
                    reconcile_links(&mut state, core_ptr as *mut pw::sys::pw_core);
                }
            }
        })
        .register();

    // One round trip makes the initial node/port snapshot complete before the
    // target layout is treated as authoritative.
    let sync_seq = core.sync(0)?;
    debug!(?sync_seq, "requested initial PipeWire registry sync");
    let sync_complete = Rc::new(Cell::new(false));
    let sync_complete_callback = Rc::clone(&sync_complete);
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == sync_seq {
                sync_complete_callback.set(true);
            }
        })
        .register();

    let sync_deadline = Instant::now() + REGISTRY_DISCOVERY_TIMEOUT;
    while !sync_complete.get() && Instant::now() < sync_deadline {
        mainloop
            .loop_()
            .iterate(Timeout::Finite(Duration::from_millis(50)));
    }
    if !sync_complete.get() {
        return Err(io::Error::other(format!(
            "timed out syncing PipeWire registry while discovering target sink '{}'",
            config.target_sink
        ))
        .into());
    }
    debug!("initial PipeWire registry sync completed");

    // Creating the filter before the target is known would hard-code a fallback
    // layout and force PipeWire to resample/relink from an invented channel map.
    // The filter's channel ports and sample format are immutable once active;
    // wait for the named target instead of inventing a fallback layout.
    let discovery_deadline = Instant::now() + REGISTRY_DISCOVERY_TIMEOUT;
    let layout = loop {
        let discovered = {
            let state = registry_state.borrow();
            discover_target_layout(&state)
        };
        match discovered {
            Ok(Some(layout)) => {
                debug!(
                    target = %config.target_sink,
                    channels = ?layout.names().collect::<Vec<_>>(),
                    "discovered target channel layout"
                );
                break layout;
            }
            Ok(None) if Instant::now() < discovery_deadline => {
                mainloop
                    .loop_()
                    .iterate(Timeout::Finite(Duration::from_millis(50)));
            }
            Ok(None) => {
                return Err(io::Error::other(format!(
                    "target PipeWire sink '{}' was not found or exposed no input audio ports within {} seconds",
                    config.target_sink,
                    REGISTRY_DISCOVERY_TIMEOUT.as_secs()
                ))
                .into());
            }
            Err(error) => {
                return Err(io::Error::other(format!(
                    "invalid channel layout for target PipeWire sink '{}': {error}",
                    config.target_sink
                ))
                .into());
            }
        }
    };
    let (target_global, configured_sample_rate) = {
        let state = registry_state.borrow();
        let target = state.target_global.as_ref().ok_or_else(|| {
            io::Error::other(format!(
                "target sink '{}' disappeared before its active format could be queried",
                config.target_sink
            ))
        })?;
        // The global may carry an explicit rate even when a suspended node has
        // no current Format parameter yet.
        let configured_rate = target
            .props
            .as_ref()
            .and_then(|props| props.get("audio.rate"))
            .and_then(|rate| rate.parse::<i32>().ok())
            .filter(|rate| *rate > 0);
        (target.to_owned(), configured_rate)
    };
    // Keep the proxy/listener alive through the main loop; their events and
    // teardown belong to this PipeWire connection's loop lifetime.
    let mut _target_node_for_runtime = None;
    let mut _target_node_listener_for_runtime = None;
    let active_sample_rate = match registry.bind::<pw::node::Node, _>(&target_global) {
        Ok(target_node) => {
            let query_result = query_active_sample_rate(core, mainloop, &target_node);
            _target_node_for_runtime = Some(target_node);
            match query_result {
                Ok(query) => {
                    _target_node_listener_for_runtime = query.listener;
                    query.active_format.or(query.node_property)
                }
                Err(error) => {
                    warn!(%error, "could not query target's active sample rate");
                    None
                }
            }
        }
        Err(error) => {
            warn!(%error, "could not bind target node to query its sample rate");
            None
        }
    };
    // Never invent a 48 kHz fallback: a mismatched format can fail to link or
    // silently resample in a way that hides the target's actual configuration.
    debug!(
        ?active_sample_rate,
        ?configured_sample_rate,
        "resolving target sample rate"
    );
    let sample_rate = active_sample_rate.or(configured_sample_rate)
        .ok_or_else(|| {
        io::Error::other(format!(
            "target sink '{}' exposes neither an active audio Format rate nor a valid audio.rate property",
            config.target_sink
        ))
    })?;
    debug!(
        target = %config.target_sink,
        sample_rate,
        "discovered target active sample rate"
    );
    registry_state.borrow_mut().layout = Some(layout.clone());
    runtime.control.get_mut().layout = layout.clone();
    create_filter(
        &config,
        mainloop.loop_().as_raw_ptr(),
        &mut runtime,
        &layout,
        sample_rate,
    )?;
    info!(
        target = %config.target_sink,
        sample_rate,
        channels = layout.channels.len(),
        "created virtual sink using target audio format"
    );
    reconcile_links(&mut registry_state.borrow_mut(), core.as_raw_ptr());

    let runtime_ptr = (&mut *runtime) as *mut Runtime as usize;
    let loop_ptr = mainloop.as_raw_ptr() as usize;
    let state_for_timer = Rc::clone(&registry_state);
    let timer = mainloop.loop_().add_timer(move |_| {
        // This timer is dispatched by the PipeWire main loop, never by the RT process thread.
        unsafe { poll_network_events(runtime_ptr as *mut Runtime) };
        let mut state = state_for_timer.borrow_mut();
        if state
            .layout_check_at
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            state.layout_check_at = None;
            match discover_target_layout(&state) {
                Ok(Some(layout)) if state.layout.as_ref() != Some(&layout) => {
                    state.layout_error = Some(format!(
                        "target sink '{}' returned with a different channel layout; restarting is required",
                        state.target_sink
                    ));
                }
                Err(error) => {
                    state.layout_error = Some(format!(
                        "target sink '{}' returned with an invalid channel layout: {error}",
                        state.target_sink
                    ));
                }
                _ => {}
            }
        }
        if let Some(error) = state.layout_error.as_deref() {
            warn!(%error, "stopping PipeWire filter after target layout change");
            // SAFETY: this timer runs on the live main loop and the pointer is its owner.
            unsafe { pw::sys::pw_main_loop_quit(loop_ptr as *mut pw::sys::pw_main_loop) };
        }
    });
    timer
        .update_timer(
            Some(Duration::from_millis(20)),
            Some(Duration::from_millis(20)),
        )
        .into_sync_result()?;

    let _sigint = mainloop.loop_().add_signal_local(Signal::INT, move || {
        // SAFETY: the signal source is owned by this live main loop and runs on its loop thread.
        unsafe {
            pw::sys::pw_main_loop_quit(loop_ptr as *mut pw::sys::pw_main_loop);
        }
    });
    let _sigterm = mainloop.loop_().add_signal_local(Signal::TERM, move || {
        // SAFETY: the signal source is owned by this live main loop and runs on its loop thread.
        unsafe {
            pw::sys::pw_main_loop_quit(loop_ptr as *mut pw::sys::pw_main_loop);
        }
    });

    mainloop.run();
    network_guard.stop();
    for (_, proxy) in registry_state.borrow_mut().link_proxies.drain() {
        if !proxy.is_null() {
            destroy_link(core.as_raw_ptr(), proxy);
        }
    }
    unsafe {
        // SAFETY: the filter was created on this loop and remains alive until after the loop exits.
        pw::sys::pw_filter_destroy(runtime.filter);
    }
    runtime.filter = ptr::null_mut();
    if let Some(error) = registry_state.borrow().layout_error.as_ref() {
        return Err(io::Error::other(error.clone()).into());
    }
    Ok(())
}

fn create_filter(
    config: &PipeWireConfig,
    loop_ptr: *mut pw::sys::pw_loop,
    runtime: &mut Runtime,
    layout: &ChannelLayout,
    sample_rate: i32,
) -> Result<(), Box<dyn Error>> {
    // The target's advertised positions become this sink's contract so clients
    // feed exactly the channels that can be linked onward to the target.
    let mut props = PropertiesBox::new();
    props.insert("media.type", "Audio");
    props.insert("media.category", "Playback");
    props.insert("media.role", "Music");
    props.insert("media.class", "Audio/Sink");
    props.insert("node.name", config.node_name.clone());
    props.insert("node.description", "AVR HDMI (network volume)");
    props.insert("node.nick", "AVR HDMI");
    props.insert("node.virtual", "true");
    props.insert("node.autoconnect", "true");
    props.insert("node.driver", "true");
    props.insert("priority.session", "2000");
    props.insert("audio.channels", layout.channels.len().to_string());
    props.insert(
        "audio.position",
        format!("[ {} ]", layout.names().collect::<Vec<_>>().join(", ")),
    );

    static FILTER_EVENTS: pw::sys::pw_filter_events = pw::sys::pw_filter_events {
        version: pw::sys::PW_VERSION_FILTER_EVENTS,
        destroy: None,
        state_changed: None,
        io_changed: None,
        param_changed: Some(param_changed),
        add_buffer: None,
        remove_buffer: None,
        process: Some(process),
        drained: None,
        command: None,
    };

    // SAFETY: `loop_ptr` is owned by the live MainLoopBox, properties ownership is transferred,
    // and the boxed Runtime address remains stable until after filter destruction.
    let filter = unsafe {
        pw::sys::pw_filter_new_simple(
            loop_ptr,
            c"avr-volume-agent".as_ptr(),
            props.into_raw(),
            &FILTER_EVENTS,
            (runtime as *mut Runtime).cast(),
        )
    };
    if filter.is_null() {
        return Err(io::Error::other("failed to create PipeWire filter").into());
    }
    runtime.filter = filter;

    runtime.rt.input = vec![ptr::null_mut(); layout.channels.len()];
    runtime.rt.output = vec![ptr::null_mut(); layout.channels.len()];
    let mut port_format_buffers = Vec::with_capacity(layout.channels.len());
    for (channel, channel_layout) in layout.channels.iter().enumerate() {
        port_format_buffers.push(build_format(sample_rate, 1, &[channel_layout.id])?);
        for (direction, is_input) in [("input", true), ("output", false)] {
            let mut props = PropertiesBox::new();
            props.insert("format.dsp", "32 bit float mono audio");
            props.insert("port.name", format!("{direction}_{}", channel_layout.name));
            props.insert("port.group", "playback");
            props.insert("audio.channel", channel_layout.name);
            let format = port_format_buffers
                .last()
                .expect("channel format was built");
            let mut params = [pod_ptr(format)];
            // SAFETY: the property object is transferred to PipeWire, the parameter POD and
            // callback-owned Runtime are valid throughout this call, and it copies port data size.
            let port = unsafe {
                pw::sys::pw_filter_add_port(
                    filter,
                    if is_input {
                        pw::sys::PW_DIRECTION_INPUT
                    } else {
                        pw::sys::PW_DIRECTION_OUTPUT
                    },
                    pw::sys::pw_filter_port_flags_PW_FILTER_PORT_FLAG_MAP_BUFFERS,
                    std::mem::size_of::<PortData>(),
                    props.into_raw(),
                    params.as_mut_ptr(),
                    1,
                )
            };
            if port.is_null() {
                return Err(io::Error::other("failed to add a PipeWire audio port").into());
            }
            // SAFETY: PipeWire returned writable port user-data storage of the requested size.
            unsafe { ptr::write(port.cast::<PortData>(), PortData { _channel: channel }) };
            if is_input {
                runtime.rt.input[channel] = port;
            } else {
                runtime.rt.output[channel] = port;
            }
        }
    }

    let channel_ids = layout.ids();
    let format = build_format(sample_rate, layout.channels.len() as i32, &channel_ids)?;
    let control = runtime.control.get_mut();
    let props = build_props(control.volume, control.muted, &control.layout)?;
    let volume_info = build_volume_info()?;
    let mute_info = build_mute_info()?;
    let mut params = [
        pod_ptr(&format),
        pod_ptr(&props),
        pod_ptr(&volume_info),
        pod_ptr(&mute_info),
    ];
    // SAFETY: all parameter POD buffers and the filter remain valid for this synchronous call.
    let result = unsafe {
        pw::sys::pw_filter_connect(
            filter,
            pw::sys::pw_filter_flags_PW_FILTER_FLAG_RT_PROCESS,
            params.as_mut_ptr(),
            params.len() as u32,
        )
    };
    if result < 0 {
        return Err(io::Error::from_raw_os_error(-result).into());
    }
    // SAFETY: the connected filter is live and activation is a PipeWire control-thread operation.
    let result = unsafe { pw::sys::pw_filter_set_active(filter, true) };
    if result < 0 {
        return Err(io::Error::from_raw_os_error(-result).into());
    }
    Ok(())
}

unsafe extern "C" fn process(
    data: *mut std::ffi::c_void,
    position: *mut pw::spa::sys::spa_io_position,
) {
    if data.is_null() || position.is_null() {
        return;
    }
    // SAFETY: PipeWire passes the live Runtime registered in pw_filter_new_simple and a valid
    // position object. `rt` is initialized before activation and never mutated while processing.
    let runtime = data.cast::<Runtime>();
    let rt = unsafe { &(*runtime).rt };
    let position = unsafe { &*position };
    let frames = position.clock.duration as usize;
    if frames == 0 {
        return;
    }
    // Hardware Props control the AVR; applying their gain here would attenuate
    // the copied signal a second time. This path deliberately remains unity gain.
    for channel in 0..rt.input.len() {
        // SAFETY: the port handles are valid until filter destruction; get_dsp_buffer is RT-safe.
        let input = unsafe { pw::sys::pw_filter_get_dsp_buffer(rt.input[channel], frames as u32) }
            .cast::<f32>();
        // SAFETY: same as above for the corresponding output port.
        let output =
            unsafe { pw::sys::pw_filter_get_dsp_buffer(rt.output[channel], frames as u32) }
                .cast::<f32>();
        if output.is_null() {
            continue;
        }
        if input.is_null() {
            // SAFETY: PipeWire provided a writable output buffer for `frames` float samples.
            unsafe { ptr::write_bytes(output, 0, frames) };
        } else {
            // SAFETY: input and output buffers each contain `frames` samples and do not overlap.
            unsafe { ptr::copy_nonoverlapping(input, output, frames) };
        }
    }
}

unsafe extern "C" fn param_changed(
    data: *mut std::ffi::c_void,
    port_data: *mut std::ffi::c_void,
    id: u32,
    param: *const pw::spa::sys::spa_pod,
) {
    if data.is_null()
        || !port_data.is_null()
        || id != pw::spa::sys::SPA_PARAM_Props
        || param.is_null()
    {
        return;
    }
    // SAFETY: PipeWire invokes parameter callbacks on the loop thread. This mutates only the
    // control cell; the RT callback accesses the separate immutable `rt` field.
    let runtime = data.cast::<Runtime>();
    let control = unsafe { &mut *(*runtime).control.get() };
    // SAFETY: `param` points to a PipeWire Props object; these helpers check the object type.
    if !unsafe { pw::spa::sys::spa_pod_is_object_type(param, pw::spa::sys::SPA_TYPE_OBJECT_Props) }
    {
        return;
    }

    let mut volume = control.volume;
    let mut muted = control.muted;
    // SAFETY: lookup returns a property within `param`, whose lifetime extends through this callback.
    let volume_prop = unsafe {
        pw::spa::sys::spa_pod_find_prop(param, ptr::null(), pw::spa::sys::SPA_PROP_channelVolumes)
    };
    if !volume_prop.is_null() {
        let mut values = vec![0.0f32; control.layout.channels.len()];
        // SAFETY: the property is an array of F32 values; the destination matches the filter layout.
        let count = unsafe {
            pw::spa::sys::spa_pod_copy_array_full(
                ptr::addr_of!((*volume_prop).value),
                pw::spa::sys::SPA_TYPE_Float,
                std::mem::size_of::<f32>() as u32,
                values.as_mut_ptr().cast(),
                values.len() as u32,
            )
        };
        if count > 0 {
            volume = values[0];
        }
    } else {
        // Some clients send the legacy scalar volume property instead of channelVolumes.
        let scalar = unsafe {
            pw::spa::sys::spa_pod_find_prop(param, ptr::null(), pw::spa::sys::SPA_PROP_volume)
        };
        if !scalar.is_null() {
            let _ = unsafe {
                pw::spa::sys::spa_pod_get_float(ptr::addr_of!((*scalar).value), &mut volume)
            };
        }
    }
    // SAFETY: lookup result refers to a bool value in the live parameter POD.
    let mute_prop =
        unsafe { pw::spa::sys::spa_pod_find_prop(param, ptr::null(), pw::spa::sys::SPA_PROP_mute) };
    if !mute_prop.is_null() {
        let _ = unsafe {
            pw::spa::sys::spa_pod_get_bool(ptr::addr_of!((*mute_prop).value), &mut muted)
        };
    }
    volume = volume.clamp(0.0, 1.0);
    if !control.ready && control.restore_volume.is_none() {
        control.restore_volume = Some(volume);
        control.restore_mute = Some(muted);
    }
    let mut volume_changed = (volume - control.volume).abs() > 0.00001;
    let mute_changed = muted != control.muted;
    if !volume_changed && !mute_changed {
        return;
    }
    let now = Instant::now();
    // Session managers may restore stale desktop Props at node creation. The
    // initial AVR report is authoritative, so briefly suppress those restores.
    if control.ready {
        if control.restore_guard_until.is_some_and(|until| now < until)
            && control
                .restore_volume
                .is_some_and(|restore| (volume - restore).abs() < 0.01)
        {
            if muted == control.muted {
                return;
            }
            volume = control.volume;
            volume_changed = false;
        }
        if control.control_after.is_some_and(|until| now < until) {
            if let Err(error) = update_props(unsafe { (*runtime).filter }, control) {
                warn!(%error, "failed to suppress startup PipeWire Props change");
            }
            return;
        }
    }
    control.volume = volume;
    control.muted = muted;
    if let Err(error) = update_props(unsafe { (*runtime).filter }, control) {
        warn!(%error, "failed to acknowledge PipeWire Props change");
    }
    if !control.ready || !control.write_enabled || control.network_commands.is_none() {
        return;
    }

    if volume_changed {
        let code = denon::slider_to_code(volume, control.status.max_code);
        control.reconciler.request_volume(code, now);
        if let Some(sender) = control.network_commands.as_ref() {
            let _ = sender.send(NetworkCommand::SetVolume(code));
        }
    }
    if mute_changed {
        control.reconciler.request_mute(muted, now);
        if let Some(sender) = control.network_commands.as_ref() {
            let _ = sender.send(NetworkCommand::SetMute(muted));
        }
    }
}

unsafe fn poll_network_events(runtime: *mut Runtime) {
    if runtime.is_null() {
        return;
    }
    // SAFETY: this pointer belongs to the run stack. Timer and parameter callbacks are serialized
    // on the loop; the RT callback only reads `rt`, not this control cell.
    let control = unsafe { &mut *(*runtime).control.get() };
    loop {
        let event = match control.network_events.as_ref() {
            Some(receiver) => receiver.try_recv().ok(),
            None => return,
        };
        let Some(event) = event else { break };
        apply_network_event(unsafe { (*runtime).filter }, control, event);
    }
}

fn apply_network_event(
    filter: *mut pw::sys::pw_filter,
    control: &mut ControlState,
    event: NetworkEvent,
) {
    // On first sync the AVR wins; after this guard window, later desktop changes
    // become commands and unsolicited receiver changes update PipeWire Props.
    let first_status = !control.ready
        && event
            .status
            .seen
            .contains(StatusFields::VOLUME | StatusFields::MAX_VOLUME);
    if first_status {
        let now = Instant::now();
        control.control_after = Some(now + Duration::from_secs(1));
        control.restore_guard_until = Some(now + Duration::from_secs(2));
        if control.restore_volume.is_none() {
            control.restore_volume = Some(control.volume);
            control.restore_mute = Some(control.muted);
        }
    }
    control.status = event.status;
    control.ready = event
        .status
        .seen
        .contains(StatusFields::VOLUME | StatusFields::MAX_VOLUME);
    let reconciled_fields = if event.changed.contains(StatusFields::MAX_VOLUME)
        && event.status.seen.contains(StatusFields::VOLUME)
    {
        event.changed | StatusFields::VOLUME
    } else {
        event.changed
    };
    let accepted =
        control
            .reconciler
            .accept_status(&event.status, reconciled_fields, Instant::now());
    debug!(
        status = ?event.status,
        changed = ?event.changed,
        accepted = ?accepted,
        "applied receiver state"
    );
    if let Some(code) = accepted.volume_code {
        control.volume = denon::code_to_channel_volume(code, event.status.max_code);
    }
    if let Some(muted) = accepted.muted {
        control.muted = muted;
    }
    if accepted.volume_code.is_some() || accepted.muted.is_some() {
        if let Err(error) = update_props(filter, control) {
            warn!(%error, "failed to apply AVR status to PipeWire Props");
        }
    }
}

fn update_props(
    filter: *mut pw::sys::pw_filter,
    control: &ControlState,
) -> Result<(), Box<dyn Error>> {
    let props = build_props(control.volume, control.muted, &control.layout)?;
    let mut params = [pod_ptr(&props)];
    // SAFETY: the filter is live on the PipeWire control loop and the POD remains valid for the call.
    let result = unsafe {
        pw::sys::pw_filter_update_params(filter, ptr::null_mut(), params.as_mut_ptr(), 1)
    };
    if result < 0 {
        return Err(io::Error::from_raw_os_error(-result).into());
    }
    Ok(())
}

fn reconcile_links(state: &mut RegistryState, core: *mut pw::sys::pw_core) {
    let Some(layout) = state.layout.as_ref() else {
        return;
    };
    let virtual_nodes: Vec<u32> = state
        .nodes
        .iter()
        .filter_map(|(id, name)| (name == &state.node_name).then_some(*id))
        .collect();
    let target_nodes: Vec<u32> = state
        .nodes
        .iter()
        .filter_map(|(id, name)| (name == &state.target_sink).then_some(*id))
        .collect();
    // Global IDs can change across hotplug, while audio.channel labels express
    // the stable relationship between each virtual output and target input.
    let mut desired = Vec::with_capacity(layout.channels.len());
    for channel in layout.names() {
        let output = state.ports.iter().find_map(|(id, port)| {
            (virtual_nodes.contains(&port.node_id)
                && port.direction == "out"
                && port.channel == channel)
                .then_some((*id, port.node_id))
        });
        let input = state.ports.iter().find_map(|(id, port)| {
            (target_nodes.contains(&port.node_id)
                && port.direction == "in"
                && port.channel == channel)
                .then_some((*id, port.node_id))
        });
        if let (Some((output, output_node)), Some((input, input_node))) = (output, input) {
            desired.push((output, input, output_node, input_node));
        }
    }

    let stale: Vec<(u32, u32)> = state
        .link_proxies
        .iter()
        .filter(|(_, proxy)| !proxy.is_null())
        .map(|(pair, _)| pair)
        .filter(|(output, input)| {
            !desired
                .iter()
                .any(|desired| desired.0 == *output && desired.1 == *input)
        })
        .copied()
        .collect();
    for pair in stale {
        if let Some(proxy) = state.link_proxies.remove(&pair) {
            if !proxy.is_null() {
                destroy_link(state.core, proxy);
            }
        }
    }

    for (output, input, output_node, input_node) in desired {
        if state.link_proxies.contains_key(&(output, input)) {
            continue;
        }
        match create_link(core, output, input, output_node, input_node) {
            Ok(proxy) => {
                state.link_proxies.insert((output, input), proxy);
                debug!(
                    output_port = output,
                    input_port = input,
                    "created PipeWire sink link"
                );
            }
            Err(error) => {
                warn!(%error, output_port = output, input_port = input, "failed to create PipeWire sink link")
            }
        }
    }
}

fn create_link(
    core: *mut pw::sys::pw_core,
    output: u32,
    input: u32,
    output_node: u32,
    input_node: u32,
) -> Result<*mut pw::sys::pw_proxy, Box<dyn Error>> {
    // Use PipeWire's link factory rather than spawning pw-link, so link lifetime
    // and hotplug reconciliation remain attached to this client/core.
    let mut props = PropertiesBox::new();
    props.insert("link.output.node", output_node.to_string());
    props.insert("link.output.port", output.to_string());
    props.insert("link.input.node", input_node.to_string());
    props.insert("link.input.port", input.to_string());
    let factory = c"link-factory";
    let dict = <PropertiesBox as AsRef<pw::spa::utils::dict::DictRef>>::as_ref(&props);
    // SAFETY: core is a live Core proxy; strings, dictionary, and the zero-sized user-data
    // argument remain valid for the synchronous create_object call.
    let proxy = unsafe {
        pw::spa::spa_interface_call_method!(
            core,
            pw::sys::pw_core_methods,
            create_object,
            factory.as_ptr(),
            pw::sys::PW_TYPE_INTERFACE_Link.as_ptr().cast::<i8>(),
            pw::sys::PW_VERSION_LINK,
            dict.as_raw_ptr(),
            0
        )
    };
    if proxy.is_null() {
        return Err(io::Error::other("PipeWire link-factory returned a null proxy").into());
    }
    Ok(proxy.cast())
}

fn destroy_link(core: *mut pw::sys::pw_core, proxy: *mut pw::sys::pw_proxy) {
    // SAFETY: RegistryState owns both live proxies. Core destroy removes the server-side link;
    // pw_proxy_destroy then releases its local proxy as Core::destroy_object does.
    unsafe {
        pw::spa::spa_interface_call_method!(
            core,
            pw::sys::pw_core_methods,
            destroy,
            proxy.cast::<std::ffi::c_void>()
        );
        pw::sys::pw_proxy_destroy(proxy);
    }
}

fn build_format(
    sample_rate: i32,
    channels: i32,
    positions: &[u32],
) -> Result<Vec<u8>, Box<dyn Error>> {
    // The filter remains F32 for simple RT copying; PipeWire converts at graph
    // boundaries, while the sample rate matches the target's configured rate.
    let mut data = vec![0; 1024];
    let mut builder = Builder::new(&mut data);
    let mut frame = MaybeUninit::uninit();
    // SAFETY: the frame remains unmoved until popped.
    unsafe {
        builder.push_object(
            &mut frame,
            pw::spa::sys::SPA_TYPE_OBJECT_Format,
            pw::spa::sys::SPA_PARAM_EnumFormat,
        )?;
    }
    builder.add_prop(pw::spa::sys::SPA_FORMAT_mediaType, 0)?;
    builder.add_id(pw::spa::utils::Id(pw::spa::sys::SPA_MEDIA_TYPE_audio))?;
    builder.add_prop(pw::spa::sys::SPA_FORMAT_mediaSubtype, 0)?;
    builder.add_id(pw::spa::utils::Id(pw::spa::sys::SPA_MEDIA_SUBTYPE_raw))?;
    builder.add_prop(pw::spa::sys::SPA_FORMAT_AUDIO_format, 0)?;
    builder.add_id(pw::spa::utils::Id(pw::spa::sys::SPA_AUDIO_FORMAT_F32))?;
    builder.add_prop(pw::spa::sys::SPA_FORMAT_AUDIO_rate, 0)?;
    builder.add_int(sample_rate)?;
    builder.add_prop(pw::spa::sys::SPA_FORMAT_AUDIO_channels, 0)?;
    builder.add_int(channels)?;
    builder.add_prop(pw::spa::sys::SPA_FORMAT_AUDIO_position, 0)?;
    // SAFETY: positions is an array of valid u32 SPA Id values with exactly its declared length.
    unsafe {
        builder.add_array(
            std::mem::size_of::<u32>() as u32,
            pw::spa::sys::SPA_TYPE_Id,
            positions.len() as u32,
            positions.as_ptr().cast(),
        )?;
        builder.pop(frame.assume_init_mut());
    }
    drop(builder);
    Ok(data)
}

fn build_props(
    volume: f32,
    muted: bool,
    layout: &ChannelLayout,
) -> Result<Vec<u8>, Box<dyn Error>> {
    // Volume/mute are hardware controls sent to the AVR. Keep software gain and
    // mute neutral so the audio copied by `process` is not changed twice.
    let mut data = vec![0; 2048];
    let mut builder = Builder::new(&mut data);
    let mut frame = MaybeUninit::uninit();
    // SAFETY: the frame remains unmoved until popped.
    unsafe {
        builder.push_object(
            &mut frame,
            pw::spa::sys::SPA_TYPE_OBJECT_Props,
            pw::spa::sys::SPA_PARAM_Props,
        )?
    };
    let volumes = vec![volume; layout.channels.len()];
    let soft_volumes = vec![1.0f32; layout.channels.len()];
    builder.add_prop(pw::spa::sys::SPA_PROP_channelVolumes, PROP_HARDWARE)?;
    // SAFETY: the arrays are contiguous F32 values whose lengths match the POD count.
    unsafe {
        builder.add_array(
            4,
            pw::spa::sys::SPA_TYPE_Float,
            volumes.len() as u32,
            volumes.as_ptr().cast(),
        )?;
    }
    builder.add_prop(pw::spa::sys::SPA_PROP_channelMap, 0)?;
    let channel_ids = layout.ids();
    // SAFETY: channel ids are contiguous SPA Id values.
    unsafe {
        builder.add_array(
            4,
            pw::spa::sys::SPA_TYPE_Id,
            channel_ids.len() as u32,
            channel_ids.as_ptr().cast(),
        )?;
    }
    builder.add_prop(pw::spa::sys::SPA_PROP_mute, PROP_HARDWARE)?;
    builder.add_bool(muted)?;
    builder.add_prop(pw::spa::sys::SPA_PROP_softMute, 0)?;
    builder.add_bool(false)?;
    builder.add_prop(pw::spa::sys::SPA_PROP_softVolumes, 0)?;
    // SAFETY: the unity values are contiguous F32 samples with a layout-sized count.
    unsafe {
        builder.add_array(
            4,
            pw::spa::sys::SPA_TYPE_Float,
            soft_volumes.len() as u32,
            soft_volumes.as_ptr().cast(),
        )?;
        builder.pop(frame.assume_init_mut());
    }
    drop(builder);
    Ok(data)
}

fn build_volume_info() -> Result<Vec<u8>, Box<dyn Error>> {
    let mut data = vec![0; 1024];
    let mut builder = Builder::new(&mut data);
    let mut object = MaybeUninit::uninit();
    // SAFETY: the frame remains unmoved until popped.
    unsafe {
        builder.push_object(
            &mut object,
            pw::spa::sys::SPA_TYPE_OBJECT_PropInfo,
            pw::spa::sys::SPA_PARAM_PropInfo,
        )?
    };
    builder.add_prop(pw::spa::sys::SPA_PROP_INFO_id, 0)?;
    builder.add_id(pw::spa::utils::Id(pw::spa::sys::SPA_PROP_channelVolumes))?;
    builder.add_prop(pw::spa::sys::SPA_PROP_INFO_description, 0)?;
    builder.add_string("AVR volume")?;
    builder.add_prop(pw::spa::sys::SPA_PROP_INFO_type, 0)?;
    let mut choice = MaybeUninit::uninit();
    // SAFETY: the nested choice frame remains unmoved until popped.
    unsafe { builder.push_choice(&mut choice, pw::spa::sys::SPA_CHOICE_Range, 0)? };
    builder.add_float(1.0)?;
    builder.add_float(0.0)?;
    builder.add_float(1.0)?;
    // SAFETY: choice is the last pushed frame.
    unsafe { builder.pop(choice.assume_init_mut()) };
    builder.add_prop(pw::spa::sys::SPA_PROP_INFO_container, 0)?;
    builder.add_id(pw::spa::utils::Id(pw::spa::sys::SPA_TYPE_Array))?;
    // SAFETY: object is the outermost and only remaining frame.
    unsafe { builder.pop(object.assume_init_mut()) };
    drop(builder);
    Ok(data)
}

fn build_mute_info() -> Result<Vec<u8>, Box<dyn Error>> {
    let mut data = vec![0; 1024];
    let mut builder = Builder::new(&mut data);
    let mut object = MaybeUninit::uninit();
    // SAFETY: the frame remains unmoved until popped.
    unsafe {
        builder.push_object(
            &mut object,
            pw::spa::sys::SPA_TYPE_OBJECT_PropInfo,
            pw::spa::sys::SPA_PARAM_PropInfo,
        )?
    };
    builder.add_prop(pw::spa::sys::SPA_PROP_INFO_id, 0)?;
    builder.add_id(pw::spa::utils::Id(pw::spa::sys::SPA_PROP_mute))?;
    builder.add_prop(pw::spa::sys::SPA_PROP_INFO_description, 0)?;
    builder.add_string("AVR mute")?;
    builder.add_prop(pw::spa::sys::SPA_PROP_INFO_type, 0)?;
    let mut choice = MaybeUninit::uninit();
    // SAFETY: the nested choice frame remains unmoved until popped.
    unsafe { builder.push_choice(&mut choice, pw::spa::sys::SPA_CHOICE_Enum, 0)? };
    builder.add_bool(false)?;
    builder.add_bool(false)?;
    builder.add_bool(true)?;
    // SAFETY: choice is the last pushed frame.
    unsafe { builder.pop(choice.assume_init_mut()) };
    // SAFETY: object is the outermost and only remaining frame.
    unsafe { builder.pop(object.assume_init_mut()) };
    drop(builder);
    Ok(data)
}

fn pod_ptr(data: &[u8]) -> *const pw::spa::sys::spa_pod {
    data.as_ptr().cast()
}

struct PipeWireGuard;

impl Drop for PipeWireGuard {
    fn drop(&mut self) {
        // SAFETY: PipeWire init occurred once at run entry and all PipeWire objects have dropped.
        unsafe { pw::sys::pw_deinit() };
    }
}
