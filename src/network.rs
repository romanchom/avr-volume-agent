//! Owns the blocking AVR TCP connection on a worker thread.
//! Keeping socket I/O out of PipeWire callbacks protects the audio loop, while
//! a single owner serializes commands and coalesces rapid slider changes.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::denon::{self, ReceiverStatus, StatusFields};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
const IO_TIMEOUT: Duration = Duration::from_millis(100);
const QUERY_INTERVAL: Duration = Duration::from_secs(30);
const INITIAL_STATUS_TIMEOUT: Duration = Duration::from_secs(1);
// If no matching reply arrives, a fresh query eventually makes receiver state authoritative.
const COMMAND_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Clone, Debug)]
pub struct NetworkConfig {
    pub host: String,
    pub port: u16,
    pub write_enabled: bool,
}

#[derive(Debug)]
pub enum NetworkCommand {
    SetVolume(i32),
    SetMute(bool),
    Stop,
}

#[derive(Debug)]
pub struct NetworkEvent {
    pub status: ReceiverStatus,
    pub changed: StatusFields,
}

#[derive(Clone, Copy)]
struct Pending<T> {
    // Denon replies contain values, not request IDs; this is the only available
    // acknowledgement key for deciding whether a report belongs to our write.
    target: T,
    sent_at: Instant,
}

#[derive(Default)]
struct DesiredState {
    // Keep the latest local intent separately from reported state so an old AVR
    // echo cannot replace a newer slider value while a command is in flight.
    volume: Option<i32>,
    volume_is_local: bool,
    mute: Option<bool>,
    mute_is_local: bool,
}

pub fn spawn(
    config: NetworkConfig,
    event_sender: Sender<NetworkEvent>,
) -> io::Result<(Sender<NetworkCommand>, JoinHandle<()>)> {
    let (command_sender, command_receiver) = mpsc::channel();
    let worker = thread::Builder::new()
        .name("avr-network".to_owned())
        .spawn(move || run(config, command_receiver, event_sender))?;
    Ok((command_sender, worker))
}

fn run(
    config: NetworkConfig,
    command_receiver: Receiver<NetworkCommand>,
    event_sender: Sender<NetworkEvent>,
) {
    // Retain the latest local intent across reconnects; after the receiver's
    // initial report, only the still-different value is replayed.
    let mut backoff = Duration::from_millis(250);
    let maximum_backoff = Duration::from_secs(10);
    let mut desired = DesiredState::default();

    loop {
        if receive_commands(&command_receiver, &mut desired) == CommandState::Stop {
            return;
        }

        match connect(&config.host, config.port) {
            Ok(mut stream) => {
                if let Err(error) = stream
                    .set_read_timeout(Some(IO_TIMEOUT))
                    .and_then(|()| stream.set_write_timeout(Some(CONNECT_TIMEOUT)))
                    .and_then(|()| stream.write_all(b"MV?\rMU?\r"))
                {
                    warn!(%error, "failed to initialize AVR connection");
                    if sleep_or_stop(backoff, &command_receiver, &mut desired) {
                        return;
                    }
                    backoff = (backoff * 2).min(maximum_backoff);
                    continue;
                }

                info!(host = %config.host, port = config.port, "connected to AVR");
                backoff = Duration::from_millis(250);
                // Each new socket needs a fresh receiver snapshot before local
                // intent can be reconciled or a queued command can be replayed.
                let disconnected = run_connected(
                    &config,
                    &command_receiver,
                    &event_sender,
                    &mut stream,
                    &mut desired,
                );
                if let Some(error) = disconnected {
                    warn!(%error, "AVR connection lost; reconnecting");
                }
            }
            Err(error) => {
                warn!(host = %config.host, port = config.port, %error, "AVR connection failed");
            }
        }

        if sleep_or_stop(backoff, &command_receiver, &mut desired) {
            return;
        }
        backoff = (backoff * 2).min(maximum_backoff);
    }
}

fn run_connected(
    config: &NetworkConfig,
    command_receiver: &Receiver<NetworkCommand>,
    event_sender: &Sender<NetworkEvent>,
    stream: &mut TcpStream,
    desired: &mut DesiredState,
) -> Option<io::Error> {
    let mut state = ConnectionState::default();
    let mut buffer = Vec::with_capacity(128);
    let mut read_buffer = [0u8; 512];
    let connected_at = Instant::now();
    let mut last_query = connected_at;

    loop {
        match receive_commands(command_receiver, desired) {
            CommandState::Stop => return None,
            CommandState::Continue => {}
        }

        if state.ready {
            if let Some(error) = dispatch_intents(config, stream, &mut state, desired) {
                return Some(error);
            }
        }

        match stream.read(&mut read_buffer) {
            Ok(0) => {
                return Some(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "AVR closed the connection",
                ));
            }
            Ok(count) => {
                for byte in &read_buffer[..count] {
                    match *byte {
                        b'\r' | b'\n' if !buffer.is_empty() => {
                            let line = String::from_utf8_lossy(&buffer).into_owned();
                            let changed = denon::parse_line(&line, &mut state.status);
                            buffer.clear();
                            if changed == StatusFields::default() {
                                continue;
                            }
                            state.seen = state.seen | changed;
                            if changed.contains(StatusFields::VOLUME) {
                                state.volume_reported =
                                    Some(state.status.volume_code.round() as i32);
                            }
                            if changed.contains(StatusFields::MUTE) {
                                state.mute_reported = Some(state.status.muted);
                            }

                            let all_initial_fields = StatusFields::VOLUME
                                | StatusFields::MAX_VOLUME
                                | StatusFields::MUTE;
                            if !state.ready && state.seen.contains(all_initial_fields) {
                                state.ready = true;
                                debug!(status = ?state.status, "received initial AVR state");
                                if let Some(error) = sync_initial_state(
                                    config,
                                    stream,
                                    &mut state,
                                    desired,
                                    event_sender,
                                ) {
                                    return Some(error);
                                }
                            } else if state.ready {
                                let accepted =
                                    accept_report(config, stream, &mut state, changed, desired);
                                match accepted {
                                    Ok(Some(accepted_fields)) => {
                                        let _ = event_sender.send(NetworkEvent {
                                            status: state.status,
                                            changed: accepted_fields,
                                        });
                                    }
                                    Ok(None) => {}
                                    Err(error) => return Some(error),
                                }
                            }
                        }
                        b'\r' | b'\n' => {}
                        byte if buffer.len() < 512 => buffer.push(byte),
                        _ => buffer.clear(),
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Some(error),
        }

        // Some receivers omit one field from their initial burst. A partial
        // volume/max snapshot is enough to expose the sink after a short grace.
        if !state.ready
            && connected_at.elapsed() >= INITIAL_STATUS_TIMEOUT
            && state
                .seen
                .contains(StatusFields::VOLUME | StatusFields::MAX_VOLUME)
        {
            state.ready = true;
            debug!(status = ?state.status, "using partial AVR state after startup timeout");
            if let Some(error) =
                sync_initial_state(config, stream, &mut state, desired, event_sender)
            {
                return Some(error);
            }
        }

        let pending_timed_out = state
            .volume_pending
            .is_some_and(|pending| pending.sent_at.elapsed() >= COMMAND_TIMEOUT)
            || state
                .mute_pending
                .is_some_and(|pending| pending.sent_at.elapsed() >= COMMAND_TIMEOUT);
        if (pending_timed_out && last_query.elapsed() >= COMMAND_TIMEOUT)
            || last_query.elapsed() >= QUERY_INTERVAL
        {
            if let Err(error) = stream.write_all(b"MV?\rMU?\r") {
                return Some(error);
            }
            last_query = Instant::now();
        }
    }
}

struct ConnectionState {
    status: ReceiverStatus,
    seen: StatusFields,
    ready: bool,
    volume_reported: Option<i32>,
    mute_reported: Option<bool>,
    volume_pending: Option<Pending<i32>>,
    mute_pending: Option<Pending<bool>>,
}

impl Default for ConnectionState {
    fn default() -> Self {
        Self {
            status: ReceiverStatus {
                max_code: 98.0,
                ..ReceiverStatus::default()
            },
            seen: StatusFields::default(),
            ready: false,
            volume_reported: None,
            mute_reported: None,
            volume_pending: None,
            mute_pending: None,
        }
    }
}

fn sync_initial_state(
    config: &NetworkConfig,
    stream: &mut TcpStream,
    state: &mut ConnectionState,
    desired: &mut DesiredState,
    event_sender: &Sender<NetworkEvent>,
) -> Option<io::Error> {
    // A reconnect must not overwrite a pending desktop intent with stale AVR
    // state; equal values acknowledge it, while differing values are resent.
    let mut changed = StatusFields::default();
    if state.seen.contains(StatusFields::MAX_VOLUME) {
        changed = changed | StatusFields::MAX_VOLUME;
    }
    if state.seen.contains(StatusFields::VOLUME) {
        if let Some(reported) = state.volume_reported {
            if let Some(error) = sync_initial_volume(
                config,
                stream,
                state.status.max_code,
                reported,
                desired,
                &mut state.volume_pending,
            ) {
                return Some(error);
            }
            if !desired.volume_is_local {
                changed = changed | StatusFields::VOLUME;
            }
        }
    }
    if state.seen.contains(StatusFields::MUTE) {
        if let Some(reported) = state.mute_reported {
            if let Some(error) =
                sync_initial_mute(config, stream, reported, desired, &mut state.mute_pending)
            {
                return Some(error);
            }
            if !desired.mute_is_local {
                changed = changed | StatusFields::MUTE;
            }
        }
    }

    if changed != StatusFields::default() {
        let _ = event_sender.send(NetworkEvent {
            status: ReceiverStatus {
                max_code: state.status.max_code,
                volume_code: state.volume_reported.unwrap_or_default() as f64,
                muted: state.mute_reported.unwrap_or_default(),
                seen: state.seen,
            },
            changed,
        });
    }
    None
}

fn sync_initial_volume(
    config: &NetworkConfig,
    stream: &mut TcpStream,
    max_code: f64,
    reported: i32,
    desired: &mut DesiredState,
    pending: &mut Option<Pending<i32>>,
) -> Option<io::Error> {
    if config.write_enabled && desired.volume_is_local {
        let target = desired
            .volume
            .unwrap_or(reported)
            .clamp(0, max_code.floor() as i32);
        if target != reported {
            return send_volume(stream, target, pending).err();
        }
    }
    desired.volume = Some(reported);
    desired.volume_is_local = false;
    None
}

fn sync_initial_mute(
    config: &NetworkConfig,
    stream: &mut TcpStream,
    reported: bool,
    desired: &mut DesiredState,
    pending: &mut Option<Pending<bool>>,
) -> Option<io::Error> {
    if config.write_enabled && desired.mute_is_local {
        let target = desired.mute.unwrap_or(reported);
        if target != reported {
            return send_mute(stream, target, pending).err();
        }
    }
    desired.mute = Some(reported);
    desired.mute_is_local = false;
    None
}

fn accept_report(
    config: &NetworkConfig,
    stream: &mut TcpStream,
    state: &mut ConnectionState,
    changed: StatusFields,
    desired: &mut DesiredState,
) -> Result<Option<StatusFields>, io::Error> {
    let mut accepted = StatusFields::default();
    if changed.contains(StatusFields::MAX_VOLUME) {
        accepted = accepted | StatusFields::MAX_VOLUME;
    }
    if changed.contains(StatusFields::VOLUME) {
        if let Some(reported) = state.volume_reported {
            if accept_volume_report(
                config,
                stream,
                state.status.max_code,
                reported,
                desired,
                &mut state.volume_pending,
            )? {
                accepted = accepted | StatusFields::VOLUME;
            }
        }
    }
    if changed.contains(StatusFields::MUTE) {
        if let Some(reported) = state.mute_reported {
            if accept_mute_report(config, stream, reported, desired, &mut state.mute_pending)? {
                accepted = accepted | StatusFields::MUTE;
            }
        }
    }

    if accepted == StatusFields::default() {
        Ok(None)
    } else {
        Ok(Some(accepted))
    }
}

fn accept_volume_report(
    config: &NetworkConfig,
    stream: &mut TcpStream,
    max_code: f64,
    reported: i32,
    desired: &mut DesiredState,
    pending: &mut Option<Pending<i32>>,
) -> Result<bool, io::Error> {
    // Serialize volume writes: the AVR protocol has no request ID, so only a
    // matching report can acknowledge the in-flight target without ambiguity.
    if let Some(in_flight) = *pending {
        if in_flight.target == reported {
            *pending = None;
            if desired.volume_is_local && desired.volume != Some(reported) {
                dispatch_volume(config, stream, max_code, reported, desired, pending)?;
                return Ok(false);
            }
            desired.volume = Some(reported);
            desired.volume_is_local = false;
            return Ok(true);
        }
        if in_flight.sent_at.elapsed() < COMMAND_TIMEOUT {
            return Ok(false);
        }
        *pending = None;
        desired.volume = Some(reported);
        desired.volume_is_local = false;
        return Ok(true);
    }

    if desired.volume_is_local && config.write_enabled {
        let target = desired
            .volume
            .unwrap_or(reported)
            .clamp(0, max_code.floor() as i32);
        if target != reported {
            dispatch_volume(config, stream, max_code, reported, desired, pending)?;
            return Ok(false);
        }
    }

    desired.volume = Some(reported);
    desired.volume_is_local = false;
    Ok(true)
}

fn accept_mute_report(
    config: &NetworkConfig,
    stream: &mut TcpStream,
    reported: bool,
    desired: &mut DesiredState,
    pending: &mut Option<Pending<bool>>,
) -> Result<bool, io::Error> {
    if let Some(in_flight) = *pending {
        if in_flight.target == reported {
            *pending = None;
            if desired.mute_is_local && desired.mute != Some(reported) {
                dispatch_mute(config, stream, reported, desired, pending)?;
                return Ok(false);
            }
            desired.mute = Some(reported);
            desired.mute_is_local = false;
            return Ok(true);
        }
        if in_flight.sent_at.elapsed() < COMMAND_TIMEOUT {
            return Ok(false);
        }
        *pending = None;
        desired.mute = Some(reported);
        desired.mute_is_local = false;
        return Ok(true);
    }

    if desired.mute_is_local && config.write_enabled {
        let target = desired.mute.unwrap_or(reported);
        if target != reported {
            dispatch_mute(config, stream, reported, desired, pending)?;
            return Ok(false);
        }
    }

    desired.mute = Some(reported);
    desired.mute_is_local = false;
    Ok(true)
}

fn dispatch_intents(
    config: &NetworkConfig,
    stream: &mut TcpStream,
    state: &mut ConnectionState,
    desired: &mut DesiredState,
) -> Option<io::Error> {
    if config.write_enabled {
        if state.volume_pending.is_none() && desired.volume_is_local {
            if let Some(reported) = state.volume_reported {
                if let Err(error) = dispatch_volume(
                    config,
                    stream,
                    state.status.max_code,
                    reported,
                    desired,
                    &mut state.volume_pending,
                ) {
                    return Some(error);
                }
            }
        }
        if state.mute_pending.is_none() && desired.mute_is_local {
            if let Some(reported) = state.mute_reported {
                if let Err(error) =
                    dispatch_mute(config, stream, reported, desired, &mut state.mute_pending)
                {
                    return Some(error);
                }
            }
        }
    }
    None
}

fn dispatch_volume(
    config: &NetworkConfig,
    stream: &mut TcpStream,
    max_code: f64,
    reported: i32,
    desired: &mut DesiredState,
    pending: &mut Option<Pending<i32>>,
) -> Result<(), io::Error> {
    let target = desired
        .volume
        .unwrap_or(reported)
        .clamp(0, max_code.floor() as i32);
    if !config.write_enabled || target == reported {
        desired.volume = Some(reported);
        desired.volume_is_local = false;
    } else {
        send_volume(stream, target, pending)?;
    }
    Ok(())
}

fn dispatch_mute(
    config: &NetworkConfig,
    stream: &mut TcpStream,
    reported: bool,
    desired: &mut DesiredState,
    pending: &mut Option<Pending<bool>>,
) -> Result<(), io::Error> {
    let target = desired.mute.unwrap_or(reported);
    if !config.write_enabled || target == reported {
        desired.mute = Some(reported);
        desired.mute_is_local = false;
    } else {
        send_mute(stream, target, pending)?;
    }
    Ok(())
}

fn send_volume(
    stream: &mut TcpStream,
    target: i32,
    pending: &mut Option<Pending<i32>>,
) -> Result<(), io::Error> {
    let command = format!("MV{target:02}\r");
    stream.write_all(command.as_bytes())?;
    *pending = Some(Pending {
        target,
        sent_at: Instant::now(),
    });
    debug!(target, "sent AVR volume command");
    Ok(())
}

fn send_mute(
    stream: &mut TcpStream,
    target: bool,
    pending: &mut Option<Pending<bool>>,
) -> Result<(), io::Error> {
    stream.write_all(if target { b"MUON\r" } else { b"MUOFF\r" })?;
    *pending = Some(Pending {
        target,
        sent_at: Instant::now(),
    });
    debug!(muted = target, "sent AVR mute command");
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CommandState {
    Continue,
    Stop,
}

fn receive_commands(
    receiver: &Receiver<NetworkCommand>,
    desired: &mut DesiredState,
) -> CommandState {
    loop {
        match receiver.try_recv() {
            Ok(NetworkCommand::SetVolume(code)) => {
                desired.volume = Some(code);
                desired.volume_is_local = true;
            }
            Ok(NetworkCommand::SetMute(muted)) => {
                desired.mute = Some(muted);
                desired.mute_is_local = true;
            }
            Ok(NetworkCommand::Stop) | Err(TryRecvError::Disconnected) => {
                return CommandState::Stop;
            }
            Err(TryRecvError::Empty) => return CommandState::Continue,
        }
    }
}

fn sleep_or_stop(
    duration: Duration,
    receiver: &Receiver<NetworkCommand>,
    desired: &mut DesiredState,
) -> bool {
    let end = Instant::now() + duration;
    while Instant::now() < end {
        match receiver.recv_timeout(IO_TIMEOUT) {
            Ok(NetworkCommand::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => return true,
            Ok(NetworkCommand::SetVolume(code)) => {
                desired.volume = Some(code);
                desired.volume_is_local = true;
            }
            Ok(NetworkCommand::SetMute(muted)) => {
                desired.mute = Some(muted);
                desired.mute_is_local = true;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    false
}

fn connect(host: &str, port: u16) -> io::Result<TcpStream> {
    let addresses: Vec<SocketAddr> = (host, port).to_socket_addrs()?.collect();
    let mut last_error = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "host resolved to no addresses",
        )
    }))
}
