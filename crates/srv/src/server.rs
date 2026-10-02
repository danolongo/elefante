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

use anyhow::Context as _;
use elefante_core::{
    ApprovalOutcome, ApprovalSink, OutputEvent, RunConfig, Session, ToolGate,
    memory::{MemContext, MemSearchTool},
};
use elefante_mem::{Indexer, RepoId, Store, indexer::IndexOptions, retrieve::Retriever};
use elefante_provider::{OpenRouterProvider, Provider};
use elefante_rpc::{Notification, NotificationEvent, truncate_preview};
use elefante_tools::ToolExecutor;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader},
    net::{UnixListener, UnixStream},
    sync::oneshot,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use crate::{MAX_LINE_BYTES, RING_CAPACITY, credentials, queue::ClientQueue, stub::StubProvider};

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

fn home() -> anyhow::Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

impl DaemonConfig {
    /// Capture and validate daemon environment settings.
    ///
    /// # Errors
    /// Rejects invalid idle durations, unavailable home paths, and non-isolated stubs.
    pub fn from_env() -> anyhow::Result<Self> {
        let stub_provider = env::var_os("ELEFANTE_STUB_PROVIDER").map(PathBuf::from);
        anyhow::ensure!(
            stub_provider.is_none() || env::var_os("ELEFANTE_SOCKET").is_some(),
            "ELEFANTE_STUB_PROVIDER requires ELEFANTE_SOCKET"
        );
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
            memory: env::var("ELEFANTE_MEMORY").map_or(true, |value| value.trim() != "0"),
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

async fn bind(path: &Path) -> anyhow::Result<Option<(UnixListener, SocketGuard)>> {
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
    fn open(repo: &RepoId) -> Result<Self, elefante_mem::MemError> {
        let store = Arc::new(Store::open(repo)?);
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

struct Shared {
    config: DaemonConfig,
    provider: Mutex<Option<Arc<dyn Provider>>>,
    sessions: Mutex<HashMap<String, Arc<SessionEntry>>>,
    memories: Mutex<HashMap<PathBuf, Arc<RepoMemory>>>,
    next_id: AtomicU64,
    instance_id: String,
    clients: AtomicUsize,
    turns: AtomicUsize,
}

fn instance_id() -> io::Result<String> {
    use std::io::Read as _;
    let mut bytes = [0_u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(u128::from_ne_bytes(bytes).to_string())
}

fn make_provider(
    config: &DaemonConfig,
    stored_key: Option<String>,
) -> anyhow::Result<Option<Arc<dyn Provider>>> {
    if let Some(path) = &config.stub_provider {
        return Ok(Some(Arc::new(StubProvider::load(path)?)));
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
    // Recheck under directory ownership: another starter may have become ready
    // while we prepared the provider and credentials.
    let Some((listener, _socket_guard)) = bind(&config.socket).await? else {
        return Ok(());
    };
    let state = Arc::new(Shared {
        config,
        provider: Mutex::new(provider),
        sessions: Mutex::new(HashMap::new()),
        memories: Mutex::new(HashMap::new()),
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
                tokio::spawn(async move {
                    let _ = connection(stream, state.clone()).await;
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
    Ok(())
}

struct SessionEntry {
    session: tokio::sync::Mutex<Session>,
    memory: Option<Arc<RepoMemory>>,
    tools: ToolExecutor,
    view: Mutex<View>,
    approvals: Arc<Approvals>,
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
                "turn.finished" => {
                    self.turns.push(json!({"turn_id":active["turn_id"],"prompt":active["prompt"],"text":active["text_so_far"],"tools":active["tools"],"stop_reason":params["stop_reason"],"usage":params["usage"]}));
                    self.active = None;
                    self.cancel = None;
                }
                _ => {},
            }
        }
        if self.ring.len() == RING_CAPACITY {
            self.ring.pop_front();
        }
        self.ring.push_back(note.clone());
        self.clients.retain(|client| {
            if let Some(client) = client.upgrade() {
                client.push(note.clone());
                !client.closed.is_cancelled()
            } else {
                false
            }
        });
    }
}

fn output(event: OutputEvent) -> NotificationEvent {
    match event {
        OutputEvent::TurnStarted => NotificationEvent::TurnStarted {},
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
        } => NotificationEvent::ApprovalRequested {
            approval_id,
            command,
            rule: rule.to_owned(),
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

async fn connection(stream: UnixStream, state: Arc<Shared>) -> anyhow::Result<()> {
    let (read, mut write) = stream.into_split();
    let queue = Arc::new(ClientQueue::new());
    let writer_queue = queue.clone();
    let writer = tokio::spawn(async move {
        let _ = writer_queue
            .write_to(&mut write, Duration::from_secs(2))
            .await;
        writer_queue.closed.cancel();
    });
    let mut reader = BufReader::new(read);
    loop {
        let mut bytes = Vec::new();
        let mut limited = (&mut reader).take((MAX_LINE_BYTES + 2) as u64);
        let result = tokio::select! {
            () = queue.closed.cancelled() => break,
            result = limited.read_until(b'\n', &mut bytes) => result,
        };
        match result {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if bytes.len() > MAX_LINE_BYTES + 1 || bytes.last() != Some(&b'\n') {
            break;
        }
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(request) => handle(&state, &queue, &request),
            Err(_) => queue.push(error(Value::Null, -32700, "invalid JSON")),
        }
    }
    queue.closed.cancel();
    let _ = writer.await;
    Ok(())
}

fn response(id: Value, result: Value) -> Value {
    serde_json::to_value(elefante_rpc::Response {
        id,
        payload: elefante_rpc::ResponsePayload::Result(result),
    })
    .expect("response serialization")
}
fn error(id: Value, code: i32, message: &str) -> Value {
    serde_json::to_value(elefante_rpc::Response::error(id, code, message))
        .expect("error serialization")
}

fn handle(state: &Arc<Shared>, queue: &Arc<ClientQueue>, request: &Value) {
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
    );
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

fn create_session(
    state: &Arc<Shared>,
    queue: &Arc<ClientQueue>,
    id: &Value,
    params: &Value,
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
    let tools = ToolExecutor::new(&cwd).map_err(invalid)?;
    let (session, tools, memory) = attach_memory(state, &cwd, Session::new(config), tools);
    let session_id = format!(
        "s{}-{}",
        state.instance_id,
        state.next_id.fetch_add(1, Ordering::Relaxed)
    );
    let entry = Arc::new(SessionEntry {
        session: tokio::sync::Mutex::new(session),
        memory,
        tools,
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
        }),
    });
    state
        .sessions
        .lock()
        .expect("registry poisoned")
        .insert(session_id.clone(), entry);
    queue.push(response(
        id.clone(),
        json!({"session_id":session_id,"seq":0}),
    ));
    Ok(())
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
            match RepoMemory::open(&repo) {
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

fn handle_method(
    state: &Arc<Shared>,
    queue: &Arc<ClientQueue>,
    id: &Value,
    method: &str,
    params: &Value,
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
        return create_session(state, queue, id, params);
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
        if let Some(entry) = state
            .sessions
            .lock()
            .expect("registry poisoned")
            .remove(session_id)
            && let Some(cancel) = &entry.view.lock().expect("view poisoned").cancel
        {
            cancel.cancel();
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
            return attach_session(queue, id, &entry, params);
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
    queue: &Arc<ClientQueue>,
    id: &Value,
    entry: &SessionEntry,
    params: &Value,
) -> RpcResult {
    attach_session_with_result(queue, id, entry, params, attach_result)
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
    view.active = Some(
        json!({"turn_id":turn_id,"prompt":text,"text_so_far":"","reasoning_so_far":"","tools":[],"pending_approval":null}),
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
                |event| {
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
        state.turns.fetch_sub(1, Ordering::SeqCst);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
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

    async fn existing_subscriber(dir: &Path) -> (Arc<ClientQueue>, Arc<SessionEntry>) {
        let state = Arc::new(Shared {
            config: DaemonConfig {
                socket: dir.join("socket"),
                credentials: dir.join("credentials.toml"),
                api_key: None,
                base_url: None,
                stub_provider: None,
                idle_timeout: Duration::from_secs(20),
                memory: false,
            },
            provider: Mutex::new(None),
            sessions: Mutex::new(HashMap::new()),
            memories: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            instance_id: instance_id().unwrap(),
            clients: AtomicUsize::new(0),
            turns: AtomicUsize::new(0),
        });
        let queue = Arc::new(ClientQueue::new());
        create_session(&state, &queue, &json!(1), &json!({"cwd":dir})).unwrap();
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
        (queue, entry)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn existing_subscriber_attach_invalidates_held_writer_frame() {
        for initial_allowance in [0, 1] {
            let dir = tempfile::tempdir().unwrap();
            let (queue, entry) = existing_subscriber(dir.path()).await;
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
                attach_session(&queue, &json!(99), &entry, &json!({"since":999}))
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
