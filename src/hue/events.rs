use crate::config::Config;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const MAX_LINE: usize = 16 * 1024;
const MAX_EVENT: usize = 64 * 1024;

/// An authoritative resource/authentication failure must stop cached Hue output.
#[derive(Debug)]
pub struct SessionInvalid(pub &'static str);

impl std::fmt::Display for SessionInvalid {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for SessionInvalid {}

/// The connector supplies an authenticated TLS stream containing an HTTP response.
pub struct EventConnection {
    reader: Box<dyn Read + Send>,
    socket: TcpStream,
}

impl EventConnection {
    pub fn new(reader: Box<dyn Read + Send>, socket: TcpStream) -> Self {
        Self { reader, socket }
    }
}

#[derive(Debug, Clone)]
pub struct HueEventState {
    pub configuration: Value,
    pub topology_changed: bool,
    /// A stop or owner change must survive coalescing until the daemon observes it.
    pub stream_interrupted: bool,
}

impl HueEventState {
    fn apply(&mut self, events: &Value) -> Result<bool> {
        let containers = events
            .as_array()
            .ok_or_else(|| anyhow!("Hue event payload was not an array"))?;
        let mut changed = false;
        for container in containers {
            let kind = container.get("type").and_then(Value::as_str).unwrap_or("");
            if kind == "error" {
                bail!("Hue event stream reported an error");
            }
            let Some(resources) = container.get("data").and_then(Value::as_array) else {
                continue;
            };
            for resource in resources {
                let resource_type = resource.get("type").and_then(Value::as_str).unwrap_or("");
                let selected = resource.get("id") == self.configuration.get("id");
                if selected && resource_type == "entertainment_configuration" {
                    if kind == "delete" {
                        return Err(SessionInvalid("Selected Hue Entertainment configuration was deleted; select an area again").into());
                    }
                    if resource.get("channels").is_some() || resource.get("locations").is_some() {
                        self.topology_changed = true;
                    }
                    if self.configuration.get("status").and_then(Value::as_str) == Some("active")
                        && (resource.get("status").and_then(Value::as_str) == Some("inactive")
                            || resource.get("active_streamer").is_some_and(|owner| {
                                Some(owner) != self.configuration.get("active_streamer")
                            }))
                    {
                        self.stream_interrupted = true;
                    }
                    merge(&mut self.configuration, resource);
                    // Inactive events need not contain the previous streamer's removal.
                    if self.configuration.get("status").and_then(Value::as_str) == Some("inactive")
                    {
                        self.configuration
                            .as_object_mut()
                            .unwrap()
                            .remove("active_streamer");
                    }
                    changed = true;
                } else if matches!(resource_type, "entertainment" | "device" | "light")
                    && (matches!(kind, "add" | "delete")
                        || resource.get("segments").is_some()
                        || resource.get("renderer_reference").is_some())
                {
                    self.topology_changed = true;
                    changed = true;
                }
            }
        }
        Ok(changed)
    }
}

fn merge(target: &mut Value, patch: &Value) {
    if let (Some(target), Some(patch)) = (target.as_object_mut(), patch.as_object()) {
        for (key, value) in patch {
            if value.is_object() && target.get(key).is_some_and(Value::is_object) {
                merge(target.get_mut(key).unwrap(), value);
            } else {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

#[derive(Default)]
struct Pending {
    epoch: u64,
    state: Option<HueEventState>,
    error: Option<anyhow::Error>,
}

type Latest = Arc<Mutex<Pending>>;

/// A single subscription; request changes cancel the socket and invalidate old results.
pub struct HueEvents {
    commands: mpsc::Sender<(u64, Option<Config>)>,
    generation: Arc<AtomicU64>,
    latest: Latest,
    socket: Arc<Mutex<Option<TcpStream>>>,
    worker: Option<JoinHandle<()>>,
}

impl HueEvents {
    pub fn new(
        snapshot: impl Fn(&Config) -> Result<Value> + Send + 'static,
        connect: impl Fn(&Config) -> Result<EventConnection> + Send + 'static,
    ) -> Result<Self> {
        let (commands, receiver) = mpsc::channel::<(u64, Option<Config>)>();
        let generation = Arc::new(AtomicU64::new(0));
        let latest: Latest = Arc::new(Mutex::new(Pending::default()));
        let socket = Arc::new(Mutex::new(None::<TcpStream>));
        let current = generation.clone();
        let output = latest.clone();
        let active_socket = socket.clone();
        let worker = thread::Builder::new()
            .name("hue-events".into())
            .spawn(move || {
                let mut pending = None;
                while let Some(mut command) = pending.take().or_else(|| receiver.recv().ok()) {
                    for newer in receiver.try_iter() {
                        command = newer;
                    }
                    let (epoch, config) = command;
                    let Some(config) = config else { continue };
                    let mut previous_configuration: Option<Value> = None;
                    let mut retry_delay = Duration::from_secs(2);
                    while current.load(Ordering::SeqCst) == epoch {
                        let result = (|| -> Result<()> {
                            // Subscribe before the snapshot so updates during GET remain queued.
                            let connection = connect(&config)?;
                            *active_socket.lock().unwrap() = Some(connection.socket);
                            if current.load(Ordering::SeqCst) != epoch {
                                return Ok(());
                            }
                            let mut reader = EventReader::new(connection.reader)?;
                            let configuration = snapshot(&config)?;
                            if configuration.get("id").and_then(Value::as_str).is_none() {
                                bail!("Hue configuration snapshot has no resource ID");
                            }
                            let mut state = HueEventState {
                                topology_changed: previous_configuration.as_ref().is_none_or(
                                    |old| {
                                        old.get("channels") != configuration.get("channels")
                                            || old.get("locations")
                                                != configuration.get("locations")
                                    },
                                ),
                                stream_interrupted: previous_configuration.as_ref().is_some_and(
                                    |old| {
                                        old.get("status").and_then(Value::as_str) == Some("active")
                                            && (old.get("status") != configuration.get("status")
                                                || old.get("active_streamer")
                                                    != configuration.get("active_streamer"))
                                    },
                                ),
                                configuration,
                            };
                            previous_configuration = Some(state.configuration.clone());
                            publish(&output, &current, epoch, Ok(state.clone()));
                            state.topology_changed = false;
                            state.stream_interrupted = false;
                            while current.load(Ordering::SeqCst) == epoch {
                                let event = reader.next_event()?;
                                retry_delay = Duration::from_secs(2);
                                if state.apply(&event)? {
                                    previous_configuration = Some(state.configuration.clone());
                                    publish(&output, &current, epoch, Ok(state.clone()));
                                    state.topology_changed = false;
                                    state.stream_interrupted = false;
                                }
                            }
                            Ok(())
                        })();
                        if let Some(socket) = active_socket.lock().unwrap().take() {
                            let _ = socket.shutdown(Shutdown::Both);
                        }
                        if let Err(error) = result {
                            publish(&output, &current, epoch, Err(error));
                        }
                        if current.load(Ordering::SeqCst) != epoch {
                            break;
                        }
                        match receiver.recv_timeout(retry_delay) {
                            Ok(command) => {
                                pending = Some(command);
                                break;
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => return,
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                        }
                        retry_delay = (retry_delay * 2).min(Duration::from_secs(30));
                    }
                }
            })?;
        Ok(Self {
            commands,
            generation,
            latest,
            socket,
            worker: Some(worker),
        })
    }

    pub fn request(&mut self, config: Option<&Config>) {
        let epoch = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(socket) = self.socket.lock().unwrap().take() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        *self.latest.lock().unwrap() = Pending::default();
        let _ = self.commands.send((epoch, config.cloned()));
    }

    pub fn poll(&mut self) -> Option<Result<HueEventState>> {
        let mut pending = self.latest.lock().unwrap();
        if pending.epoch != self.generation.load(Ordering::SeqCst) {
            return None;
        }
        pending
            .state
            .take()
            .map(Ok)
            .or_else(|| pending.error.take().map(Err))
    }
}

fn publish(latest: &Latest, generation: &AtomicU64, epoch: u64, result: Result<HueEventState>) {
    if generation.load(Ordering::SeqCst) == epoch {
        let mut pending = latest.lock().unwrap();
        if pending.epoch != epoch {
            *pending = Pending {
                epoch,
                ..Pending::default()
            };
        }
        match result {
            Ok(mut state) => {
                if let Some(previous) = pending.state.take() {
                    state.topology_changed |= previous.topology_changed;
                    state.stream_interrupted |= previous.stream_interrupted;
                }
                pending.state = Some(state);
            }
            Err(error) => {
                if !pending
                    .error
                    .as_ref()
                    .is_some_and(|previous| previous.is::<SessionInvalid>())
                {
                    pending.error = Some(error);
                }
            }
        }
    }
}

impl Drop for HueEvents {
    fn drop(&mut self) {
        self.request(None);
        let (replacement, _) = mpsc::channel();
        drop(std::mem::replace(&mut self.commands, replacement));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn line(reader: &mut impl BufRead, max: usize) -> Result<Vec<u8>> {
    let mut result = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            bail!("Hue event stream disconnected")
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |i| i + 1);
        if result.len() + take > max {
            bail!("Hue event stream line exceeded limit")
        }
        result.extend_from_slice(&available[..take]);
        reader.consume(take);
        if result.last() == Some(&b'\n') {
            return Ok(result);
        }
    }
}

struct EventReader {
    reader: BufReader<Box<dyn Read + Send>>,
    chunked: bool,
    remaining: usize,
    separator: bool,
}

impl EventReader {
    fn new(reader: Box<dyn Read + Send>) -> Result<Self> {
        let mut reader = BufReader::new(reader);
        let status = line(&mut reader, MAX_LINE)?;
        match std::str::from_utf8(&status)?.split_whitespace().nth(1) {
            Some("200") => {}
            Some("401" | "403") => {
                return Err(
                    SessionInvalid("Hue authorization revoked; pair the bridge again").into(),
                )
            }
            Some("429") => bail!("Hue event subscription rate limited; retry later"),
            _ => bail!("Hue event subscription failed"),
        }
        let mut header_size = status.len();
        let mut chunked = false;
        let mut event_stream = false;
        loop {
            let header = line(&mut reader, MAX_LINE)?;
            header_size += header.len();
            if header_size > MAX_LINE {
                bail!("Hue event headers exceeded limit")
            }
            if header == b"\r\n" || header == b"\n" {
                break;
            }
            if let Some((name, value)) = std::str::from_utf8(&header)?.split_once(':') {
                if name.eq_ignore_ascii_case("transfer-encoding") {
                    if !value.trim().eq_ignore_ascii_case("chunked") {
                        bail!("Unsupported Hue event transfer encoding")
                    }
                    chunked = true;
                }
                if name.eq_ignore_ascii_case("content-type") {
                    event_stream = value
                        .trim()
                        .split(';')
                        .next()
                        .is_some_and(|value| value.eq_ignore_ascii_case("text/event-stream"));
                }
            }
        }
        if !event_stream {
            bail!("Hue subscription did not return an event stream")
        }
        Ok(Self {
            reader,
            chunked,
            remaining: 0,
            separator: false,
        })
    }

    fn byte(&mut self) -> Result<u8> {
        if self.chunked && self.remaining == 0 {
            if self.separator {
                let mut separator = [0; 2];
                self.reader.read_exact(&mut separator)?;
                if separator != *b"\r\n" {
                    bail!("Malformed Hue event chunk separator")
                }
            }
            let header = line(&mut self.reader, MAX_LINE)?;
            let size = std::str::from_utf8(&header)?
                .trim()
                .split(';')
                .next()
                .unwrap_or("");
            self.remaining =
                usize::from_str_radix(size, 16).context("Malformed Hue event chunk length")?;
            if self.remaining == 0 {
                bail!("Hue event stream disconnected")
            }
            self.separator = true;
        }
        let mut byte = [0];
        self.reader
            .read_exact(&mut byte)
            .context("Hue event stream disconnected")?;
        if self.chunked {
            self.remaining -= 1
        }
        Ok(byte[0])
    }

    fn next_event(&mut self) -> Result<Value> {
        let mut data = String::new();
        loop {
            let mut bytes = Vec::new();
            loop {
                let byte = self.byte()?;
                if byte == b'\n' {
                    break;
                }
                if bytes.len() == MAX_LINE {
                    bail!("Hue event stream line exceeded limit")
                }
                bytes.push(byte);
            }
            let text = std::str::from_utf8(&bytes)?.trim_end_matches('\r');
            if text.is_empty() {
                if !data.is_empty() {
                    return serde_json::from_str(&data).context("Malformed Hue event JSON");
                }
            } else if let Some(value) = text.strip_prefix("data:") {
                if data.len() + value.len() + 1 > MAX_EVENT {
                    bail!("Hue event exceeded limit")
                }
                if !data.is_empty() {
                    data.push('\n')
                }
                data.push_str(value.strip_prefix(' ').unwrap_or(value));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn partial_events_preserve_owner_detect_topology_and_clear_inactive_owner() {
        let mut state = HueEventState {
            configuration: json!({"id":"area","status":"active","active_streamer":{"rid":"ours"},"metadata":{"name":"Room"}}),
            topology_changed: false,
            stream_interrupted: false,
        };
        state.apply(&json!([{"type":"update","data":[{"id":"area","type":"entertainment_configuration","metadata":{"name":"TV"}}]}])).unwrap();
        assert_eq!(state.configuration["active_streamer"]["rid"], "ours");
        assert_eq!(state.configuration["metadata"]["name"], "TV");
        state.apply(&json!([{"type":"update","data":[{"id":"area","type":"entertainment_configuration","status":"inactive","channels":[]}]}])).unwrap();
        assert!(state.configuration.get("active_streamer").is_none());
        assert!(state.topology_changed);
        assert!(state.stream_interrupted);
        let error = state.apply(&json!([{"type":"delete","data":[{"id":"area","type":"entertainment_configuration"}]}])).unwrap_err();
        assert!(error.is::<SessionInvalid>());
    }

    #[test]
    fn chunked_events_cross_chunk_boundaries_and_disconnect_is_an_error() {
        let wire = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n8\r\ndata: [{\r\n7\r\n}]\n\n: x\r\n0\r\n\r\n";
        let mut reader = EventReader::new(Box::new(std::io::Cursor::new(wire.to_vec()))).unwrap();
        assert_eq!(reader.next_event().unwrap(), json!([{}]));
        assert!(reader.next_event().is_err());
        for status in [401, 403] {
            let response = format!("HTTP/1.1 {status} Unauthorized\r\n\r\n");
            let error = EventReader::new(Box::new(std::io::Cursor::new(response.into_bytes())))
                .err()
                .unwrap();
            assert!(error.is::<SessionInvalid>());
        }
        let oversized = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {}\n\n",
            "x".repeat(MAX_LINE + 1)
        );
        assert!(
            EventReader::new(Box::new(std::io::Cursor::new(oversized.into_bytes())))
                .unwrap()
                .next_event()
                .is_err()
        );
    }

    #[test]
    fn old_generation_cannot_publish_state() {
        let latest: Latest = Arc::new(Mutex::new(Pending::default()));
        let generation = AtomicU64::new(2);
        publish(&latest, &generation, 1, Err(anyhow!("stale")));
        assert!(latest.lock().unwrap().error.is_none());
        publish(&latest, &generation, 2, Err(anyhow!("current")));
        assert!(latest.lock().unwrap().error.is_some());
        publish(
            &latest,
            &generation,
            2,
            Err(SessionInvalid("deleted").into()),
        );
        publish(&latest, &generation, 2, Err(anyhow!("transient")));
        assert!(latest
            .lock()
            .unwrap()
            .error
            .as_ref()
            .unwrap()
            .is::<SessionInvalid>());
        publish(
            &latest,
            &generation,
            2,
            Ok(HueEventState {
                configuration: json!({"id":"area","status":"inactive"}),
                topology_changed: true,
                stream_interrupted: true,
            }),
        );
        publish(
            &latest,
            &generation,
            2,
            Ok(HueEventState {
                configuration: json!({"id":"area","status":"active"}),
                topology_changed: false,
                stream_interrupted: false,
            }),
        );
        let pending = latest.lock().unwrap();
        let state = pending.state.as_ref().unwrap();
        assert!(state.topology_changed && state.stream_interrupted);
        assert_eq!(state.configuration["status"], "active");
        assert!(pending.error.is_some());
        drop(pending);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        std::io::Write::write_all(
            &mut server,
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n",
        )
        .unwrap();
        let client = Mutex::new(Some(client));
        let mut worker = HueEvents::new(
            |_| Ok(json!({"id":"area","status":"inactive"})),
            move |_| {
                let stream = client.lock().unwrap().take().unwrap();
                let cancellation = stream.try_clone()?;
                Ok(EventConnection::new(Box::new(stream), cancellation))
            },
        )
        .unwrap();
        worker.request(Some(&Config::new_default("", "", "", "")));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let first = loop {
            if let Some(state) = worker.poll() {
                break state.unwrap();
            }
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        };
        assert!(first.topology_changed);
        worker.request(None);
        drop(worker);
        server
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        assert_eq!(server.read(&mut [0]).unwrap(), 0);
    }
}
