use std::{
    collections::{HashMap, VecDeque},
    env, fs, io,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use crate::wire::{
    MEMORY_OFF, MemoryChange, MemoryConsentState, MemoryIndexState, MemoryLearningState,
    MemoryNote, MemoryNoteCounts, MemorySeed, MemoryStatus, Notification, NotificationEvent,
    SandboxState, SandboxStatus, UNAUTHORIZED, UNKNOWN_NOTE, Usage, truncate_preview,
};
use anyhow::Context as _;
use elefante_core::sandbox::Sandbox;
use elefante_core::tools::ToolExecutor;
use elefante_core::{
    ApprovalOutcome, ApprovalSink, Note as CoreNote, OutputEvent, RunConfig, Session, ToolGate,
    learn::{
        ChangedNote, ENV_MODEL, LearnChange, LearnConfig, LearnFailure, LearnHealth, LearnSink,
        Learner,
    },
    memory::{MemContext, MemSearchTool},
};
use elefante_mem::{
    Indexer, MemKey, RepoId, Store, StoreDir,
    consent::{self, CONSENT_FILE, ConsentAnswer, ConsentRecord},
    indexer::IndexOptions,
    learn,
    retrieve::Retriever,
    schema::open_memory_reader,
    trust::KEY_FILE,
};
use elefante_provider::{Message, OpenRouterProvider, Provider};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    net::{
        UnixListener, UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::oneshot,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

#[cfg(debug_assertions)]
use super::stub::StubProvider;
use super::{
    MAX_LINE_BYTES, RING_CAPACITY, credentials, queue::ClientQueue, search::ProviderSearch,
};

/// Startup configuration captured once, rather than reading mutable process state per turn.
#[derive(Clone)]
pub struct DaemonConfig {
    pub socket: PathBuf,
    pub credentials: PathBuf,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub stub_provider: Option<PathBuf>,
    pub idle_timeout: Duration,
    /// Index repositories and give sessions context from them. `ELEFANTE_MEMORY=0` turns it all
    /// off: no store is opened, nothing is written under `.elefante/`, no context is added.
    pub memory: bool,
    /// Whether learning is disabled, explicitly enabled, or waits for file-backed consent.
    pub learn: LearnMode,
    /// How long after a session's last turn anything the learner is still holding is extracted.
    pub learn_idle: Duration,
    /// How long one extraction call may take before it counts as a failure.
    pub learn_timeout: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LearnMode {
    Off,
    Ask,
    On,
}

/// Resolve the test override, runtime directory, then the home-directory fallback.
///
/// # Errors
/// Returns an error when the fallback needs an unavailable home directory.
pub fn socket_path() -> anyhow::Result<PathBuf> {
    if let Some(path) = env::var_os("ELEFANTE_SOCKET") {
        return Ok(path.into());
    }
    if let Some(path) = env::var_os("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(path).join("elefante.sock"));
    }
    Ok(home()?.join(".elefante/run/elefante.sock"))
}

/// The daemon writes its token beside its socket: `~/.elefante/run/token` by default.
#[must_use]
pub fn token_path(socket: &Path) -> PathBuf {
    socket.with_file_name("token")
}

fn home() -> anyhow::Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// An on/off environment switch: on unless explicitly `0`.
fn switch(value: Option<&str>) -> bool {
    value.is_none_or(|value| value.trim() != "0")
}

fn learn_mode(memory: bool, value: Option<&str>) -> LearnMode {
    if !memory {
        return LearnMode::Off;
    }
    match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("1" | "on") => LearnMode::On,
        Some("0" | "off") => LearnMode::Off,
        None | Some(_) => LearnMode::Ask,
    }
}

impl DaemonConfig {
    /// Capture and validate daemon environment settings.
    ///
    /// # Errors
    /// Rejects invalid idle durations, unavailable home paths, and non-isolated stubs.
    pub fn from_env() -> anyhow::Result<Self> {
        // The scripted provider exists for tests. A release build never reads the variable, so it
        // cannot be switched to a script by its environment.
        #[cfg(debug_assertions)]
        let stub_provider = env::var_os("ELEFANTE_STUB_PROVIDER").map(PathBuf::from);
        #[cfg(not(debug_assertions))]
        let stub_provider: Option<PathBuf> = None;
        anyhow::ensure!(
            stub_provider.is_none() || env::var_os("ELEFANTE_SOCKET").is_some(),
            "ELEFANTE_STUB_PROVIDER requires ELEFANTE_SOCKET"
        );
        let memory = switch(env::var("ELEFANTE_MEMORY").ok().as_deref());
        Ok(Self {
            socket: socket_path()?,
            credentials: home()?.join(".elefante/credentials.toml"),
            api_key: env::var("OPENROUTER_API_KEY")
                .ok()
                .filter(|key| !key.trim().is_empty()),
            base_url: env::var("OPENROUTER_BASE_URL").ok(),
            stub_provider,
            idle_timeout: Duration::from_secs(
                env::var("ELEFANTE_IDLE_SECS")
                    .map_or(Ok(900), |value| value.parse())
                    .context("invalid ELEFANTE_IDLE_SECS")?,
            ),
            memory,
            learn: learn_mode(memory, env::var("ELEFANTE_LEARN").ok().as_deref()),
            learn_idle: Duration::from_secs(
                env::var("ELEFANTE_LEARN_IDLE_SECS")
                    .map_or(Ok(300), |value| value.parse())
                    .context("invalid ELEFANTE_LEARN_IDLE_SECS")?,
            ),
            learn_timeout: Duration::from_secs(60),
        })
    }
}

/// Start the daemon using its process environment.
///
/// # Errors
/// Returns configuration, credential, provider setup, or socket errors.
pub async fn run_from_env() -> anyhow::Result<()> {
    run(DaemonConfig::from_env()?).await
}

struct SocketGuard {
    path: PathBuf,
    inode: u64,
    directory: fs::File,
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        // Lock the existing directory, not a lock file. Kernel ownership survives
        // neither process death nor descriptor close, so crashes cannot wedge startup.
        let Ok(()) = self.directory.lock() else {
            return;
        };
        if fs::symlink_metadata(&self.path).is_ok_and(|metadata| metadata.ino() == self.inode) {
            let _ = fs::remove_file(&self.path);
        }
        let _ = self.directory.unlock();
    }
}

/// Binds the socket and, just before, writes `token` for its clients. Only the starter that wins
/// the directory lock writes, so a losing starter never replaces a live daemon's token.
async fn bind(path: &Path, token: &str) -> anyhow::Result<Option<(UnixListener, SocketGuard)>> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .context("socket path needs a parent directory")?;
    credentials::private_directory(parent)?;
    let directory = fs::File::open(parent)?;
    loop {
        match directory.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(fs::TryLockError::Error(error)) => return Err(error.into()),
        }
    }
    // All cooperating starters and normal shutdowns serialize probe/unlink/bind.
    // Keep this section synchronous: cancellation must not leave an unguarded socket.
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
            // Never unlink arbitrary files or symlinks at the configured pathname.
            if let Ok(metadata) = fs::symlink_metadata(path) {
                anyhow::ensure!(
                    metadata.file_type().is_socket(),
                    "stale socket path is not a socket"
                );
                match fs::remove_file(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    credentials::write_private(&token_path(path), token)?;
    match UnixListener::bind(path) {
        Ok(listener) => {
            let guard = SocketGuard {
                path: path.to_owned(),
                inode: fs::symlink_metadata(path)?.ino(),
                directory,
            };
            guard.directory.unlock()?;
            Ok(Some((listener, guard)))
        }
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            directory.unlock()?;
            for _ in 0..20 {
                if UnixStream::connect(path).await.is_ok() {
                    return Ok(None);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error.into())
        }
        Err(error) => Err(error.into()),
    }
}

/// One repository's index: opened lazily the first time a session starts beneath it, and shared by
/// every session after that, so they all attach to one warm store and one indexer.
struct RepoMemory {
    store: Arc<Store>,
    indexer: Arc<Indexer>,
    retriever: Arc<Retriever>,
}

impl RepoMemory {
    fn open(repo: &RepoId, key: MemKey) -> Result<Self, elefante_mem::MemError> {
        let store = Arc::new(Store::open_with_key(repo, key)?);
        Ok(Self {
            indexer: Indexer::new(
                Arc::clone(&store),
                repo.root().to_path_buf(),
                IndexOptions::default(),
            ),
            retriever: Arc::new(Retriever::new(Arc::clone(&store))),
            store,
        })
    }
}

/// What was last read from the consent file, and when the file was last seen to change.
#[derive(Default)]
struct ConsentCache {
    answers: HashMap<String, ConsentAnswer>,
    seen: Option<(std::time::SystemTime, u64)>,
}

/// One answer per extractor model. The file can be rewritten by `elefante memory learn` while no
/// daemon is listening on its socket, so it is looked at again whenever it has changed on disk.
struct ConsentGate {
    mode: LearnMode,
    path: PathBuf,
    key: Option<MemKey>,
    cache: std::cell::RefCell<ConsentCache>,
    revision: std::cell::Cell<u64>,
}

impl ConsentGate {
    fn new(config: &DaemonConfig) -> Self {
        let path = config.credentials.parent().map_or_else(
            || PathBuf::from(CONSENT_FILE),
            |parent| parent.join(CONSENT_FILE),
        );
        let key = (config.memory && config.learn != LearnMode::Off)
            .then(|| memory_key(config))
            .transpose()
            .unwrap_or_else(|error| {
                eprintln!("elefante: cannot read memory consent: {error}");
                None
            });
        let gate = Self {
            mode: config.learn,
            path,
            key,
            cache: std::cell::RefCell::new(ConsentCache::default()),
            revision: std::cell::Cell::new(0),
        };
        gate.sync();
        gate.revision.set(0);
        gate
    }

    /// Bumped whenever an answer changes, by this daemon or by a write to the file.
    fn revision(&self) -> u64 {
        self.sync();
        self.revision.get()
    }

    /// Re-reads the file if it changed since it was last read.
    fn sync(&self) {
        let Some(key) = &self.key else {
            return;
        };
        let seen = fs::metadata(&self.path)
            .ok()
            .and_then(|meta| meta.modified().ok().map(|time| (time, meta.len())));
        let mut cache = self.cache.borrow_mut();
        if cache.seen == seen {
            return;
        }
        let answers: HashMap<String, ConsentAnswer> = consent::read_all(&self.path, key)
            .into_iter()
            .map(|record| (record.extractor, record.answer))
            .collect();
        if answers != cache.answers {
            self.revision.set(self.revision.get().wrapping_add(1));
        }
        cache.answers = answers;
        cache.seen = seen;
    }

    fn seed(&self, extractor: Option<&str>) -> MemorySeed {
        self.sync();
        match (self.mode, extractor) {
            (LearnMode::Off, _) => MemorySeed {
                consent: MemoryConsentState::Off,
                extractor: None,
            },
            (LearnMode::On, Some(extractor)) => MemorySeed {
                consent: MemoryConsentState::Granted,
                extractor: Some(extractor.to_owned()),
            },
            (LearnMode::Ask, Some(extractor)) => {
                let consent = match self.cache.borrow().answers.get(extractor).copied() {
                    Some(ConsentAnswer::Learn) => MemoryConsentState::Granted,
                    Some(ConsentAnswer::Off) => MemoryConsentState::Declined,
                    None => MemoryConsentState::Unset,
                };
                MemorySeed {
                    consent,
                    extractor: Some(extractor.to_owned()),
                }
            }
            (_, None) => MemorySeed {
                consent: MemoryConsentState::Unset,
                extractor: None,
            },
        }
    }

    fn answer(
        &mut self,
        answer: ConsentAnswer,
        extractor: &str,
    ) -> Result<MemoryConsentState, elefante_mem::MemError> {
        if self.key.is_none() {
            let directory = self
                .path
                .parent()
                .ok_or_else(|| elefante_mem::MemError::Refused {
                    path: self.path.clone(),
                    reason: "there is no directory to keep the memory key in",
                })?;
            self.key = Some(MemKey::load_or_create(&directory.join(KEY_FILE))?);
        }
        let key = self
            .key
            .as_ref()
            .ok_or_else(|| elefante_mem::MemError::Refused {
                path: self.path.clone(),
                reason: "the memory key is unavailable",
            })?;
        let record = ConsentRecord {
            answer,
            extractor: extractor.to_owned(),
        };
        consent::write(&self.path, key, &record)?;
        // `sync` sees the file as changed and bumps the revision once, whatever else it read.
        self.sync();
        Ok(self.seed(Some(extractor)).consent)
    }

    fn granted(&self, extractor: Option<&str>) -> bool {
        self.seed(extractor).consent == MemoryConsentState::Granted
    }
}

struct Shared {
    config: DaemonConfig,
    provider: Mutex<Option<Arc<dyn Provider>>>,
    sessions: Mutex<HashMap<String, Arc<SessionEntry>>>,
    memories: Mutex<HashMap<PathBuf, Arc<RepoMemory>>>,
    consent: Mutex<ConsentGate>,
    standalone_extractor: String,
    /// The user's own Git identity, read once at start; `None` denies harness Git writes.
    git_identity: Option<elefante_guard::git::Identity>,
    next_id: AtomicU64,
    instance_id: String,
    clients: AtomicUsize,
    turns: AtomicUsize,
}

fn random_bits() -> io::Result<[u8; 16]> {
    use std::io::Read as _;
    let mut bytes = [0_u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn instance_id() -> io::Result<String> {
    Ok(u128::from_ne_bytes(random_bits()?).to_string())
}

fn new_token() -> io::Result<String> {
    Ok(format!("{:032x}", u128::from_ne_bytes(random_bits()?)))
}

fn standalone_extractor() -> String {
    [ENV_MODEL, "OPENROUTER_MODEL"]
        .into_iter()
        .find_map(|name| env::var(name).ok().filter(|value| !value.trim().is_empty()))
        .unwrap_or_else(|| "anthropic/claude-sonnet-5".to_owned())
}

fn make_provider(
    config: &DaemonConfig,
    stored_key: Option<String>,
) -> anyhow::Result<Option<Arc<dyn Provider>>> {
    if let Some(path) = &config.stub_provider {
        #[cfg(debug_assertions)]
        return Ok(Some(Arc::new(StubProvider::load(path)?)));
        #[cfg(not(debug_assertions))]
        anyhow::bail!(
            "a scripted provider ({}) needs a debug build",
            path.display()
        );
    }
    let Some(key) = config.api_key.clone().or(stored_key) else {
        return Ok(None);
    };
    let mut provider = OpenRouterProvider::new(key);
    if let Some(base_url) = &config.base_url {
        provider = provider
            .with_base_url(base_url)
            .context("OPENROUTER_BASE_URL is invalid")?;
    }
    Ok(Some(Arc::new(provider)))
}

/// Reads `user.name` and `user.email` from the user's global Git config files with the hardened
/// invocation. `~/.gitconfig` wins over `$XDG_CONFIG_HOME/git/config`, as in Git itself.
async fn git_identity() -> Option<elefante_guard::git::Identity> {
    let home = home().ok()?;
    let xdg = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    git_identity_in(&home, xdg).await
}

async fn git_identity_in(
    home: &Path,
    xdg_config: Option<PathBuf>,
) -> Option<elefante_guard::git::Identity> {
    let git = elefante_core::sandbox::binary("git").ok()?;
    let xdg = xdg_config
        .unwrap_or_else(|| home.join(".config"))
        .join("git/config");
    let files = [home.join(".gitconfig"), xdg];
    let mut values = Vec::new();
    for key in ["user.name", "user.email"] {
        let mut value = None;
        for file in files.iter().filter(|file| file.is_file()) {
            let invocation = elefante_guard::git::hardened(&git, home);
            let mut command = tokio::process::Command::new(&invocation.program);
            command
                .args(&invocation.args)
                .args(["config", "--includes", "--file"])
                .arg(file)
                .args(["--get", key])
                .current_dir(&invocation.cwd)
                .env_clear()
                .envs(&invocation.environment);
            let output = elefante_core::sandbox::run_process(command, Duration::from_secs(10))
                .await
                .ok()?;
            let text = output.content.trim();
            if output.success && !text.is_empty() {
                value = Some(text.to_owned());
                break;
            }
        }
        values.push(value?);
    }
    let email = values.pop()?;
    let name = values.pop()?;
    Some(elefante_guard::git::Identity { name, email })
}

fn sandbox_status(sandbox: &Sandbox) -> SandboxStatus {
    let (state, detail) = match sandbox {
        Sandbox::Seatbelt => (SandboxState::On, "seatbelt".to_owned()),
        Sandbox::Bubblewrap { .. } => (SandboxState::On, "bubblewrap".to_owned()),
        Sandbox::Disabled => (SandboxState::Off, "ELEFANTE_SANDBOX=0".to_owned()),
        Sandbox::Unavailable(reason) => (SandboxState::Unavailable, reason.clone()),
    };
    SandboxStatus { state, detail }
}

/// Serve until no clients and no running turns remain for the idle timeout.
///
/// # Errors
/// Returns startup configuration, private-directory, or socket listener errors.
pub async fn run(config: DaemonConfig) -> anyhow::Result<()> {
    // A live daemon wins even if this starter's environment is invalid. Cold
    // startup must finish validation before bind advertises readiness to clients.
    if UnixStream::connect(&config.socket).await.is_ok() {
        return Ok(());
    }
    let stored_key = if config.api_key.is_none() && config.stub_provider.is_none() {
        credentials::read(&config.credentials)?
    } else {
        None
    };
    let provider = make_provider(&config, stored_key)?;
    let instance_id = instance_id()?;
    let token: Arc<str> = new_token()?.into();
    // Settled once, before clients connect: every session takes this sandbox state and identity.
    let _ = elefante_core::sandbox::sweep_stale_temp_dirs();
    if let Sandbox::Unavailable(reason) = elefante_core::sandbox::probe().await {
        eprintln!("elefante: sandbox unavailable: {reason}");
    }
    let git_identity = git_identity().await;
    // Recheck under directory ownership: another starter may have become ready
    // while we prepared the provider and credentials.
    let Some((listener, _socket_guard)) = bind(&config.socket, &token).await? else {
        return Ok(());
    };
    let consent = ConsentGate::new(&config);
    let standalone_extractor = standalone_extractor();
    let state = Arc::new(Shared {
        config,
        provider: Mutex::new(provider),
        sessions: Mutex::new(HashMap::new()),
        memories: Mutex::new(HashMap::new()),
        consent: Mutex::new(consent),
        standalone_extractor,
        git_identity,
        next_id: AtomicU64::new(1),
        instance_id,
        clients: AtomicUsize::new(0),
        turns: AtomicUsize::new(0),
    });
    let mut idle_since = Some(Instant::now());
    let mut tick = tokio::time::interval(Duration::from_millis(25));
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                state.clients.fetch_add(1, Ordering::SeqCst);
                idle_since = None;
                let state = state.clone();
                let token = token.clone();
                tokio::spawn(async move {
                    let _ = connection(stream, state.clone(), &token).await;
                    state.clients.fetch_sub(1, Ordering::SeqCst);
                });
            }
            _ = tick.tick() => {
                if state.clients.load(Ordering::SeqCst) == 0 && state.turns.load(Ordering::SeqCst) == 0 {
                    if idle_since.get_or_insert_with(Instant::now).elapsed() >= state.config.idle_timeout { break; }
                } else { idle_since = None; }
            }
        }
    }
    flush_all(&state).await;
    Ok(())
}

struct SessionEntry {
    session: tokio::sync::Mutex<Session>,
    memory: Option<Arc<RepoMemory>>,
    learn_config: Option<LearnConfig>,
    learning: Mutex<SessionLearning>,
    memory_state: Mutex<SessionMemoryState>,
    tools: ToolExecutor,
    /// `web_search` spend not yet added to a turn's usage.
    search_usage: Arc<Mutex<Usage>>,
    view: Mutex<View>,
    approvals: Arc<Approvals>,
}

#[derive(Default)]
struct SessionMemoryState {
    spend: Usage,
    health: LearnHealth,
    reset_pending: bool,
}

#[derive(Default)]
struct SessionLearning {
    /// Absolute watermark in the session history, including turns skipped before consent.
    seen_messages: usize,
    /// Only messages from turns that began and ended under the same granted consent revision.
    history: Vec<Message>,
    learner: Option<Arc<Learner>>,
    idle_generation: u64,
}

struct View {
    id: String,
    cwd: String,
    model: String,
    seq: u64,
    turns: Vec<Value>,
    active: Option<Value>,
    cancel: Option<CancellationToken>,
    ring: VecDeque<Value>,
    clients: Vec<Weak<ClientQueue>>,
    memory: MemoryStatus,
}

impl View {
    fn snapshot(&self) -> Value {
        json!({"session_id":self.id,"seq":self.seq,"cwd":self.cwd,"model":self.model,"turns":self.turns,"active_turn":self.active})
    }

    fn subscribe(&mut self, queue: &Arc<ClientQueue>) {
        self.clients.retain(|client| {
            client
                .upgrade()
                .is_some_and(|client| !client.closed.is_cancelled())
        });
        if !self
            .clients
            .iter()
            .any(|client| client.ptr_eq(&Arc::downgrade(queue)))
        {
            self.clients.push(Arc::downgrade(queue));
        }
    }

    fn emit(&mut self, event: OutputEvent) {
        let turn_id = self
            .active
            .as_ref()
            .and_then(|active| active["turn_id"].as_str())
            .map(str::to_owned);
        self.seq += 1;
        let note = serde_json::to_value(Notification::new(
            &self.id,
            self.seq,
            turn_id,
            output(event),
        ))
        .expect("notification serialization");
        let method = note["method"].as_str().expect("notification method");
        let params = &note["params"];
        if let Some(active) = &mut self.active {
            match method {
                "text.delta" | "reasoning.delta" => {
                    let field = if method == "text.delta" { "text_so_far" } else { "reasoning_so_far" };
                    let mut text = active[field].as_str().unwrap_or_default().to_owned();
                    text.push_str(params["text"].as_str().unwrap_or_default());
                    active[field] = text.into();
                }
                "tool.started" => active["tools"].as_array_mut().expect("tools array").push(json!({"tool_call_id":params["tool_call_id"],"name":params["name"],"state":"running","is_error":false})),
                "tool.finished" => {
                    for tool in active["tools"].as_array_mut().expect("tools array") {
                        if tool["tool_call_id"] == params["tool_call_id"] {
                            tool["is_error"] = params["is_error"].clone();
                            tool["state"] = if params["is_error"] == true {
                                if params["output_preview"].as_str() == Some("interrupted") { "cancelled" } else { "error" }
                            } else { "ok" }.into();
                        }
                    }
                }
                "approval.requested" => active["pending_approval"] = params.clone(),
                "approval.resolved" => active["pending_approval"] = Value::Null,
                "memory.context" => {
                    active["memory_note_ids"] = Value::Array(
                        params["notes"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|note| note["id"].as_i64().map(Value::from))
                            .collect(),
                    );
                }
                "turn.finished" => {
                    self.turns.push(json!({"turn_id":active["turn_id"],"prompt":active["prompt"],"text":active["text_so_far"],"tools":active["tools"],"stop_reason":params["stop_reason"],"usage":params["usage"],"memory_note_ids":active["memory_note_ids"]}));
                    self.active = None;
                    self.cancel = None;
                }
                _ => {},
            }
        }
        self.publish(note);
    }

    fn emit_session(&mut self, event: NotificationEvent) {
        if let NotificationEvent::MemoryStatus(status) = &event {
            self.memory = status.clone();
        }
        self.seq += 1;
        let note = serde_json::to_value(Notification::new(&self.id, self.seq, None, event))
            .expect("notification serialization");
        self.publish(note);
    }

    fn publish(&mut self, note: Value) {
        if self.ring.len() == RING_CAPACITY {
            self.ring.pop_front();
        }
        self.clients.retain(|client| {
            if let Some(client) = client.upgrade() {
                client.push(note.clone());
                !client.closed.is_cancelled()
            } else {
                false
            }
        });
        self.ring.push_back(note);
    }
}

fn output(event: OutputEvent) -> NotificationEvent {
    match event {
        OutputEvent::TurnStarted => NotificationEvent::TurnStarted {},
        OutputEvent::Context { notes } => NotificationEvent::MemoryContext {
            notes: notes.iter().map(wire_context_note).collect(),
        },
        OutputEvent::ReasoningDelta(text) => NotificationEvent::ReasoningDelta { text },
        OutputEvent::TextDelta(text) => NotificationEvent::TextDelta { text },
        OutputEvent::ToolStarted {
            id,
            name,
            input_preview,
        } => NotificationEvent::ToolStarted {
            tool_call_id: id,
            name,
            input_preview: truncate_preview(&input_preview),
        },
        OutputEvent::ToolFinished {
            id,
            is_error,
            output_preview,
            duration_ms,
            ..
        } => NotificationEvent::ToolFinished {
            tool_call_id: id,
            is_error,
            output_preview: truncate_preview(&output_preview),
            duration_ms,
        },
        OutputEvent::ApprovalRequested {
            approval_id,
            command,
            rule,
            network,
        } => NotificationEvent::ApprovalRequested {
            approval_id,
            command,
            rule: rule.to_owned(),
            network,
        },
        OutputEvent::ApprovalResolved {
            approval_id,
            approved,
        } => NotificationEvent::ApprovalResolved {
            approval_id,
            approved,
        },
        OutputEvent::TurnFinished { stop_reason, usage } => {
            NotificationEvent::TurnFinished { stop_reason, usage }
        }
        OutputEvent::Error { message } => NotificationEvent::SessionError { message },
    }
}

fn wire_context_note(note: &CoreNote) -> MemoryNote {
    MemoryNote {
        id: note.id,
        kind: learn::display_safe(&note.kind),
        status: learn::display_safe(&note.status),
        statement: learn::display_safe(&note.statement),
        origin: learn::display_safe(&note.origin),
    }
}

#[derive(Default)]
struct Approvals {
    pending: Mutex<HashMap<String, PendingApproval>>,
}

struct PendingApproval {
    sender: Option<oneshot::Sender<ApprovalOutcome>>,
    receiver: Option<oneshot::Receiver<ApprovalOutcome>>,
}

impl Approvals {
    fn register(&self, id: &str) {
        let (send, receive) = oneshot::channel();
        self.pending.lock().expect("approval map poisoned").insert(
            id.to_owned(),
            PendingApproval {
                sender: Some(send),
                receiver: Some(receive),
            },
        );
    }

    fn resolve(&self, id: &str, approved: bool) -> bool {
        let mut pending = self.pending.lock().expect("approval map poisoned");
        if let Some(sender) = pending
            .get_mut(id)
            .and_then(|pending| pending.sender.take())
        {
            let outcome = if approved {
                ApprovalOutcome::Approved
            } else {
                ApprovalOutcome::Denied
            };
            sender.send(outcome).is_ok()
        } else {
            false
        }
    }
}

impl ApprovalSink for Approvals {
    fn request<'a>(
        &'a self,
        id: &'a str,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ApprovalOutcome> {
        Box::pin(async move {
            let receiver = self
                .pending
                .lock()
                .expect("approval map poisoned")
                .get_mut(id)
                .and_then(|pending| pending.receiver.take());
            let Some(receiver) = receiver else {
                return ApprovalOutcome::Denied;
            };
            let outcome = tokio::select! {
                () = cancel.cancelled() => ApprovalOutcome::Cancelled,
                result = receiver => result.unwrap_or(ApprovalOutcome::Cancelled),
            };
            self.pending
                .lock()
                .expect("approval map poisoned")
                .remove(id);
            outcome
        })
    }
}

/// Reads one NDJSON line, or `None` when the client closed, failed, or sent an oversized or
/// unterminated frame.
async fn read_line(reader: &mut BufReader<OwnedReadHalf>) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut limited = reader.take((MAX_LINE_BYTES + 2) as u64);
    match limited.read_until(b'\n', &mut bytes).await {
        Ok(0) | Err(_) => return None,
        Ok(_) => {}
    }
    (bytes.len() <= MAX_LINE_BYTES + 1 && bytes.last() == Some(&b'\n')).then_some(bytes)
}

/// Compares every byte, so the time taken does not reveal how much of a guess was right.
fn same_token(given: &[u8], token: &[u8]) -> bool {
    given.len() == token.len()
        && given
            .iter()
            .zip(token)
            .fold(0, |difference, (a, b)| difference | (a ^ b))
            == 0
}

/// The first request must be `auth { token }` with the daemon's token. The daemon answers anything
/// else with `UNAUTHORIZED` and the caller closes the connection.
async fn authenticate(
    reader: &mut BufReader<OwnedReadHalf>,
    write: &mut OwnedWriteHalf,
    token: &str,
) -> bool {
    let Some(bytes) = read_line(reader).await else {
        return false;
    };
    let request = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let accepted = request["method"] == "auth"
        && request["params"]["token"]
            .as_str()
            .is_some_and(|given| same_token(given.as_bytes(), token.as_bytes()));
    let reply = if accepted {
        response(id, json!({}))
    } else {
        error(
            id,
            UNAUTHORIZED,
            "the first request must be auth with the daemon's token",
        )
    };
    let Ok(frame) = crate::wire::encode_frame(&reply) else {
        return false;
    };
    write.write_all(&frame).await.is_ok() && accepted
}

async fn connection(stream: UnixStream, state: Arc<Shared>, token: &str) -> anyhow::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    if !authenticate(&mut reader, &mut write, token).await {
        return Ok(());
    }
    let queue = Arc::new(ClientQueue::new());
    let writer_queue = queue.clone();
    let writer = tokio::spawn(async move {
        let _ = writer_queue
            .write_to(&mut write, Duration::from_secs(2))
            .await;
        writer_queue.closed.cancel();
    });
    let mut consent_target = Some(state.standalone_extractor.clone());
    loop {
        let bytes = tokio::select! {
            () = queue.closed.cancelled() => break,
            bytes = read_line(&mut reader) => bytes,
        };
        let Some(bytes) = bytes else { break };
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(request) => handle(&state, &queue, &request, &mut consent_target).await,
            Err(_) => queue.push(error(Value::Null, -32700, "invalid JSON")),
        }
    }
    queue.closed.cancel();
    let _ = writer.await;
    Ok(())
}

fn response(id: Value, result: Value) -> Value {
    serde_json::to_value(crate::wire::Response {
        id,
        payload: crate::wire::ResponsePayload::Result(result),
    })
    .expect("response serialization")
}
fn error(id: Value, code: i32, message: &str) -> Value {
    serde_json::to_value(crate::wire::Response::error(id, code, message))
        .expect("error serialization")
}

async fn handle(
    state: &Arc<Shared>,
    queue: &Arc<ClientQueue>,
    request: &Value,
    consent_target: &mut Option<String>,
) {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    if !request.is_object()
        || request.get("id").is_none()
        || !(id.is_string() || id.is_number() || id.is_null())
    {
        queue.push(error(id, -32600, "request requires an id"));
        return;
    }
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = handle_method(
        state,
        queue,
        &id,
        request["method"].as_str().unwrap_or_default(),
        &params,
        consent_target,
    )
    .await;
    if let Err((code, message)) = result {
        queue.push(error(id, code, &message));
    }
}

type RpcResult = Result<(), (i32, String)>;
fn invalid(error: impl std::fmt::Display) -> (i32, String) {
    (-32602, error.to_string())
}
fn string<'a>(params: &'a Value, name: &str) -> Result<&'a str, (i32, String)> {
    params[name]
        .as_str()
        .ok_or_else(|| invalid(format!("{name} must be a string")))
}

fn note_counts(memory: Option<&Arc<RepoMemory>>) -> MemoryNoteCounts {
    let Some(memory) = memory else {
        return MemoryNoteCounts::default();
    };
    let counts = memory.store.learned_counts();
    MemoryNoteCounts {
        active: counts.active,
        pending: counts.pending,
    }
}

fn health_problem(health: LearnHealth, threshold: u32) -> Option<String> {
    if health.consecutive_failures < threshold {
        return None;
    }
    Some(match health.failure {
        Some(LearnFailure::ProviderStatus(status)) => {
            format!("the provider refused the request (HTTP {status})")
        }
        Some(LearnFailure::UnreadableReply) => "the reply could not be read".to_owned(),
        Some(LearnFailure::NoReply) => "no reply from the provider".to_owned(),
        Some(LearnFailure::Store) | None => "the memory key is unavailable".to_owned(),
    })
}

fn build_memory_status(
    state: &Shared,
    memory: Option<&Arc<RepoMemory>>,
    learn_config: Option<&LearnConfig>,
    local: &SessionMemoryState,
) -> MemoryStatus {
    let seed = state
        .consent
        .lock()
        .expect("consent poisoned")
        .seed(learn_config.map(|config| config.model.as_str()));
    let enabled = state.config.memory && memory.is_some();
    let threshold = learn_config.map_or(3, |config| config.max_failures);
    let failing = enabled
        && seed.consent == MemoryConsentState::Granted
        && local.health.consecutive_failures >= threshold;
    let learning = if failing {
        MemoryLearningState::Failing
    } else if enabled && seed.consent == MemoryConsentState::Granted {
        MemoryLearningState::Working
    } else {
        MemoryLearningState::Paused
    };
    let index = if !state.config.memory || memory.is_none() {
        MemoryIndexState::Off
    } else if memory.is_some_and(|memory| memory.store.index_state().is_complete()) {
        MemoryIndexState::Ready
    } else {
        MemoryIndexState::Building
    };
    let problem = if state.config.memory && memory.is_none() {
        Some("the memory key is unavailable".to_owned())
    } else if local.reset_pending {
        Some("memory was reset: it was not made by this installation".to_owned())
    } else {
        health_problem(local.health, threshold)
    };
    MemoryStatus {
        enabled,
        consent: seed.consent,
        learning,
        extractor: seed.extractor,
        notes: note_counts(memory),
        index,
        spend: local.spend,
        problem,
    }
}

fn memory_status(state: &Shared, entry: &SessionEntry) -> MemoryStatus {
    let local = entry.memory_state.lock().expect("memory state poisoned");
    build_memory_status(
        state,
        entry.memory.as_ref(),
        entry.learn_config.as_ref(),
        &local,
    )
}

fn refresh_memory_status(state: &Shared, entry: &SessionEntry) {
    let status = memory_status(state, entry);
    let mut view = entry.view.lock().expect("view poisoned");
    if view.memory != status {
        view.emit_session(NotificationEvent::MemoryStatus(status));
    }
}

/// The session's tools: the user's Git identity for harness writes and the `web_search` backend,
/// whose spend collects in `search_usage` until the turn ends.
fn session_tools(
    state: &Arc<Shared>,
    cwd: &Path,
    author: &elefante_provider::ModelId,
    search_usage: &Arc<Mutex<Usage>>,
) -> Result<ToolExecutor, (i32, String)> {
    let mut tools = ToolExecutor::new(cwd).map_err(invalid)?;
    if let Some(identity) = &state.git_identity {
        // A failed attach leaves the session without harness Git, and the gate denies its writes.
        match tools.clone().with_git_identity(identity.clone()) {
            Ok(identified) => tools = identified,
            Err(error) => eprintln!(
                "elefante: harness git unavailable for {}: {error}",
                cwd.display()
            ),
        }
    }
    let provider = Arc::downgrade(state);
    Ok(tools.with_web_search(Arc::new(ProviderSearch::new(
        move || provider.upgrade()?.provider.lock().ok()?.clone(),
        super::search::search_model(author, &|name| env::var(name).ok()),
        Arc::clone(search_usage),
    ))))
}

fn create_session(
    state: &Arc<Shared>,
    queue: &Arc<ClientQueue>,
    id: &Value,
    params: &Value,
    consent_target: &mut Option<String>,
) -> RpcResult {
    let cwd = fs::canonicalize(string(params, "cwd")?).map_err(invalid)?;
    if !cwd.is_dir() {
        return Err(invalid("cwd must be a directory"));
    }
    let model = params
        .get("model")
        .map_or(Ok("anthropic/claude-sonnet-5"), |value| {
            value
                .as_str()
                .ok_or_else(|| invalid("model must be a string"))
        })?;
    let mut config = RunConfig::new(model);
    if let Some(value) = params.get("effort") {
        config.effort = Some(serde_json::from_value(value.clone()).map_err(invalid)?);
    }
    if let Some(value) = params.get("max_tokens") {
        config.max_tokens = value
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0)
            .ok_or_else(|| invalid("max_tokens must be a positive u32"))?;
    }
    if let Some(value) = params.get("max_rounds") {
        config.max_rounds = value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n > 0)
            .ok_or_else(|| invalid("max_rounds must be positive"))?;
    }
    let search_usage = Arc::new(Mutex::new(Usage::default()));
    let tools = session_tools(state, &cwd, &config.model, &search_usage)?;
    let sandbox = sandbox_status(tools.sandbox_state());
    let author = config.model.clone();
    let (session, tools, memory) = attach_memory(state, &cwd, Session::new(config), tools);
    let learn_config = (memory.is_some() && state.config.learn != LearnMode::Off).then(|| {
        let mut config = LearnConfig::from_env(&author, &|name| env::var(name).ok());
        config.call_timeout = state.config.learn_timeout;
        config
    });
    // Only a session in which learning can run is told the harness may save preferences.
    let session = if learn_config.is_some() {
        session.with_system_note(elefante_core::LEARNING_HINT)
    } else {
        session
    };
    let session_id = format!(
        "s{}-{}",
        state.instance_id,
        state.next_id.fetch_add(1, Ordering::Relaxed)
    );
    let memory_state = SessionMemoryState {
        reset_pending: memory
            .as_ref()
            .is_some_and(|memory| memory.store.reset_reason().is_some()),
        ..SessionMemoryState::default()
    };
    let status = build_memory_status(state, memory.as_ref(), learn_config.as_ref(), &memory_state);
    let entry = Arc::new(SessionEntry {
        session: tokio::sync::Mutex::new(session),
        memory,
        learn_config,
        learning: Mutex::new(SessionLearning::default()),
        memory_state: Mutex::new(memory_state),
        tools,
        search_usage,
        approvals: Arc::new(Approvals::default()),
        view: Mutex::new(View {
            id: session_id.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
            model: model.to_owned(),
            seq: 0,
            turns: Vec::new(),
            active: None,
            cancel: None,
            ring: VecDeque::new(),
            clients: vec![Arc::downgrade(queue)],
            memory: status.clone(),
        }),
    });
    consent_target.clone_from(&status.extractor);
    state
        .sessions
        .lock()
        .expect("registry poisoned")
        .insert(session_id.clone(), entry);
    queue.push(response(
        id.clone(),
        json!({"session_id":session_id,"seq":0,"memory":status,"sandbox":sandbox}),
    ));
    Ok(())
}

/// The key that authenticates this user's memory stores: beside the credentials, outside any
/// working tree. Created on first use.
fn memory_key(config: &DaemonConfig) -> Result<MemKey, elefante_mem::MemError> {
    let directory = config
        .credentials
        .parent()
        .ok_or_else(|| elefante_mem::MemError::Refused {
            path: config.credentials.clone(),
            reason: "there is no directory to keep the memory key in",
        })?;
    MemKey::load_or_create(&directory.join(KEY_FILE))
}

/// Gives a new session the repository index beneath its working directory, if there is one.
///
/// Memory is an aid and never a requirement: any failure to open the index is reported and the
/// session simply runs without it. The refresh requested here returns immediately.
fn attach_memory(
    state: &Arc<Shared>,
    cwd: &Path,
    session: Session,
    tools: ToolExecutor,
) -> (Session, ToolExecutor, Option<Arc<RepoMemory>>) {
    if !state.config.memory {
        return (session, tools, None);
    }
    let repo = match RepoId::resolve(cwd) {
        Ok(repo) => repo,
        Err(error) => {
            eprintln!("elefante: {error}");
            return (session, tools, None);
        }
    };
    let memory = {
        let mut memories = state.memories.lock().expect("memory registry poisoned");
        if let Some(existing) = memories.get(repo.root()) {
            Arc::clone(existing)
        } else {
            match memory_key(&state.config).and_then(|key| RepoMemory::open(&repo, key)) {
                Ok(opened) => {
                    let opened = Arc::new(opened);
                    memories.insert(repo.root().to_path_buf(), Arc::clone(&opened));
                    opened
                }
                Err(error) => {
                    eprintln!(
                        "elefante: memory unavailable for {}: {error}",
                        repo.root().display()
                    );
                    return (session, tools, None);
                }
            }
        }
    };
    memory.indexer.refresh();
    let scope = repo.scope_of(cwd);
    let context = MemContext::new(
        Arc::clone(&memory.store),
        Arc::clone(&memory.retriever),
        scope.clone(),
    );
    let search = MemSearchTool::new(Arc::clone(&memory.retriever), scope);
    (
        session.with_context(Arc::new(context)),
        tools.with_memory(Arc::new(search)),
        Some(memory),
    )
}

fn validated_cwd(params: &Value) -> Result<PathBuf, (i32, String)> {
    let cwd = fs::canonicalize(string(params, "cwd")?).map_err(invalid)?;
    if !cwd.is_dir() {
        return Err(invalid("cwd must be a directory"));
    }
    Ok(cwd)
}

/// What a repository-level request found at a working directory.
enum RepoAccess {
    /// A store this installation made, open for the request.
    Live(Arc<RepoMemory>),
    /// Nothing has been saved here: there is no store, and reading about one must not make it.
    Absent,
    /// A store this installation did not make. It is left exactly as found.
    Foreign,
}

const FOREIGN_STORE: &str = "memory here was not made by this installation";

/// Finds the repository's store for a `memory.*` request without creating or rebuilding anything.
/// A live session's store is reused. Otherwise an existing store is opened only if its stamp
/// verifies, and is dropped when the request ends. Sessions are the only path that may create or
/// rebuild a store.
fn repository_memory(state: &Shared, cwd: &Path) -> Result<RepoAccess, (i32, String)> {
    if !state.config.memory {
        return Err((MEMORY_OFF, "memory is off".to_owned()));
    }
    let repo =
        RepoId::resolve(cwd).map_err(|_| (MEMORY_OFF, "memory is unavailable".to_owned()))?;
    if let Some(memory) = state
        .memories
        .lock()
        .expect("memory registry poisoned")
        .get(repo.root())
    {
        return Ok(RepoAccess::Live(Arc::clone(memory)));
    }
    let database = StoreDir::path_for(repo.root()).join("memory.db");
    if !database.exists() {
        return Ok(RepoAccess::Absent);
    }
    let unavailable = |_| (MEMORY_OFF, "memory is unavailable".to_owned());
    let key = memory_key(&state.config).map_err(unavailable)?;
    let trusted = open_memory_reader(&database)
        .is_ok_and(|connection| learn::store_stamp_valid(&connection, &key));
    if !trusted {
        return Ok(RepoAccess::Foreign);
    }
    RepoMemory::open(&repo, key)
        .map(|opened| RepoAccess::Live(Arc::new(opened)))
        .map_err(unavailable)
}

/// The numeric part of a session id (`s<instance>-<n>`), so "most recent" is well defined.
fn session_sequence(id: &str) -> u64 {
    id.rsplit('-')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// A repository's status for a client with no session of its own. With several live sessions it is
/// deterministic: the most recently created session speaks for consent and the extractor, spend is
/// the sum over them, and a failing session makes the whole failing.
fn repository_status(state: &Shared, memory: &Arc<RepoMemory>) -> MemoryStatus {
    let mut entries: Vec<(u64, Arc<SessionEntry>)> = state
        .sessions
        .lock()
        .expect("registry poisoned")
        .iter()
        .filter(|(_, entry)| {
            entry
                .memory
                .as_ref()
                .is_some_and(|candidate| Arc::ptr_eq(candidate, memory))
        })
        .map(|(id, entry)| (session_sequence(id), Arc::clone(entry)))
        .collect();
    entries.sort_by_key(|(sequence, _)| *sequence);
    let Some((_, latest)) = entries.last() else {
        let config = LearnConfig::new(state.standalone_extractor.clone());
        let local = SessionMemoryState {
            reset_pending: memory.store.reset_reason().is_some(),
            ..SessionMemoryState::default()
        };
        return build_memory_status(state, Some(memory), Some(&config), &local);
    };
    let mut status = memory_status(state, latest);
    let mut spend = elefante_provider::Usage::default();
    for (_, entry) in &entries {
        let other = memory_status(state, entry);
        spend.accumulate(other.spend);
        if other.learning == MemoryLearningState::Failing {
            status.learning = MemoryLearningState::Failing;
            status.problem = status.problem.or(other.problem);
        }
    }
    status.spend = spend;
    status
}

/// The status of a repository with no store: nothing to count, nothing indexed.
fn absent_status(state: &Shared) -> MemoryStatus {
    let config = LearnConfig::new(state.standalone_extractor.clone());
    build_memory_status(state, None, Some(&config), &SessionMemoryState::default())
}

/// A learned row as it goes on the wire: the origin without the store's `learned:` prefix, as
/// `memory.learned` already sends it.
fn wire_row(row: &learn::LearnedRow) -> Value {
    let origin = row
        .origin
        .strip_prefix(learn::SOURCE_PREFIX)
        .unwrap_or(&row.origin)
        .to_owned();
    json!({
        "id":row.id,"kind":row.kind,"status":row.status,
        "statement":row.statement,"origin":origin,
        "confidence":row.effective_confidence,"verified":row.verified,
        "reinforcements":row.reinforcements
    })
}

async fn handle_repository_memory(
    state: &Arc<Shared>,
    queue: &Arc<ClientQueue>,
    id: &Value,
    method: &str,
    params: &Value,
) -> RpcResult {
    let cwd = validated_cwd(params)?;
    let memory = match repository_memory(state, &cwd)? {
        RepoAccess::Live(memory) => memory,
        RepoAccess::Foreign => return Err((MEMORY_OFF, FOREIGN_STORE.to_owned())),
        RepoAccess::Absent => {
            // Nothing is stored. Answer as an empty store would, and create nothing.
            let result = match method {
                "memory.list" => json!({"notes":[]}),
                "memory.clear" => json!({"removed":0}),
                "memory.forget" | "memory.approve" => {
                    return Err((UNKNOWN_NOTE, "unknown learned note".to_owned()));
                }
                "memory.status" => {
                    serde_json::to_value(absent_status(state)).expect("memory status serialization")
                }
                _ => return Err((-32601, "unknown method".to_owned())),
            };
            queue.push(response(id.clone(), result));
            return Ok(());
        }
    };
    match method {
        "memory.list" => {
            let all = match params.get("all") {
                None => false,
                Some(value) => value
                    .as_bool()
                    .ok_or_else(|| invalid("all must be a boolean"))?,
            };
            let rows = learn::list_async(&memory.store, all)
                .await
                .map_err(invalid)?;
            let notes: Vec<_> = rows.iter().map(wire_row).collect();
            queue.push(response(id.clone(), json!({"notes":notes})));
        }
        "memory.forget" | "memory.approve" => {
            let note_id = params["id"]
                .as_i64()
                .ok_or_else(|| invalid("id must be an integer"))?;
            let changed = if method == "memory.forget" {
                learn::forget_async(&memory.store, note_id)
                    .await
                    .map_err(invalid)?
            } else {
                learn::approve_async(&memory.store, note_id)
                    .await
                    .map_err(invalid)?
            };
            if !changed {
                return Err((UNKNOWN_NOTE, "unknown learned note".to_owned()));
            }
            queue.push(response(id.clone(), json!({})));
        }
        "memory.clear" => {
            let removed = learn::clear_async(&memory.store).await.map_err(invalid)?;
            queue.push(response(id.clone(), json!({"removed":removed})));
        }
        "memory.status" => {
            queue.push(response(
                id.clone(),
                serde_json::to_value(repository_status(state, &memory))
                    .expect("memory status serialization"),
            ));
        }
        _ => return Err((-32601, "unknown method".to_owned())),
    }
    Ok(())
}

fn handle_memory_consent(
    state: &Shared,
    queue: &ClientQueue,
    id: &Value,
    params: &Value,
    consent_target: Option<&str>,
) -> RpcResult {
    let answer = match string(params, "answer")? {
        "learn" => ConsentAnswer::Learn,
        "off" => ConsentAnswer::Off,
        _ => return Err(invalid("answer must be learn or off")),
    };
    match state.config.learn {
        LearnMode::Off => {
            queue.push(response(id.clone(), json!({"consent":"off"})));
            return Ok(());
        }
        LearnMode::On => {
            queue.push(response(id.clone(), json!({"consent":"granted"})));
            return Ok(());
        }
        LearnMode::Ask => {}
    }
    // An answer is for one extractor model: the one named, else the one the connection's last session
    // uses.
    let extractor = match params.get("extractor") {
        Some(value) => value
            .as_str()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .ok_or_else(|| invalid("extractor must be a model name"))?,
        None => consent_target.ok_or_else(|| {
            invalid("memory.consent requires an extractor or a learning-capable session target")
        })?,
    };
    let consent = state
        .consent
        .lock()
        .expect("consent poisoned")
        .answer(answer, extractor)
        .map_err(invalid)?;
    discard_revoked_learning(state);
    refresh_all_memory_status(state);
    queue.push(response(id.clone(), json!({"consent":consent})));
    Ok(())
}

async fn handle_method(
    state: &Arc<Shared>,
    queue: &Arc<ClientQueue>,
    id: &Value,
    method: &str,
    params: &Value,
    consent_target: &mut Option<String>,
) -> RpcResult {
    if method == "key.set" {
        let key = string(params, "api_key")?;
        if key.trim().is_empty() {
            return Err(invalid("api_key must not be empty"));
        }
        let provider = make_provider(&state.config, Some(key.to_owned())).map_err(invalid)?;
        credentials::write(&state.config.credentials, key).map_err(invalid)?;
        *state.provider.lock().expect("provider lock poisoned") = provider;
        queue.push(response(id.clone(), json!({})));
        return Ok(());
    }
    if method == "session.create" {
        return create_session(state, queue, id, params, consent_target);
    }
    if method == "memory.consent" {
        return handle_memory_consent(state, queue, id, params, consent_target.as_deref());
    }
    if matches!(
        method,
        "memory.list" | "memory.forget" | "memory.approve" | "memory.clear" | "memory.status"
    ) {
        return handle_repository_memory(state, queue, id, method, params).await;
    }
    if !matches!(
        method,
        "session.prompt"
            | "session.attach"
            | "session.interrupt"
            | "session.close"
            | "session.resolve_approval"
    ) {
        return Err((-32601, "unknown method".into()));
    }
    let session_id = string(params, "session_id")?;
    if method == "session.close" {
        let removed = state
            .sessions
            .lock()
            .expect("registry poisoned")
            .remove(session_id);
        if let Some(entry) = removed {
            if let Some(cancel) = &entry.view.lock().expect("view poisoned").cancel {
                cancel.cancel();
            }
            // Extract what the gate was holding; the response does not wait for it.
            if let Some(learner) = flushable_learner(state, &entry) {
                tokio::spawn(async move { learner.flush().await });
            }
        }
        queue.push(response(id.clone(), json!({})));
        return Ok(());
    }
    let entry = state
        .sessions
        .lock()
        .expect("registry poisoned")
        .get(session_id)
        .cloned()
        .ok_or((-32001, "unknown session".into()))?;
    match method {
        "session.attach" => {
            return attach_session(state, queue, id, &entry, params, consent_target);
        }
        "session.interrupt" => {
            if let Some(cancel) = &entry.view.lock().expect("view poisoned").cancel {
                cancel.cancel();
            }
            queue.push(response(id.clone(), json!({})));
        }
        "session.resolve_approval" => {
            let approval_id = string(params, "approval_id")?;
            let decision = string(params, "decision")?;
            if !matches!(decision, "allow" | "deny") {
                return Err(invalid("decision must be allow or deny"));
            }
            if !entry.approvals.resolve(approval_id, decision == "allow") {
                return Err((-32005, "unknown or resolved approval".into()));
            }
            queue.push(response(id.clone(), json!({})));
        }
        "session.prompt" => {
            return prompt_session(state, queue, id, &entry, params);
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn attach_session(
    state: &Arc<Shared>,
    queue: &Arc<ClientQueue>,
    id: &Value,
    entry: &SessionEntry,
    params: &Value,
    consent_target: &mut Option<String>,
) -> RpcResult {
    let status = memory_status(state, entry);
    let target = status.extractor.clone();
    let sandbox = serde_json::to_value(sandbox_status(entry.tools.sandbox_state()))
        .expect("sandbox status serialization");
    attach_session_with_result(queue, id, entry, params, move |view, since| {
        let mut result = attach_result(view, since);
        result["memory"] = serde_json::to_value(status).expect("memory status serialization");
        if result["kind"] == "snapshot" {
            result["snapshot"]["sandbox"] = sandbox.clone();
        }
        result["sandbox"] = sandbox;
        result
    })?;
    *consent_target = target;
    Ok(())
}

fn attach_session_with_result(
    queue: &Arc<ClientQueue>,
    id: &Value,
    entry: &SessionEntry,
    params: &Value,
    materialize: impl FnOnce(&View, Option<u64>) -> Value,
) -> RpcResult {
    let since = params
        .get("since")
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| invalid("since must be a nonnegative integer"))
        })
        .transpose()?;
    let mut view = entry.view.lock().expect("view poisoned");
    if since.is_some_and(|since| since > view.seq) {
        return Err((-32003, "since exceeds session seq".into()));
    }
    // Validation errors preserve the live stream. Once validated, invalidate the
    // writer's delivery copy before potentially expensive replay/snapshot cloning.
    // The view lock prevents new same-session notifications until response enqueue.
    queue.discard_session(&view.id);
    let result = materialize(&view, since);
    queue.push(response(id.clone(), result));
    view.subscribe(queue);
    Ok(())
}

fn attach_result(view: &View, since: Option<u64>) -> Value {
    if let Some(since) = since.filter(|since| {
        view.ring
            .front()
            .is_none_or(|first| *since >= first["seq"].as_u64().unwrap() - 1)
    }) {
        let events: Vec<_> = view
            .ring
            .iter()
            .filter(|event| event["seq"].as_u64().unwrap() > since)
            .cloned()
            .collect();
        json!({"kind":"resumed","from":since + 1,"events":events})
    } else {
        json!({"kind":"snapshot","seq":view.seq,"snapshot":view.snapshot()})
    }
}

fn extractor(entry: &SessionEntry) -> Option<&str> {
    entry
        .learn_config
        .as_ref()
        .map(|config| config.model.as_str())
}

fn discard_revoked_learning(state: &Shared) {
    let entries: Vec<_> = state
        .sessions
        .lock()
        .expect("registry poisoned")
        .values()
        .cloned()
        .collect();
    let mut cleared = Vec::new();
    {
        // Lock order everywhere is consent, then learning; memory state is taken on its own
        // (it takes consent itself while building a status), so it is reset after this block.
        let consent = state.consent.lock().expect("consent poisoned");
        for entry in entries {
            if !consent.granted(extractor(&entry)) {
                let mut learning = entry.learning.lock().expect("learning poisoned");
                learning.history.clear();
                if let Some(learner) = learning.learner.take() {
                    learner.cancel();
                }
                learning.idle_generation = learning.idle_generation.wrapping_add(1);
                drop(learning);
                cleared.push(entry);
            }
        }
    }
    for entry in cleared {
        reset_learning_health(state, &entry);
    }
}

/// A learner that is gone takes its failures with it: a new one starts healthy, and the status
/// must say so rather than keep reporting the old learner's problem.
fn reset_learning_health(state: &Shared, entry: &SessionEntry) {
    entry
        .memory_state
        .lock()
        .expect("memory state poisoned")
        .health = LearnHealth::default();
    refresh_memory_status(state, entry);
}

fn refresh_all_memory_status(state: &Shared) {
    let entries: Vec<_> = state
        .sessions
        .lock()
        .expect("registry poisoned")
        .values()
        .cloned()
        .collect();
    for entry in entries {
        refresh_memory_status(state, &entry);
    }
}

struct ServerLearnSink {
    state: Weak<Shared>,
    entry: Weak<SessionEntry>,
}

fn wire_note(note: &learn::LearnedNode) -> MemoryNote {
    MemoryNote {
        id: note.id,
        kind: learn::display_safe(note.kind.as_str()),
        status: learn::display_safe(note.status.as_str()),
        statement: learn::display_safe(&note.statement),
        origin: learn::display_safe(
            note.origin
                .source()
                .strip_prefix(learn::SOURCE_PREFIX)
                .unwrap_or(note.origin.source()),
        ),
    }
}

fn wire_change(change: LearnChange) -> MemoryChange {
    match change {
        LearnChange::Added => MemoryChange::Added,
        LearnChange::Promoted => MemoryChange::Promoted,
        LearnChange::Superseded => MemoryChange::Superseded,
        LearnChange::Revived => MemoryChange::Revived,
    }
}

impl LearnSink for ServerLearnSink {
    fn learned(&self, changes: &[ChangedNote]) {
        let (Some(state), Some(entry)) = (self.state.upgrade(), self.entry.upgrade()) else {
            return;
        };
        {
            let mut view = entry.view.lock().expect("view poisoned");
            for change in changes {
                view.emit_session(NotificationEvent::MemoryLearned {
                    note: wire_note(&change.note),
                    change: wire_change(change.change),
                    replaced: change.replaced.as_ref().map(wire_note),
                });
            }
        }
        refresh_memory_status(&state, &entry);
    }

    fn usage(&self, usage: Usage) {
        let (Some(state), Some(entry)) = (self.state.upgrade(), self.entry.upgrade()) else {
            return;
        };
        entry
            .memory_state
            .lock()
            .expect("memory state poisoned")
            .spend
            .accumulate(usage);
        refresh_memory_status(&state, &entry);
    }

    fn health(&self, health: LearnHealth) {
        let (Some(state), Some(entry)) = (self.state.upgrade(), self.entry.upgrade()) else {
            return;
        };
        entry
            .memory_state
            .lock()
            .expect("memory state poisoned")
            .health = health;
        refresh_memory_status(&state, &entry);
    }
}

fn prompt_session(
    state: &Arc<Shared>,
    queue: &Arc<ClientQueue>,
    id: &Value,
    entry: &Arc<SessionEntry>,
    params: &Value,
) -> RpcResult {
    let text = string(params, "text")?.to_owned();
    let provider = state
        .provider
        .lock()
        .expect("provider lock poisoned")
        .clone()
        .ok_or((-32004, "OPENROUTER_API_KEY is not set".into()))?;
    let mut view = entry.view.lock().expect("view poisoned");
    if view.cancel.is_some() {
        return Err((-32002, "turn already in progress".into()));
    }
    let turn_id = format!("t{}", state.next_id.fetch_add(1, Ordering::Relaxed));
    let cancel = CancellationToken::new();
    let consent_at_start = {
        let consent = state.consent.lock().expect("consent poisoned");
        (consent.granted(extractor(entry)), consent.revision())
    };
    view.active = Some(
        json!({"turn_id":turn_id,"prompt":text,"text_so_far":"","reasoning_so_far":"","tools":[],"pending_approval":null,"memory_note_ids":[]}),
    );
    view.cancel = Some(cancel.clone());
    view.subscribe(queue);
    queue.push(response(id.clone(), json!({"turn_id":turn_id})));
    state.turns.fetch_add(1, Ordering::SeqCst);
    drop(view);
    let state = state.clone();
    let entry = entry.clone();
    tokio::spawn(async move {
        let gate = ToolGate::new(entry.approvals.clone());
        let mut session = entry.session.lock().await;
        // The user may have edited files since the last turn. Never waited on.
        if let Some(memory) = &entry.memory {
            memory.indexer.refresh();
        }
        let _ = session
            .prompt(
                provider.as_ref(),
                &entry.tools,
                &gate,
                text,
                cancel,
                |mut event| {
                    if let OutputEvent::TurnFinished { usage, .. } = &mut event {
                        let mut searched =
                            entry.search_usage.lock().expect("search usage poisoned");
                        usage.accumulate(std::mem::take(&mut *searched));
                    }
                    if let OutputEvent::ApprovalRequested { approval_id, .. } = &event {
                        entry.approvals.register(approval_id);
                    }
                    if let OutputEvent::ApprovalResolved { approval_id, .. } = &event {
                        entry
                            .approvals
                            .pending
                            .lock()
                            .expect("approval map poisoned")
                            .remove(approval_id);
                    }
                    entry.view.lock().expect("view poisoned").emit(event);
                },
            )
            .await;
        // The agent may have edited files during the turn.
        if let Some(memory) = &entry.memory {
            memory.indexer.refresh();
        }
        observe_learning(
            &state,
            &entry,
            &provider,
            session.messages(),
            consent_at_start,
        );
        entry
            .memory_state
            .lock()
            .expect("memory state poisoned")
            .reset_pending = false;
        refresh_memory_status(&state, &entry);
        state.turns.fetch_sub(1, Ordering::SeqCst);
    });
    Ok(())
}

fn observe_learning(
    state: &Arc<Shared>,
    entry: &Arc<SessionEntry>,
    provider: &Arc<dyn Provider>,
    messages: &[Message],
    (granted_at_start, start_revision): (bool, u64),
) {
    // The consent guard is held until the learner has been handed the text. Checking, releasing and
    // then locking learning left a gap in which a revoke could clear everything and this turn would
    // then start a new learner under consent that no longer existed.
    let consent = state.consent.lock().expect("consent poisoned");
    let still_granted = granted_at_start
        && consent.revision() == start_revision
        && consent.granted(extractor(entry));
    let mut learning = entry.learning.lock().expect("learning poisoned");
    let from = learning.seen_messages.min(messages.len());
    learning.seen_messages = messages.len();
    if !still_granted {
        learning.history.clear();
        let had_learner = learning.learner.take().inspect(|learner| learner.cancel());
        learning.idle_generation = learning.idle_generation.wrapping_add(1);
        drop(learning);
        drop(consent);
        if had_learner.is_some() {
            reset_learning_health(state, entry);
        }
        return;
    }
    if from == messages.len() {
        return;
    }
    learning.history.extend_from_slice(&messages[from..]);
    let (Some(memory), Some(config)) = (&entry.memory, &entry.learn_config) else {
        return;
    };
    let learner = Arc::clone(learning.learner.get_or_insert_with(|| {
        Learner::with_sink(
            Arc::clone(&memory.store),
            Arc::clone(provider),
            config.clone(),
            Arc::new(ServerLearnSink {
                state: Arc::downgrade(state),
                entry: Arc::downgrade(entry),
            }),
        )
    }));
    learner.observe(&learning.history);
    learning.idle_generation = learning.idle_generation.wrapping_add(1);
    let generation = learning.idle_generation;
    drop(learning);
    drop(consent);
    schedule_idle_flush(state, entry, learner, generation, start_revision);
}

/// After a quiet spell, extract whatever the gate was holding. A turn in between restarts the wait.
fn schedule_idle_flush(
    state: &Arc<Shared>,
    entry: &Arc<SessionEntry>,
    learner: Arc<Learner>,
    generation: u64,
    consent_revision: u64,
) {
    let wait = state.config.learn_idle;
    let state = Arc::clone(state);
    let entry = Arc::clone(entry);
    tokio::spawn(async move {
        tokio::time::sleep(wait).await;
        let consent_valid = {
            let consent = state.consent.lock().expect("consent poisoned");
            consent.revision() == consent_revision && consent.granted(extractor(&entry))
        };
        let learner_current = {
            let learning = entry.learning.lock().expect("learning poisoned");
            learning.idle_generation == generation
                && learning
                    .learner
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &learner))
        };
        if consent_valid && learner_current {
            learner.flush().await;
        }
    });
}

fn flushable_learner(state: &Shared, entry: &SessionEntry) -> Option<Arc<Learner>> {
    if !state
        .consent
        .lock()
        .expect("consent poisoned")
        .granted(extractor(entry))
    {
        return None;
    }
    entry
        .learning
        .lock()
        .expect("learning poisoned")
        .learner
        .clone()
}

/// Extracts what every session is still holding, waiting a bounded time for each. Called as the
/// daemon exits, because a session that never closed cleanly would otherwise lose its last turns.
async fn flush_all(state: &Arc<Shared>) {
    let entries: Vec<Arc<SessionEntry>> = state
        .sessions
        .lock()
        .expect("registry poisoned")
        .values()
        .cloned()
        .collect();
    for entry in entries {
        if let Some(learner) = flushable_learner(state, &entry) {
            let _ = tokio::time::timeout(Duration::from_secs(10), learner.flush()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn switches_are_on_unless_exactly_zero() {
        use super::{LearnMode, learn_mode, switch};
        assert!(switch(None));
        assert!(switch(Some("1")));
        assert!(switch(Some("yes")));
        assert!(switch(Some("")));
        assert!(!switch(Some("0")));
        assert!(!switch(Some(" 0 ")));
        assert_eq!(learn_mode(true, None), LearnMode::Ask);
        assert_eq!(learn_mode(true, Some("1")), LearnMode::On);
        assert_eq!(learn_mode(true, Some(" ON ")), LearnMode::On);
        assert_eq!(learn_mode(true, Some("0")), LearnMode::Off);
        assert_eq!(learn_mode(true, Some(" off ")), LearnMode::Off);
        assert_eq!(learn_mode(true, Some("unexpected")), LearnMode::Ask);
        assert_eq!(learn_mode(false, None), LearnMode::Off);
        assert_eq!(learn_mode(false, Some("on")), LearnMode::Off);
    }

    use super::*;
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::{io::AsyncWrite, sync::Notify};

    struct HeldWriter {
        stream: UnixStream,
        allowance: Arc<AtomicUsize>,
        blocked: Arc<Notify>,
        probe_written: std::sync::mpsc::Sender<()>,
    }
    impl AsyncWrite for HeldWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            let allowance = self.allowance.load(Ordering::SeqCst);
            if allowance == 0 {
                self.blocked.notify_one();
                return Poll::Pending;
            }
            let result =
                Pin::new(&mut self.stream).poll_write(cx, &bytes[..bytes.len().min(allowance)]);
            if let Poll::Ready(Ok(count)) = result {
                self.allowance.fetch_sub(count, Ordering::SeqCst);
                if bytes == b"{\"id\":\"probe\"}\n" && count == bytes.len() {
                    self.probe_written.send(()).unwrap();
                }
            }
            result
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.stream).poll_shutdown(cx)
        }
    }

    async fn existing_subscriber(dir: &Path) -> (Arc<Shared>, Arc<ClientQueue>, Arc<SessionEntry>) {
        let state = Arc::new(Shared {
            config: DaemonConfig {
                socket: dir.join("socket"),
                credentials: dir.join("credentials.toml"),
                api_key: None,
                base_url: None,
                stub_provider: None,
                idle_timeout: Duration::from_secs(20),
                memory: false,
                learn: LearnMode::Off,
                learn_idle: Duration::from_secs(300),
                learn_timeout: Duration::from_secs(60),
            },
            provider: Mutex::new(None),
            sessions: Mutex::new(HashMap::new()),
            memories: Mutex::new(HashMap::new()),
            consent: Mutex::new(ConsentGate {
                mode: LearnMode::Off,
                path: dir.join(CONSENT_FILE),
                key: None,
                cache: std::cell::RefCell::new(ConsentCache::default()),
                revision: std::cell::Cell::new(0),
            }),
            standalone_extractor: "anthropic/claude-sonnet-5".to_owned(),
            git_identity: None,
            next_id: AtomicU64::new(1),
            instance_id: instance_id().unwrap(),
            clients: AtomicUsize::new(0),
            turns: AtomicUsize::new(0),
        });
        let queue = Arc::new(ClientQueue::new());
        create_session(&state, &queue, &json!(1), &json!({"cwd":dir}), &mut None).unwrap();
        let created = queue.pop().await.unwrap();
        let id = created["result"]["session_id"].as_str().unwrap();
        let entry = state.sessions.lock().unwrap()[id].clone();
        entry
            .view
            .lock()
            .unwrap()
            .emit(OutputEvent::TextDelta("before attach".into()));
        entry.view.lock().unwrap().turns.push(json!({
            "turn_id":"old", "prompt":"history", "text":"x".repeat(1024 * 1024),
            "tools":[], "stop_reason":"end_turn", "usage":null,
        }));
        (state, queue, entry)
    }

    #[tokio::test]
    async fn create_and_attach_report_sandbox_state_and_keep_the_network_badge() {
        let dir = tempfile::tempdir().unwrap();
        let (state, queue, entry) = existing_subscriber(dir.path()).await;
        let expected = serde_json::to_value(sandbox_status(entry.tools.sandbox_state())).unwrap();
        create_session(
            &state,
            &queue,
            &json!(5),
            &json!({"cwd":dir.path()}),
            &mut None,
        )
        .unwrap();
        let created = loop {
            let message = queue.pop().await.unwrap();
            if message["id"] == 5 {
                break message;
            }
        };
        assert_eq!(created["result"]["sandbox"], expected);
        assert!(matches!(
            expected["state"].as_str(),
            Some("on" | "off" | "unavailable")
        ));
        {
            let mut view = entry.view.lock().unwrap();
            view.turns.clear();
            view.active = Some(
                json!({"turn_id":"t1","prompt":"p","text_so_far":"","reasoning_so_far":"","tools":[],"pending_approval":null,"memory_note_ids":[]}),
            );
            view.emit(OutputEvent::ApprovalRequested {
                approval_id: "a1".into(),
                command: "curl example.com".into(),
                rule: "network-requested",
                network: true,
            });
        }
        let attached = Arc::new(ClientQueue::new());
        attach_session(&state, &attached, &json!(6), &entry, &json!({}), &mut None).unwrap();
        let result = attached.pop().await.unwrap()["result"].clone();
        assert_eq!(result["sandbox"], expected);
        let wire: crate::wire::AttachResult = serde_json::from_value(result).unwrap();
        let crate::wire::AttachResult::Snapshot { snapshot, .. } = wire else {
            panic!("expected a snapshot");
        };
        assert_eq!(
            serde_json::to_value(snapshot.sandbox).unwrap(),
            expected,
            "the snapshot carries the same state"
        );
        let pending = snapshot.active_turn.unwrap().pending_approval.unwrap();
        assert!(pending.network, "a reattached client still sees the badge");
        assert_eq!(pending.command, "curl example.com");
    }

    #[tokio::test]
    async fn git_identity_comes_from_the_global_files_and_needs_both_keys() {
        let home = tempfile::tempdir().unwrap();
        let xdg = home.path().join("xdg");
        assert_eq!(git_identity_in(home.path(), Some(xdg.clone())).await, None);
        fs::create_dir_all(xdg.join("git")).unwrap();
        fs::write(xdg.join("git/config"), "[user]\n\tname = Xdg Name\n").unwrap();
        assert_eq!(
            git_identity_in(home.path(), Some(xdg.clone())).await,
            None,
            "a name without an email is no identity"
        );
        fs::write(
            home.path().join(".gitconfig"),
            "[user]\n\temail = person@example.invalid\n",
        )
        .unwrap();
        assert_eq!(
            git_identity_in(home.path(), Some(xdg.clone())).await,
            Some(elefante_guard::git::Identity {
                name: "Xdg Name".into(),
                email: "person@example.invalid".into(),
            })
        );
        fs::write(
            home.path().join(".gitconfig"),
            "[user]\n\tname = Home Name\n\temail = person@example.invalid\n",
        )
        .unwrap();
        assert_eq!(
            git_identity_in(home.path(), Some(xdg)).await.unwrap().name,
            "Home Name",
            "~/.gitconfig wins over the XDG file"
        );
    }

    #[tokio::test]
    async fn web_search_spend_joins_the_turn_usage() {
        let dir = tempfile::tempdir().unwrap();
        let (state, queue, entry) = existing_subscriber(dir.path()).await;
        let usage = |input: u64, cost: f64| {
            json!({"Usage":{"input_tokens":input,"output_tokens":1,"reasoning_tokens":0,
                "cached_input_tokens":0,"cache_write_tokens":0,"cost":cost}})
        };
        // The stub answers requests in order: the turn, the search it starts, then the turn again.
        let script = dir.path().join("script.json");
        fs::write(
            &script,
            serde_json::to_vec(&json!({"rounds":[
                [{"ToolUseStart":{"id":"c1","name":"web_search"}},
                 {"ToolUseComplete":{"id":"c1","input":{"query":"rust release"}}},
                 usage(10, 0.5), {"Stop":"tool_use"}],
                [{"TextDelta":"1. https://example.com/rust"}, usage(7, 0.25), {"Stop":"end_turn"}],
                [{"TextDelta":"done"}, usage(20, 0.125), {"Stop":"end_turn"}]
            ]}))
            .unwrap(),
        )
        .unwrap();
        *state.provider.lock().unwrap() = Some(Arc::new(StubProvider::load(&script).unwrap()));
        prompt_session(&state, &queue, &json!(7), &entry, &json!({"text":"search"})).unwrap();
        let mut search_ok = false;
        let finished = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let message = queue.pop().await.unwrap();
                if message["method"] == "tool.finished" {
                    search_ok = message["params"]["is_error"] == false;
                }
                if message["method"] == "turn.finished" {
                    break message;
                }
            }
        })
        .await
        .unwrap();
        assert!(search_ok, "the search returned a result");
        let usage = &finished["params"]["usage"];
        assert_eq!(usage["input_tokens"], 37, "turn 10 + search 7 + turn 20");
        assert_eq!(usage["cost"], 0.875);
        assert_eq!(*entry.search_usage.lock().unwrap(), Usage::default());
    }

    #[test]
    fn sandbox_status_names_each_state() {
        assert_eq!(sandbox_status(&Sandbox::Seatbelt).state, SandboxState::On);
        assert_eq!(
            sandbox_status(&Sandbox::Bubblewrap {
                bwrap: "/usr/bin/bwrap".into()
            })
            .detail,
            "bubblewrap"
        );
        assert_eq!(sandbox_status(&Sandbox::Disabled).state, SandboxState::Off);
        let unavailable = sandbox_status(&Sandbox::Unavailable("no bwrap".into()));
        assert_eq!(unavailable.state, SandboxState::Unavailable);
        assert_eq!(unavailable.detail, "no bwrap");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn existing_subscriber_attach_invalidates_held_writer_frame() {
        for initial_allowance in [0, 1] {
            let dir = tempfile::tempdir().unwrap();
            let (state, queue, entry) = existing_subscriber(dir.path()).await;
            let (stream, reader) = UnixStream::pair().unwrap();
            let allowance = Arc::new(AtomicUsize::new(initial_allowance));
            let blocked = Arc::new(Notify::new());
            let (probe_written, probe_received) = std::sync::mpsc::channel();
            let mut writer = HeldWriter {
                stream,
                allowance: allowance.clone(),
                blocked: blocked.clone(),
                probe_written,
            };
            let writer_queue = queue.clone();
            let task = tokio::spawn(async move {
                writer_queue
                    .write_to(&mut writer, Duration::from_secs(2))
                    .await
                    .unwrap();
            });
            tokio::time::timeout(Duration::from_secs(2), blocked.notified())
                .await
                .unwrap();
            // Writer has already popped this subscriber's notification, and may
            // have sent one byte. Attach must coordinate with that exact state.
            assert_eq!(
                attach_session(
                    &state,
                    &queue,
                    &json!(99),
                    &entry,
                    &json!({"since":999}),
                    &mut None,
                )
                .unwrap_err()
                .0,
                -32003
            );
            assert!(
                !queue.closed.is_cancelled(),
                "invalid cursors must leave live delivery intact"
            );
            attach_session_with_result(&queue, &json!(2), &entry, &json!({}), |view, since| {
                // Resume the held writer inside snapshot materialization.
                // The unrelated probe forces a completed transport write at
                // this exact point, before cloning the 1 MiB history below.
                allowance.store(usize::MAX, Ordering::SeqCst);
                if initial_allowance == 0 {
                    queue.push(json!({"id":"probe"}));
                    probe_received.recv_timeout(Duration::from_secs(1)).unwrap();
                }
                attach_result(view, since)
            })
            .unwrap();
            let mut reader = BufReader::new(reader);
            if initial_allowance == 0 {
                entry
                    .view
                    .lock()
                    .unwrap()
                    .emit(OutputEvent::TextDelta("after attach".into()));
                let mut line = String::new();
                tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&line).unwrap()["id"],
                    "probe",
                    "no same-session note may finish while attach materializes"
                );
                line.clear();
                tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
                    .await
                    .unwrap()
                    .unwrap();
                let response: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(response["id"], 2);
                assert_eq!(
                    response["result"]["snapshot"]["turns"][0]["text"]
                        .as_str()
                        .unwrap()
                        .len(),
                    1024 * 1024
                );
                line.clear();
                reader.read_line(&mut line).await.unwrap();
                let notification: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(notification["params"]["text"], "after attach");
                queue.closed.cancel();
            } else {
                assert!(queue.closed.is_cancelled());
                let mut bytes = Vec::new();
                tokio::time::timeout(Duration::from_secs(2), reader.read_to_end(&mut bytes))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    bytes.len(),
                    1,
                    "the old frame must not complete or be followed by a response"
                );
            }
            task.await.unwrap();
        }
    }
}
