use anyhow::{Context, bail};
use elefante_cli::StreamRenderer;
use elefante_rpc::{
    MAX_FRAME_BYTES, Notification, NotificationEvent, Sequence, decode_frame, encode_frame,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, VecDeque},
    env,
    io::{self, BufRead, BufReader, IsTerminal, Read, Write},
    os::unix::{net::UnixStream, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const USAGE: &str = "usage: elefante [--model MODEL] [--effort LEVEL] [--max-rounds N] [--max-tokens N] [PROMPT ...]\n       elefante memory <list|forget|approve|clear>\n\nWith a prompt, stream a headless daemon session. With no prompt, launch the TUI. Optional: OPENROUTER_API_KEY, OPENROUTER_MODEL, OPENROUTER_BASE_URL, ELEFANTE_EFFORT.";

struct Options {
    prompt: Option<String>,
    model: String,
    effort: String,
    max_rounds: usize,
    max_tokens: u32,
}
impl Options {
    fn parse() -> anyhow::Result<Self> {
        let mut args = env::args().skip(1);
        let mut parts = Vec::new();
        let mut options = Self {
            prompt: None,
            model: env::var("OPENROUTER_MODEL")
                .unwrap_or_else(|_| "anthropic/claude-sonnet-5".into()),
            effort: env::var("ELEFANTE_EFFORT").unwrap_or_else(|_| "high".into()),
            max_rounds: 24,
            max_tokens: 16_384,
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                "--model" => options.model = next_value(&mut args, &arg)?,
                "--effort" => options.effort = next_value(&mut args, &arg)?,
                "--max-rounds" => {
                    options.max_rounds = next_value(&mut args, &arg)?
                        .parse()
                        .context("invalid --max-rounds")?;
                }
                "--max-tokens" => {
                    options.max_tokens = next_value(&mut args, &arg)?
                        .parse()
                        .context("invalid --max-tokens")?;
                }
                "--" => {
                    parts.extend(args);
                    break;
                }
                flag if flag.starts_with('-') => bail!("unknown option {flag:?}\n\n{USAGE}"),
                _ => parts.push(arg),
            }
        }
        if !matches!(
            options.effort.as_str(),
            "low" | "medium" | "high" | "xhigh" | "max"
        ) {
            bail!(
                "invalid effort {:?}; expected low, medium, high, xhigh, or max",
                options.effort
            );
        }
        if !parts.is_empty() {
            let prompt = parts.join(" ").trim_end_matches(['\n', '\r']).to_owned();
            if prompt.trim().is_empty() {
                bail!("prompt cannot be empty\n\n{USAGE}");
            }
            options.prompt = Some(prompt);
        }
        Ok(options)
    }
}
fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<String> {
    args.next()
        .with_context(|| format!("{flag} requires a value"))
}

fn socket_path() -> anyhow::Result<PathBuf> {
    if let Some(path) = env::var_os("ELEFANTE_SOCKET") {
        return Ok(path.into());
    }
    if let Some(dir) = env::var_os("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(dir).join("elefante.sock"));
    }
    Ok(
        PathBuf::from(env::var_os("HOME").context("HOME is not set")?)
            .join(".elefante/run/elefante.sock"),
    )
}
fn daemon_binary() -> anyhow::Result<PathBuf> {
    let exe = env::current_exe().context("cannot locate the elefante executable")?;
    let sibling = exe.with_file_name("elefante-srv");
    if sibling.is_file() {
        return Ok(sibling);
    }
    if exe.parent().is_some_and(|dir| dir.ends_with("deps"))
        && let Some(profile) = exe.parent().and_then(Path::parent)
    {
        let sibling = profile.join("elefante-srv");
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    Ok(PathBuf::from("elefante-srv"))
}
fn connect(path: &Path) -> anyhow::Result<UnixStream> {
    if let Ok(stream) = UnixStream::connect(path) {
        return Ok(stream);
    }
    let mut child = Command::new(daemon_binary()?)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn elefante-srv; install it beside elefante or on PATH")?;
    let mut stderr = child
        .stderr
        .take()
        .context("daemon stderr was not captured")?;
    let diagnostics = Arc::new(Mutex::new(Vec::new()));
    let collected = Arc::clone(&diagnostics);
    let reader = thread::spawn(move || {
        let mut buffer = [0; 4096];
        while let Ok(count) = stderr.read(&mut buffer) {
            if count == 0 {
                break;
            }
            if let Ok(mut output) = collected.lock() {
                let remaining = 65_536_usize.saturating_sub(output.len());
                output.extend_from_slice(&buffer[..count.min(remaining)]);
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(stream) = UnixStream::connect(path) {
            return Ok(stream);
        }
        if let Some(status) = child.try_wait().context("failed to poll daemon startup")? {
            if let Ok(stream) = UnixStream::connect(path) {
                return Ok(stream);
            }
            let _ = reader.join();
            let output = diagnostics
                .lock()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default();
            bail!(
                "elefante-srv exited during startup ({status}): {}",
                output.trim()
            );
        }
        if Instant::now() >= deadline {
            bail!(
                "elefante-srv did not become ready within 2 seconds at {}",
                path.display()
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

struct Client {
    reader: BufReader<UnixStream>,
    next_id: u64,
    pending: VecDeque<Notification>,
}
impl Client {
    fn new(stream: UnixStream) -> Self {
        Self {
            reader: BufReader::new(stream),
            next_id: 0,
            pending: VecDeque::new(),
        }
    }
    fn read(&mut self) -> anyhow::Result<Value> {
        let mut line = Vec::new();
        loop {
            let chunk = self
                .reader
                .fill_buf()
                .context("failed to read daemon stream")?;
            if chunk.is_empty() {
                bail!("daemon disconnected before turn.finished");
            }
            let count = chunk
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(chunk.len(), |index| index + 1);
            let done = chunk[count - 1] == b'\n';
            if line.len() + count - usize::from(done) > MAX_FRAME_BYTES {
                bail!("daemon frame exceeds 8 MiB");
            }
            line.extend_from_slice(&chunk[..count]);
            self.reader.consume(count);
            if done {
                return decode_frame(&line).context("invalid daemon JSON");
            }
        }
    }
    fn request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let writer = self.reader.get_mut();
        let mut request = json!({"jsonrpc":"2.0", "id":id, "method":method});
        request["params"] = params;
        writer.write_all(&encode_frame(&request)?)?;
        writer.flush()?;
        loop {
            let message = self.read()?;
            if message.get("id").is_none() {
                self.pending
                    .push_back(serde_json::from_value(message).context("invalid notification")?);
                continue;
            }
            if message["id"] != id {
                bail!("unexpected daemon response id");
            }
            if let Some(error) = message.get("error") {
                return Err(anyhow::Error::new(RpcFailure {
                    code: error["code"].as_i64().unwrap_or(0),
                    message: error["message"]
                        .as_str()
                        .unwrap_or("unknown daemon error")
                        .to_owned(),
                }));
            }
            return message
                .get("result")
                .cloned()
                .context("daemon response has no result");
        }
    }
    fn notification(&mut self) -> anyhow::Result<Notification> {
        if let Some(note) = self.pending.pop_front() {
            return Ok(note);
        }
        serde_json::from_value(self.read()?).context("expected daemon notification")
    }
}
#[derive(Debug)]
struct RpcFailure {
    code: i64,
    message: String,
}
impl std::fmt::Display for RpcFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}
impl std::error::Error for RpcFailure {}

fn launch_tui(options: &Options, path: &Path) -> anyhow::Result<()> {
    let exe = env::current_exe()?;
    let sibling = exe
        .parent()
        .context("executable has no parent")?
        .join("tui/src/main.tsx");
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tui/src/main.tsx");
    let entry = if sibling.is_file() { sibling } else { source };
    let status = Command::new("bun")
        .arg("run")
        .arg(entry)
        .arg("--model")
        .arg(&options.model)
        .arg("--effort")
        .arg(&options.effort)
        .arg("--max-rounds")
        .arg(options.max_rounds.to_string())
        .arg("--max-tokens")
        .arg(options.max_tokens.to_string())
        .env("ELEFANTE_SOCKET", path)
        .status()
        .context("failed to launch TUI; Bun must be installed on PATH")?;
    if !status.success() {
        bail!("TUI exited with {status}");
    }
    Ok(())
}
fn prompt_with_key(client: &mut Client, session: &str, prompt: &str) -> anyhow::Result<()> {
    let params = json!({"session_id":session,"text":prompt});
    match client.request("session.prompt", params.clone()) {
        Ok(_) => return Ok(()),
        Err(error)
            if error
                .downcast_ref::<RpcFailure>()
                .is_some_and(|e| e.code == -32004) => {}
        Err(error) => return Err(error),
    }
    if !io::stdin().is_terminal() {
        bail!("OPENROUTER_API_KEY is not set");
    }
    eprint!("OpenRouter API key: ");
    io::stderr().flush()?;
    let mut key = String::new();
    io::stdin().read_line(&mut key)?;
    let key = key.trim();
    if key.is_empty() {
        bail!("OPENROUTER_API_KEY is not set");
    }
    client.request("key.set", json!({"api_key":key}))?;
    client.request("session.prompt", params)?;
    Ok(())
}
fn resume(client: &mut Client, path: &Path, session: &str, since: u64) -> anyhow::Result<()> {
    *client = Client::new(connect(path)?);
    let result = client.request(
        "session.attach",
        json!({"session_id":session,"since":since}),
    )?;
    if result["kind"] != "resumed" {
        bail!(
            "cannot resume headless output: daemon returned a snapshot; previously written output cannot be replaced"
        );
    }
    let events: Vec<Notification> = serde_json::from_value(result["events"].clone())?;
    for event in events.into_iter().rev() {
        client.pending.push_front(event);
    }
    Ok(())
}
fn stream_turn(client: &mut Client, path: &Path, session: &str) -> anyhow::Result<()> {
    let color = env::var_os("NO_COLOR").is_none();
    let mut out = StreamRenderer::new(io::stdout(), color && io::stdout().is_terminal());
    let mut err = StreamRenderer::new(io::stderr(), color && io::stderr().is_terminal());
    let mut seen = BTreeSet::new();
    let mut cursor = 0;
    let mut failures = 0;
    let mut abnormal = false;
    let result = (|| -> anyhow::Result<()> {
        loop {
            let note = match client.notification() {
                Ok(note) => note,
                Err(error) => {
                    failures += 1;
                    if failures > 3 {
                        return Err(error);
                    }
                    resume(client, path, session, cursor)?;
                    continue;
                }
            };
            if note.session_id != session {
                continue;
            }
            let (from, to) = match note.sequence {
                Sequence::Single { seq } => (seq, seq),
                Sequence::Coalesced { seq_from, seq_to } => (seq_from, seq_to),
            };
            if from == 0 || to < from {
                bail!("invalid notification sequence range");
            }
            if (from..=to).all(|seq| seen.contains(&seq)) {
                continue;
            }
            if from != cursor + 1 {
                failures += 1;
                if failures > 3 {
                    bail!("daemon notification stream has an unresolved sequence gap");
                }
                resume(client, path, session, cursor)?;
                continue;
            }
            if matches!(note.event, NotificationEvent::TextDelta { .. }) {
                out.render(&note)?;
            } else {
                err.render(&note)?;
            }
            if let NotificationEvent::ApprovalRequested { approval_id, .. } = &note.event {
                // Scripts resolve escalation immediately without executing the command.
                match client.request(
                    "session.resolve_approval",
                    json!({"session_id":session,"approval_id":approval_id,"decision":"deny"}),
                ) {
                    Err(error)
                        if error
                            .downcast_ref::<RpcFailure>()
                            .is_some_and(|e| e.code == -32005) => {}
                    result => {
                        result?;
                    }
                }
            }
            seen.extend(from.max(to.saturating_sub(4096))..=to);
            cursor = to;
            seen.retain(|seq| *seq >= cursor.saturating_sub(4096));
            abnormal |= matches!(note.event, NotificationEvent::SessionError { .. });
            if matches!(note.event, NotificationEvent::TurnFinished { .. }) {
                break;
            }
        }
        if abnormal {
            bail!("session ended abnormally");
        }
        Ok(())
    })();
    out.finish()?;
    err.finish()?;
    result
}
fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("memory") {
        let cwd = env::current_dir().context("failed to determine current directory")?;
        let key_path = elefante_mem::MemKey::default_path().context("HOME is not set")?;
        let code = elefante_cli::memory::run(&args[1..], &cwd, &key_path, &mut io::stdout())?;
        std::process::exit(code);
    }
    let options = Options::parse()?;
    let path = socket_path()?;
    let mut client = Client::new(connect(&path)?);
    let Some(prompt) = &options.prompt else {
        return launch_tui(&options, &path);
    };
    let cwd = env::current_dir().context("failed to determine current directory")?;
    let created = client.request("session.create", json!({"cwd":cwd,"model":options.model,"effort":options.effort,"max_rounds":options.max_rounds,"max_tokens":options.max_tokens}))?;
    let session = created["session_id"]
        .as_str()
        .context("session.create returned no session_id")?;
    let result = (|| {
        client.request("session.attach", json!({"session_id":session,"since":0}))?;
        prompt_with_key(&mut client, session, prompt)?;
        stream_turn(&mut client, &path, session)
    })();
    let close = client.request("session.close", json!({"session_id":session}));
    result?;
    close?;
    Ok(())
}
