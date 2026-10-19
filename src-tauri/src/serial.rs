//! Serial ports for the page's stand-in for `@serialport/bindings-cpp` (page/src/node/serialport.ts), which
//! tabby-serial uses through `@serialport/stream`: listing ports, opening one with the usual line settings, writes,
//! baud-rate changes, control lines, flush and drain.
//!
//! A port's input is read on a thread of its own (the port library's reads block) and goes to the page as raw
//! frames on a Tauri channel, `[0][bytes]` for data and `[1][JSON]` for the end of input or an error, as the
//! sockets in net.rs do. Reading pauses while more than MAX_UNACKED bytes are unacknowledged by the page.
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serialport::{ClearBuffer, DataBits, FlowControl, Parity, SerialPort, SerialPortType, StopBits};
use tauri::ipc::{Channel, InvokeBody, InvokeResponseBody, Request};
use tauri::State;

const READ_CHUNK: usize = 64 * 1024;
const MAX_UNACKED: usize = 1024 * 1024;
/// How long a read waits before checking whether the port was closed meanwhile.
const READ_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Serialize, Debug)]
pub struct SerialError {
    code: String,
    message: String,
}

impl SerialError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        SerialError { code: code.to_string(), message: message.into() }
    }
}

fn io_code(kind: std::io::ErrorKind) -> &'static str {
    use std::io::ErrorKind::*;
    match kind {
        NotFound => "ENOENT",
        PermissionDenied => "EACCES",
        TimedOut => "ETIMEDOUT",
        BrokenPipe => "EPIPE",
        InvalidInput => "EINVAL",
        AlreadyExists => "EBUSY",
        _ => "EIO",
    }
}

impl From<serialport::Error> for SerialError {
    fn from(e: serialport::Error) -> Self {
        let code = match e.kind() {
            serialport::ErrorKind::NoDevice => "ENOENT",
            serialport::ErrorKind::InvalidInput => "EINVAL",
            serialport::ErrorKind::Io(kind) => io_code(kind),
            serialport::ErrorKind::Unknown => "EIO",
        };
        SerialError::new(code, e.to_string())
    }
}

impl From<std::io::Error> for SerialError {
    fn from(e: std::io::Error) -> Self {
        SerialError::new(io_code(e.kind()), e.to_string())
    }
}

/// As `@serialport/bindings-interface` describes a port (only `path` is certain).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortInfo {
    path: String,
    manufacturer: Option<String>,
    serial_number: Option<String>,
    pnp_id: Option<String>,
    location_id: Option<String>,
    product_id: Option<String>,
    vendor_id: Option<String>,
}

/// `@serialport/bindings-interface`'s OpenOptions.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenOptions {
    path: String,
    baud_rate: u32,
    data_bits: Option<u8>,
    stop_bits: Option<f64>,
    parity: Option<String>,
    rtscts: Option<bool>,
    xon: Option<bool>,
    xoff: Option<bool>,
    xany: Option<bool>,
    lock: Option<bool>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenedPort {
    id: u32,
    baud_rate: u32,
}

/// `@serialport/bindings-interface`'s SetOptions: every line is set on each call, to the value given or the usual one.
#[derive(Deserialize, Default)]
pub struct SetOptions {
    brk: Option<bool>,
    dtr: Option<bool>,
    rts: Option<bool>,
}

#[derive(Serialize)]
pub struct PortStatus {
    cts: bool,
    dsr: bool,
    dcd: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BaudRate {
    baud_rate: u32,
}

struct Port {
    closed: AtomicBool,
    /// Writes and settings; the reader thread has a clone of the port.
    handle: Mutex<Box<dyn SerialPort>>,
    /// A pseudo-terminal (socat's virtual ports, say) has no line speed or modem lines: the baud rate asked for is
    /// remembered for it, and its control lines are left alone.
    pseudo: bool,
    baud_rate: Mutex<u32>,
    unacked: Mutex<usize>,
    room: Condvar,
}

impl Port {
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.room.notify_all();
    }
}

#[derive(Default)]
pub struct Serial {
    next: AtomicU32,
    ports: Mutex<HashMap<u32, Arc<Port>>>,
}

impl Serial {
    fn port(&self, id: u32) -> Result<Arc<Port>, SerialError> {
        self.ports
            .lock()
            .unwrap()
            .get(&id)
            .filter(|port| !port.closed.load(Ordering::SeqCst))
            .cloned()
            .ok_or_else(|| SerialError::new("ERR_PORT_CLOSED", "Port is not open"))
    }
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "camelCase")]
enum PortEvent {
    End,
    Error { code: String, message: String },
}

fn event_frame(event: &PortEvent) -> Vec<u8> {
    let mut out = vec![1];
    out.extend_from_slice(&serde_json::to_vec(event).unwrap_or_default());
    out
}

fn line_settings(options: &OpenOptions) -> Result<(DataBits, StopBits, Parity, FlowControl), SerialError> {
    let data_bits = match options.data_bits.unwrap_or(8) {
        5 => DataBits::Five,
        6 => DataBits::Six,
        7 => DataBits::Seven,
        8 => DataBits::Eight,
        other => return Err(SerialError::new("EINVAL", format!("Unsupported data bits: {other}"))),
    };
    let stop_bits = match options.stop_bits.unwrap_or(1.0) {
        x if x == 1.0 => StopBits::One,
        x if x == 2.0 => StopBits::Two,
        other => return Err(SerialError::new("EINVAL", format!("Unsupported stop bits: {other} (1 or 2 here)"))),
    };
    let parity = match options.parity.as_deref().unwrap_or("none") {
        "none" => Parity::None,
        "even" => Parity::Even,
        "odd" => Parity::Odd,
        other => return Err(SerialError::new("EINVAL", format!("Unsupported parity: {other} (none, even or odd here)"))),
    };
    let flow_control = if options.rtscts.unwrap_or(false) {
        FlowControl::Hardware
    } else if options.xon.unwrap_or(false) || options.xoff.unwrap_or(false) || options.xany.unwrap_or(false) {
        FlowControl::Software
    } else {
        FlowControl::None
    };
    Ok((data_bits, stop_bits, parity, flow_control))
}

/// macOS's pseudo-terminals are /dev/ttysNNN.
fn is_pseudo_terminal(path: &str) -> bool {
    cfg!(target_os = "macos") && path.strip_prefix("/dev/ttys").is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// The ports the system knows, USB ones with their maker and serial number.
#[tauri::command]
pub fn serial_list() -> Result<Vec<PortInfo>, SerialError> {
    let ports = serialport::available_ports()?;
    Ok(ports
        .into_iter()
        .map(|port| {
            let usb = match port.port_type {
                SerialPortType::UsbPort(info) => Some(info),
                _ => None,
            };
            PortInfo {
                path: port.port_name,
                manufacturer: usb.as_ref().and_then(|u| u.manufacturer.clone()),
                serial_number: usb.as_ref().and_then(|u| u.serial_number.clone()),
                pnp_id: None,
                location_id: None,
                product_id: usb.as_ref().map(|u| format!("{:04x}", u.pid)),
                vendor_id: usb.as_ref().map(|u| format!("{:04x}", u.vid)),
            }
        })
        .collect())
}

/// Opens the port with the settings given; input goes to `data` from then on.
#[tauri::command]
pub fn serial_open(serial: State<'_, Serial>, options: OpenOptions, data: Channel<InvokeResponseBody>) -> Result<OpenedPort, SerialError> {
    let (data_bits, stop_bits, parity, flow_control) = line_settings(&options)?;
    let build = |baud_rate: u32| {
        serialport::new(&options.path, baud_rate)
            .data_bits(data_bits)
            .stop_bits(stop_bits)
            .parity(parity)
            .flow_control(flow_control)
            .timeout(READ_TIMEOUT)
            // (the driver's own DTR behaviour on open, as Node's bindings leave it)
            .preserve_dtr_on_open()
            .exclusive(options.lock.unwrap_or(true))
            .open()
    };
    let failed = |e: serialport::Error| {
        let message = format!("Opening {}: {e}", options.path);
        SerialError { message, ..SerialError::from(e) }
    };
    // On macOS the port library sets the speed with an ioctl a pseudo-terminal doesn't have (ENOTTY, "Not a
    // typewriter"); a speed of 0 is its way of leaving the speed alone.
    let mut pseudo = is_pseudo_terminal(&options.path);
    let handle = if pseudo {
        build(0).map_err(failed)?
    } else {
        match build(options.baud_rate) {
            Ok(handle) => handle,
            Err(e) if cfg!(target_os = "macos") && e.to_string().contains("typewriter") => {
                pseudo = true;
                build(0).map_err(failed)?
            }
            Err(e) => return Err(failed(e)),
        }
    };
    let mut reader = handle.try_clone()?;
    let baud_rate = if pseudo { options.baud_rate } else { handle.baud_rate().unwrap_or(options.baud_rate) };
    let port = Arc::new(Port {
        closed: AtomicBool::new(false),
        handle: Mutex::new(handle),
        pseudo,
        baud_rate: Mutex::new(baud_rate),
        unacked: Mutex::new(0),
        room: Condvar::new(),
    });
    let id = serial.next.fetch_add(1, Ordering::Relaxed) + 1;
    serial.ports.lock().unwrap().insert(id, port.clone());

    std::thread::Builder::new()
        .name(format!("serial-{id}"))
        .spawn(move || {
            let mut buf = vec![0u8; READ_CHUNK];
            loop {
                // Waits while the page is behind.
                {
                    let mut unacked = port.unacked.lock().unwrap();
                    while *unacked >= MAX_UNACKED && !port.closed.load(Ordering::SeqCst) {
                        unacked = port.room.wait_timeout(unacked, READ_TIMEOUT).unwrap().0;
                    }
                }
                if port.closed.load(Ordering::SeqCst) {
                    return;
                }
                let frame = match reader.read(&mut buf) {
                    Ok(0) => event_frame(&PortEvent::End),
                    Ok(n) => {
                        *port.unacked.lock().unwrap() += n;
                        let mut frame = Vec::with_capacity(n + 1);
                        frame.push(0);
                        frame.extend_from_slice(&buf[..n]);
                        if data.send(InvokeResponseBody::Raw(frame)).is_err() {
                            return;
                        }
                        continue;
                    }
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock) => continue,
                    Err(e) => {
                        let code = io_code(e.kind());
                        event_frame(&PortEvent::Error { code: code.to_string(), message: format!("read {code}: {e}") })
                    }
                };
                if !port.closed.load(Ordering::SeqCst) {
                    let _ = data.send(InvokeResponseBody::Raw(frame));
                }
                return;
            }
        })
        .map_err(SerialError::from)?;
    Ok(OpenedPort { id, baud_rate })
}

/// The raw request body is the bytes, the `serial-id` header says which port.
#[tauri::command]
pub async fn serial_write(serial: State<'_, Serial>, request: Request<'_>) -> Result<(), SerialError> {
    let id = request.headers().get("serial-id").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).unwrap_or(0);
    let port = serial.port(id)?;
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err(SerialError::new("EINVAL", "expected raw bytes"));
    };
    let bytes = bytes.clone();
    tauri::async_runtime::spawn_blocking(move || {
        if port.closed.load(Ordering::SeqCst) {
            return Err(SerialError::new("ERR_PORT_CLOSED", "Port is not open"));
        }
        port.handle.lock().unwrap().write_all(&bytes).map_err(SerialError::from)
    })
    .await
    .map_err(|e| SerialError::new("EIO", e.to_string()))?
}

#[tauri::command]
pub fn serial_ack(serial: State<'_, Serial>, id: u32, length: usize) {
    if let Ok(port) = serial.port(id) {
        let mut unacked = port.unacked.lock().unwrap();
        *unacked = unacked.saturating_sub(length);
        port.room.notify_all();
    }
}

#[tauri::command]
pub fn serial_close(serial: State<'_, Serial>, id: u32) {
    let port = serial.ports.lock().unwrap().remove(&id);
    if let Some(port) = port {
        port.close();
    }
}

#[tauri::command]
pub fn serial_update(serial: State<'_, Serial>, id: u32, baud_rate: u32) -> Result<(), SerialError> {
    let port = serial.port(id)?;
    if !port.pseudo {
        port.handle.lock().unwrap().set_baud_rate(baud_rate)?;
    }
    *port.baud_rate.lock().unwrap() = baud_rate;
    Ok(())
}

#[tauri::command]
pub fn serial_set(serial: State<'_, Serial>, id: u32, options: Option<SetOptions>) -> Result<(), SerialError> {
    let options = options.unwrap_or_default();
    let port = serial.port(id)?;
    if port.pseudo {
        return Ok(());
    }
    let mut handle = port.handle.lock().unwrap();
    handle.write_data_terminal_ready(options.dtr.unwrap_or(true))?;
    handle.write_request_to_send(options.rts.unwrap_or(true))?;
    if options.brk.unwrap_or(false) {
        handle.set_break()?;
    } else {
        handle.clear_break()?;
    }
    Ok(())
}

#[tauri::command]
pub fn serial_get(serial: State<'_, Serial>, id: u32) -> Result<PortStatus, SerialError> {
    let port = serial.port(id)?;
    if port.pseudo {
        return Ok(PortStatus { cts: false, dsr: false, dcd: false });
    }
    let mut handle = port.handle.lock().unwrap();
    Ok(PortStatus {
        cts: handle.read_clear_to_send()?,
        dsr: handle.read_data_set_ready()?,
        dcd: handle.read_carrier_detect()?,
    })
}

#[tauri::command]
pub fn serial_get_baud_rate(serial: State<'_, Serial>, id: u32) -> Result<BaudRate, SerialError> {
    let port = serial.port(id)?;
    let baud_rate = if port.pseudo { *port.baud_rate.lock().unwrap() } else { port.handle.lock().unwrap().baud_rate()? };
    Ok(BaudRate { baud_rate })
}

/// Discards what was received but not read, and what was written but not sent.
#[tauri::command]
pub fn serial_flush(serial: State<'_, Serial>, id: u32) -> Result<(), SerialError> {
    serial.port(id)?.handle.lock().unwrap().clear(ClearBuffer::All)?;
    Ok(())
}

/// Waits until everything written has been sent.
#[tauri::command]
pub async fn serial_drain(serial: State<'_, Serial>, id: u32) -> Result<(), SerialError> {
    let port = serial.port(id)?;
    tauri::async_runtime::spawn_blocking(move || port.handle.lock().unwrap().flush().map_err(SerialError::from))
        .await
        .map_err(|e| SerialError::new("EIO", e.to_string()))?
}

/// What the page before this one had is gone with it.
#[tauri::command]
pub fn serial_reset(serial: State<'_, Serial>) {
    for (_, port) in serial.ports.lock().unwrap().drain() {
        port.close();
    }
}
