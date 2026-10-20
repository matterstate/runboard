use crate::app::{self, Settings};
use async_nats::jetstream::{self, consumer, stream};
use futures_util::StreamExt;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, watch, Semaphore};

pub const EVENT_BYTES_MAX: usize = 48 << 10;
pub const CLIENTS_MAX: usize = 64;

#[derive(Clone)]
pub struct Options {
    pub state_dir: Option<PathBuf>,
    pub url: Option<String>,
    pub port: u16,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            state_dir: None,
            url: None,
            port: 4223,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Event {
    pub sequence: u64,
    pub data: Arc<str>,
}

pub struct Live {
    pub events: broadcast::Sender<Event>,
    pub status: watch::Sender<&'static str>,
    pub clients: Arc<Semaphore>,
    pub consumer_ready: AtomicBool,
    client: async_nats::Client,
    context: jetstream::Context,
    stream: stream::Stream,
    pub subject: String,
    pub state_dir: PathBuf,
    consumer_name: String,
    entity_prefixes: Vec<String>,
    child: Option<tokio::sync::Mutex<Child>>,
    _lock: File,
}

pub fn hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(14695981039346656037, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(1099511628211)
    })
}

fn write_state(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = path.with_extension("tmp");
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> std::io::Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|error| format!("State write: {error}"))
}

impl Live {
    pub async fn start(settings: &Settings) -> Result<Arc<Self>, String> {
        let identity = settings
            .projects
            .iter()
            .map(|project| format!("{}={}\n", project.name, project.root.display()))
            .collect::<String>();
        let project_scope = format!("{:016x}", hash(identity.as_bytes()));
        let directory = match &settings.live.state_dir {
            Some(directory) => directory.clone(),
            None => {
                let base = std::env::var_os("XDG_STATE_HOME")
                    .map(PathBuf::from)
                    .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
                    .ok_or("set --state-dir or XDG_STATE_HOME")?;
                base.join("runboard").join(&project_scope)
            }
        };
        fs::create_dir_all(&directory).map_err(|error| format!("State directory: {error}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).map_err(|error| error.to_string())?;
        }
        let directory = directory.canonicalize().map_err(|error| error.to_string())?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("lock"))
            .map_err(|error| error.to_string())?;
        lock.try_lock()
            .map_err(|_| "another runboard instance owns this state directory")?;
        let consumer_path = directory.join("consumer.id");
        let consumer_id = match crate::data::read_text(&consumer_path, 64) {
            Some(id) if id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()) => id,
            _ if !consumer_path.exists() => {
                let mut bytes = [0u8; 16];
                getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
                let id = bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
                write_state(&consumer_path, id.as_bytes())?;
                id
            }
            _ => return Err("invalid consumer.id in state directory".into()),
        };
        let scope = if settings.live.url.is_some() {
            project_scope
        } else {
            consumer_id[..16].to_string()
        };
        let mut child = None;
        let url = match &settings.live.url {
            Some(url) => url.clone(),
            None => {
                let token_path = directory.join("nats.token");
                let token_bytes = File::open(&token_path).and_then(|file| {
                    let mut bytes = Vec::new();
                    file.take(65).read_to_end(&mut bytes).map(|_| bytes)
                });
                let token = match token_bytes {
                    Ok(bytes) if bytes.len() == 64 && bytes.iter().all(u8::is_ascii_hexdigit) => {
                        String::from_utf8(bytes).map_err(|error| error.to_string())?
                    }
                    Ok(_) => return Err("invalid nats.token in state directory".into()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        let mut bytes = [0u8; 32];
                        getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
                        let token = bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
                        write_state(&token_path, token.as_bytes())?;
                        token
                    }
                    Err(error) => return Err(error.to_string()),
                };
                let config = json!({"host":"127.0.0.1","port":settings.live.port,"server_name":format!("runboard-{scope}"),
                    "max_payload":65536,"max_connections":128,"authorization":{"users":[{"user":"runboard","password":token}]},
                    "jetstream":{"store_dir":directory.join("jetstream"),"max_file_store":1_073_741_824u64,"max_memory_store":67_108_864}});
                let config_path = directory.join("nats.conf");
                write_state(&config_path, config.to_string().as_bytes())?;
                let url = format!("nats://runboard:{token}@127.0.0.1:{}", settings.live.port);
                write_state(&directory.join("nats.url"), url.as_bytes())?;
                child = Some(
                    Command::new(option_env!("RUNBOARD_NATS_SERVER").unwrap_or("nats-server"))
                        .arg("-c")
                        .arg(config_path)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::inherit())
                        .kill_on_drop(true)
                        .spawn()
                        .map_err(|error| format!("Cannot start bundled nats-server: {error}"))?,
                );
                url
            }
        };
        let mut client = None;
        let address = url
            .parse::<async_nats::ServerAddr>()
            .map_err(|_| "invalid NATS endpoint")?;
        let mut options = async_nats::ConnectOptions::new()
            .name("runboard")
            .connection_timeout(Duration::from_millis(500))
            .client_capacity(256)
            .subscription_capacity(256);
        if let Some(user) = address.username() {
            let user = percent_encoding::percent_decode_str(user)
                .decode_utf8()
                .map_err(|_| "invalid NATS credentials")?
                .into_owned();
            options = match address.password() {
                Some(password) => options.user_and_password(
                    user,
                    percent_encoding::percent_decode_str(password)
                        .decode_utf8()
                        .map_err(|_| "invalid NATS credentials")?
                        .into_owned(),
                ),
                None => options.token(user),
            };
        }
        for _ in 0..50 {
            if let Some(child) = &mut child {
                if child.try_wait().map_err(|error| error.to_string())?.is_some() {
                    return Err("NATS sidecar exited during startup; check the port and server output".into());
                }
            }
            match options.clone().connect(address.clone()).await {
                Ok(connection) => {
                    if settings.live.url.is_none()
                        && connection.server_info().server_name != format!("runboard-{scope}")
                    {
                        return Err("managed NATS port is occupied by another server; choose --nats-port".into());
                    }
                    client = Some(connection);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        let client = client.ok_or("NATS did not become ready; check the endpoint and credentials")?;
        let mut context = jetstream::new(client.clone());
        context.set_timeout(Duration::from_secs(3));
        let subject = format!("runboard.{scope}");
        let stream_name = format!("RUNBOARD_{}", scope.to_ascii_uppercase());
        let stream = context
            .get_or_create_stream(stream::Config {
                name: stream_name,
                subjects: vec![format!("{subject}.>")],
                max_bytes: 536_870_912,
                max_messages: 1_000_000,
                max_age: Duration::from_secs(86400),
                max_message_size: EVENT_BYTES_MAX as i32,
                max_consumers: 8,
                storage: stream::StorageType::File,
                discard: stream::DiscardPolicy::Old,
                ..Default::default()
            })
            .await
            .map_err(|error| format!("Runboard stream: {error}"))?;
        let config = &stream.cached_info().config;
        if config.subjects != [format!("{subject}.>")]
            || config.max_bytes <= 0
            || config.max_bytes > 536_870_912
            || config.max_age.is_zero()
            || config.max_age > Duration::from_secs(86400)
            || config.max_messages <= 0
            || config.max_messages > 1_000_000
            || config.max_message_size <= 0
            || config.max_message_size > 65536
            || config.storage != stream::StorageType::File
        {
            return Err("existing runboard stream has incompatible subjects or unbounded retention".into());
        }
        let (events, _) = broadcast::channel(256);
        let (status, _) = watch::channel("connecting");
        let mut entity_prefixes = settings
            .projects
            .iter()
            .map(|project| format!("{}/", project.name))
            .collect::<Vec<_>>();
        if settings.queue.is_some() || settings.pueue.is_some() {
            entity_prefixes.push("shared/".into());
            entity_prefixes.push("queue/".into());
        }
        Ok(Arc::new(Self {
            events,
            status,
            clients: Arc::new(Semaphore::new(CLIENTS_MAX)),
            consumer_ready: AtomicBool::new(false),
            client,
            context,
            stream,
            subject,
            state_dir: directory,
            consumer_name: format!("dashboard_{consumer_id}"),
            entity_prefixes,
            child: child.map(tokio::sync::Mutex::new),
            _lock: lock,
        }))
    }

    pub fn connection_status(&self) -> &'static str {
        if !self.consumer_ready.load(Ordering::Relaxed) {
            return "reconnecting";
        }
        if self.client.connection_state() != async_nats::connection::State::Connected {
            return "reconnecting";
        }
        *self.status.borrow()
    }

    pub async fn sidecar_alive(&self) -> bool {
        match &self.child {
            Some(child) => matches!(child.lock().await.try_wait(), Ok(None)),
            None => true,
        }
    }

    pub async fn publish(&self, key: &str, data: String, id: &str) -> Result<u64, String> {
        if data.len() > EVENT_BYTES_MAX {
            return Err("live event exceeds 48 KiB".into());
        }
        let mut headers = async_nats::HeaderMap::new();
        headers.insert("Nats-Msg-Id", id);
        let subject = format!("{}.{:016x}", self.subject, hash(key.as_bytes()));
        let acknowledgement = self
            .context
            .publish_with_headers(subject, headers, data.into())
            .await
            .map_err(|error| error.to_string())?
            .await
            .map_err(|error| error.to_string())?;
        Ok(acknowledgement.sequence)
    }

    pub async fn consume(self: Arc<Self>) -> Result<(), String> {
        let consumer = self
            .stream
            .get_or_create_consumer(
                &self.consumer_name,
                consumer::pull::Config {
                    durable_name: Some(self.consumer_name.clone()),
                    ack_policy: consumer::AckPolicy::Explicit,
                    max_ack_pending: 64,
                    max_batch: 64,
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        let mut messages = consumer
            .stream()
            .max_messages_per_batch(64)
            .max_bytes_per_batch(4 << 20)
            .messages()
            .await
            .map_err(|error| error.to_string())?;
        self.consumer_ready.store(true, Ordering::Relaxed);
        while let Some(message) = messages.next().await {
            let message = message.map_err(|error| error.to_string())?;
            let sequence = message.info().map_err(|error| error.to_string())?.stream_sequence;
            if message.payload.len() <= EVENT_BYTES_MAX {
                if let Ok(data) = std::str::from_utf8(&message.payload) {
                    let permitted = serde_json::from_str::<serde_json::Value>(data)
                        .ok()
                        .and_then(|value| {
                            value["entity"]
                                .as_str()
                                .map(|entity| self.entity_prefixes.iter().any(|prefix| entity.starts_with(prefix)))
                        })
                        .unwrap_or(false);
                    if permitted {
                        let _ = self.events.send(Event {
                            sequence,
                            data: data.into(),
                        });
                    }
                }
            }
            message.ack().await.map_err(|error| error.to_string())?;
        }
        Err("NATS consumer stopped".into())
    }

    pub async fn bridge(self: Arc<Self>, settings: Arc<Settings>) -> Result<(), String> {
        let mut known = BTreeMap::new();
        let mut known_errors = BTreeSet::new();
        let mut pending: Option<(String, u64, String, String)> = None;
        let mut revision = 0u64;
        let epoch = crate::html::now().to_bits();
        loop {
            let source_settings = settings.clone();
            let previous = known.clone();
            let sources = tokio::task::spawn_blocking(move || app::live_sources(&source_settings, &previous))
                .await
                .map_err(|error| error.to_string())?;
            match sources {
                Ok(sources) => {
                    let present = sources.iter().map(|source| source.key.clone()).collect::<BTreeSet<_>>();
                    let removed = known
                        .keys()
                        .filter(|key| !present.contains(*key))
                        .cloned()
                        .collect::<Vec<_>>();
                    let mut healthy = true;
                    for source in sources {
                        if known.get(&source.key) == Some(&source.revision) {
                            continue;
                        }
                        revision += 1;
                        let mut value = source.data;
                        let mut data = json!({"entity":source.key,"revision":source.revision,"time":crate::html::now(),"data":value}).to_string();
                        if data.len() > EVENT_BYTES_MAX {
                            value = json!({"kind":"input-error","error":"Event exceeds 48 KiB; read full data from the snapshot"});
                            data = json!({"entity":source.key,"revision":source.revision,"time":crate::html::now(),"data":value}).to_string();
                        }
                        let (id, data) = match &pending {
                            Some((key, revision, id, data)) if key == &source.key && *revision == source.revision => {
                                (id.clone(), data.clone())
                            }
                            _ => (format!("{epoch}-{revision}"), data),
                        };
                        pending = Some((source.key.clone(), source.revision, id.clone(), data.clone()));
                        match self.publish(&source.key, data, &id).await {
                            Ok(_) => {
                                if value["kind"] == "input-error" {
                                    known_errors.insert(source.key.clone());
                                } else {
                                    known_errors.remove(&source.key);
                                }
                                known.insert(source.key, source.revision);
                                pending = None;
                            }
                            Err(error) => {
                                eprintln!("Live: publish failed: {error}");
                                healthy = false;
                                break;
                            }
                        }
                    }
                    if healthy {
                        for key in removed {
                            revision += 1;
                            let data = json!({"entity":key,"time":crate::html::now(),"removed":true}).to_string();
                            if self
                                .publish(
                                    &key,
                                    data,
                                    &format!("{epoch}-remove-{}-{}", hash(key.as_bytes()), known[&key]),
                                )
                                .await
                                .is_err()
                            {
                                healthy = false;
                                break;
                            }
                            known.remove(&key);
                            known_errors.remove(&key);
                        }
                    }
                    self.status.send_replace(if !healthy {
                        "reconnecting"
                    } else if known_errors.is_empty() {
                        "live"
                    } else {
                        "input-error"
                    });
                }
                Err(error) => {
                    eprintln!("Live: input scan failed: {error}");
                    self.status.send_replace("input-error");
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    pub async fn shutdown(&self) {
        if tokio::time::timeout(Duration::from_secs(2), self.client.drain())
            .await
            .is_err()
        {
            eprintln!("Live: NATS drain timed out");
        }
        if let Some(child) = &self.child {
            let mut child = child.lock().await;
            if let Some(id) = child.id() {
                let signal = Command::new(option_env!("RUNBOARD_NATS_SERVER").unwrap_or("nats-server"))
                    .args(["--signal", &format!("quit={id}")])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
                if !matches!(signal,Ok(status) if status.success()) {
                    let _ = child.start_kill();
                }
            }
            if tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn settings() -> (PathBuf, Settings) {
        let root = std::env::temp_dir().join(format!(
            "runboard-nats-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("reports")).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let settings = crate::arguments(vec![
            "--root".into(),
            format!("demo={}", root.display()),
            "--state-dir".into(),
            root.join("state").display().to_string(),
            "--nats-port".into(),
            port.to_string(),
        ])
        .unwrap();
        (root, settings)
    }

    #[tokio::test]
    async fn sidecar_persists_deduplicates_and_replays_events() {
        let (root, settings) = settings();
        let live = Live::start(&settings).await.unwrap();
        assert!(live.sidecar_alive().await);
        assert!(Live::start(&settings).await.is_err());
        assert!(async_nats::ConnectOptions::new()
            .connection_timeout(Duration::from_secs(1))
            .connect(format!("nats://127.0.0.1:{}", settings.live.port))
            .await
            .is_err());
        let data = json!({"entity":"demo/run/test","data":{"metrics":{"loss":1.0}}}).to_string();
        let sequence = live.publish("demo/run/test", data.clone(), "test-one").await.unwrap();
        assert_eq!(sequence, live.publish("demo/run/test", data, "test-one").await.unwrap());
        let last = live
            .publish(
                "outside/run/private",
                json!({"entity":"outside/run/private","data":"private"}).to_string(),
                "outside",
            )
            .await
            .unwrap();
        assert!(live
            .publish("x", "x".repeat(EVENT_BYTES_MAX + 1), "oversized")
            .await
            .is_err());
        live.shutdown().await;
        drop(live);
        let live = Live::start(&settings).await.unwrap();
        assert_eq!(live.stream.cached_info().state.last_sequence, last);
        let mut receiver = live.events.subscribe();
        let consumer = tokio::spawn(live.clone().consume());
        let event = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.sequence, sequence);
        assert!(event.data.contains("loss"));
        assert!(tokio::time::timeout(Duration::from_millis(200), receiver.recv())
            .await
            .is_err());
        consumer.abort();
        let _ = consumer.await;
        live.shutdown().await;
        drop(live);
        assert!(std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, settings.live.port)).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn external_mode_reuses_broker_without_owning_its_lifecycle() {
        let (root, settings) = settings();
        let managed = Live::start(&settings).await.unwrap();
        let url = fs::read_to_string(root.join("state/nats.url")).unwrap();
        let mut external = crate::arguments(vec![
            "--root".into(),
            format!("other={}", root.display()),
            "--state-dir".into(),
            root.join("external").display().to_string(),
            "--nats-url".into(),
            url,
        ])
        .unwrap();
        let live = Live::start(&external).await.unwrap();
        assert!(live.child.is_none());
        assert_ne!(live.subject, managed.subject);
        live.shutdown().await;
        drop(live);
        assert!(managed.sidecar_alive().await);
        external.live.url = None;
        external.live.port = settings.live.port;
        assert!(Live::start(&external).await.is_err());
        assert!(managed.sidecar_alive().await);
        managed.shutdown().await;
        drop(managed);
        fs::remove_dir_all(root).unwrap();
    }
}
