//! `elefante memory`: look at what was learned about a repository, and remove it.
//!
//! These commands go through a listening daemon when there is one (`memory.*` over the socket, so
//! the change takes effect on its next retrieval) and open the repository's store directly when
//! there is not. Persisted free-form text is injected into prompts, so a person must always be able
//! to see it and take it out.

use std::{
    env,
    io::Write,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use elefante_mem::{
    MemKey, RepoId, StoreDir,
    consent::{CONSENT_FILE, ConsentAnswer, ConsentRecord},
    learn::{self, LearnedRow},
    schema::{open_memory, open_memory_reader},
};
use serde_json::{Value, json};

pub const USAGE: &str = "usage: elefante memory <command>\n\ncommands:\n  list [--all]     show what was learned about this repository (--all includes superseded)\n  forget <id>      delete one learned note\n  approve <id>     accept a note only the assistant had vouched for\n  clear --yes      delete every learned note for this repository\n  learn on|off [--model SLUG]  allow or decline learning, for the extractor model this will use";

/// One `memory learn` command: the answer, and the extractor model it is an answer for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LearnRequest {
    pub answer: ConsentAnswer,
    pub extractor: String,
}

/// Parses `memory learn on|off [--model SLUG]`, or returns `None` for another subcommand.
///
/// Consent is kept per extractor model, so the model the answer is for is always named: `--model`,
/// else `ELEFANTE_EXTRACT_MODEL`, else `OPENROUTER_MODEL`, else the default a bare `elefante` uses.
///
/// # Errors
/// A learn command that is not exactly one setting and at most one `--model`.
pub fn learn_request(args: &[String]) -> anyhow::Result<Option<LearnRequest>> {
    let [command, rest @ ..] = args else {
        return Ok(None);
    };
    if command != "learn" {
        return Ok(None);
    }
    let mut setting = None;
    let mut model = None;
    let mut rest = rest.iter();
    while let Some(arg) = rest.next() {
        if arg == "--model" {
            let slug = rest
                .next()
                .filter(|slug| !slug.trim().is_empty())
                .with_context(|| format!("`memory learn --model` needs a model\n\n{USAGE}"))?;
            if model.replace(slug.trim().to_owned()).is_some() {
                bail!("`memory learn` takes one --model\n\n{USAGE}");
            }
        } else if setting.replace(arg.as_str()).is_some() {
            bail!("`memory learn` takes exactly one setting\n\n{USAGE}");
        }
    }
    let answer = match setting {
        Some("on") => ConsentAnswer::Learn,
        Some("off") => ConsentAnswer::Off,
        _ => bail!("`memory learn` expects on or off\n\n{USAGE}"),
    };
    Ok(Some(LearnRequest {
        answer,
        extractor: model.unwrap_or_else(|| configured_extractor().trim().to_owned()),
    }))
}

/// What `memory learn` tells the person, naming the model the answer is for.
#[must_use]
pub fn learn_report(consent: &str, extractor: &str) -> String {
    format!("memory learning is {consent} for {extractor}")
}

fn configured_extractor() -> String {
    env::var("ELEFANTE_EXTRACT_MODEL")
        .ok()
        .filter(|model| !model.trim().is_empty())
        .or_else(|| {
            env::var("OPENROUTER_MODEL")
                .ok()
                .filter(|model| !model.trim().is_empty())
        })
        .unwrap_or_else(|| "anthropic/claude-sonnet-5".to_owned())
}

/// Writes the answer straight to the consent file, for when no daemon is listening.
///
/// # Errors
/// The memory key or the file cannot be read or written.
pub fn set_learning(
    request: &LearnRequest,
    key_path: &Path,
    out: &mut dyn Write,
) -> anyhow::Result<i32> {
    let key = MemKey::load_or_create(key_path).context("cannot read the memory key")?;
    let consent_path = key_path
        .parent()
        .context("memory key path has no parent directory")?
        .join(CONSENT_FILE);
    elefante_mem::consent::write(
        &consent_path,
        &key,
        &ConsentRecord {
            answer: request.answer,
            extractor: request.extractor.clone(),
        },
    )
    .context("cannot save memory consent")?;
    writeln!(
        out,
        "{}",
        learn_report(
            if request.answer == ConsentAnswer::Learn {
                "granted"
            } else {
                "declined"
            },
            &request.extractor
        )
    )?;
    Ok(0)
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

fn id_arg(args: &[String], command: &str) -> anyhow::Result<i64> {
    let [id] = args else {
        bail!("`memory {command}` takes exactly one note id\n\n{USAGE}");
    };
    id.parse()
        .with_context(|| format!("{id:?} is not a note id"))
}

fn render(rows: &[LearnedRow], out: &mut dyn Write) -> anyhow::Result<()> {
    if rows.is_empty() {
        writeln!(out, "no learned notes for this repository")?;
        return Ok(());
    }
    writeln!(
        out,
        "{:>5}  {:<10}  {:<10}  {:>4}  {:<9}  note",
        "id", "status", "kind", "conf", "origin"
    )?;
    for row in rows {
        let status = learn::display_safe(&row.status);
        let kind = learn::display_safe(&row.kind);
        let origin =
            learn::display_safe(row.origin.strip_prefix("learned:").unwrap_or(&row.origin));
        let statement = learn::display_safe(&row.statement);
        let flag = if row.verified {
            ""
        } else {
            "  [fails verification; never shown]"
        };
        writeln!(
            out,
            "{:>5}  {:<10}  {:<10}  {:>4.2}  {:<9}  {}{flag}",
            row.id, status, kind, row.effective_confidence, origin, statement
        )?;
    }
    Ok(())
}

/// Validates a repository memory command and produces its daemon request. Consent and help are
/// handled separately and return `None`.
///
/// # Errors
/// Malformed arguments or an unknown command.
pub fn daemon_request(
    args: &[String],
    cwd: &Path,
) -> anyhow::Result<Option<(&'static str, Value)>> {
    let Some((command, rest)) = args.split_first() else {
        return Ok(None);
    };
    let cwd = cwd.to_string_lossy();
    match command.as_str() {
        "list" => {
            let all = match rest {
                [] => false,
                [flag] if flag == "--all" => true,
                _ => bail!("`memory list` takes only --all\n\n{USAGE}"),
            };
            Ok(Some(("memory.list", json!({"cwd":cwd,"all":all}))))
        }
        "forget" | "approve" => {
            let id = id_arg(rest, command)?;
            let method = if command == "forget" {
                "memory.forget"
            } else {
                "memory.approve"
            };
            Ok(Some((method, json!({"cwd":cwd,"id":id}))))
        }
        "clear" => {
            if rest != ["--yes"] {
                bail!(
                    "this deletes every learned note for this repository; run `elefante memory clear --yes` to do it"
                );
            }
            Ok(Some(("memory.clear", json!({"cwd":cwd}))))
        }
        "learn" | "-h" | "--help" | "help" => Ok(None),
        other => bail!("unknown memory command {other:?}\n\n{USAGE}"),
    }
}

/// Renders one successful daemon administration result using the direct-mode vocabulary.
///
/// # Errors
/// A malformed daemon result or output writer failure.
pub fn render_daemon_result(
    args: &[String],
    result: &Value,
    out: &mut dyn Write,
) -> anyhow::Result<i32> {
    match args.first().map(String::as_str) {
        Some("list") => {
            let rows = result["notes"]
                .as_array()
                .context("memory.list returned no notes")?;
            if rows.is_empty() {
                writeln!(out, "no learned notes for this repository")?;
                return Ok(0);
            }
            writeln!(
                out,
                "{:>5}  {:<10}  {:<10}  {:>4}  {:<9}  note",
                "id", "status", "kind", "conf", "origin"
            )?;
            for row in rows {
                let safe =
                    |field: &str| learn::display_safe(row[field].as_str().unwrap_or_default());
                let raw_origin = safe("origin");
                let origin = raw_origin
                    .strip_prefix("learned:")
                    .unwrap_or(&raw_origin)
                    .to_owned();
                let confidence = row["confidence"].as_f64().unwrap_or(0.0);
                let flag = if row["verified"].as_bool().unwrap_or(false) {
                    ""
                } else {
                    "  [fails verification; never shown]"
                };
                writeln!(
                    out,
                    "{:>5}  {:<10}  {:<10}  {:>4.2}  {:<9}  {}{flag}",
                    row["id"].as_i64().unwrap_or_default(),
                    safe("status"),
                    safe("kind"),
                    confidence,
                    origin,
                    safe("statement")
                )?;
            }
            Ok(0)
        }
        Some("forget") => {
            writeln!(out, "forgot note {}", id_arg(&args[1..], "forget")?)?;
            Ok(0)
        }
        Some("approve") => {
            writeln!(out, "approved note {}", id_arg(&args[1..], "approve")?)?;
            Ok(0)
        }
        Some("clear") => {
            writeln!(
                out,
                "cleared {} learned note(s)",
                result["removed"].as_u64().unwrap_or_default()
            )?;
            Ok(0)
        }
        _ => Ok(0),
    }
}

/// Runs a `memory` subcommand against the repository containing `cwd`.
///
/// `args` are the words after `memory`. Output goes to `out`. Returns the process exit code.
///
/// # Errors
/// An unusable key, an unreadable store, or malformed arguments.
pub fn run(
    args: &[String],
    cwd: &Path,
    key_path: &Path,
    out: &mut dyn Write,
) -> anyhow::Result<i32> {
    let Some((command, rest)) = args.split_first() else {
        writeln!(out, "{USAGE}")?;
        return Ok(2);
    };
    if matches!(command.as_str(), "-h" | "--help" | "help") {
        writeln!(out, "{USAGE}")?;
        return Ok(0);
    }
    if let Some(request) = learn_request(args)? {
        return set_learning(&request, key_path, out);
    }
    let repo = RepoId::resolve(cwd).context("cannot tell which repository this is")?;
    let dir = StoreDir::path_for(repo.root());
    let memory_db = dir.join("memory.db");
    if !memory_db.exists() {
        if command == "list" {
            writeln!(out, "no learned notes for this repository")?;
            return Ok(0);
        }
        bail!("nothing is stored for this repository yet");
    }
    let key = MemKey::load_or_create(key_path).context("cannot read the memory key")?;

    match command.as_str() {
        "list" => {
            let all = match rest {
                [] => false,
                [flag] if flag == "--all" => true,
                _ => bail!("`memory list` takes only --all\n\n{USAGE}"),
            };
            let conn = open_memory_reader(&memory_db).context("cannot open the store")?;
            if !learn::store_stamp_valid(&conn, &key) {
                writeln!(
                    out,
                    "memory store is not from this Elefante installation; no notes shown"
                )?;
                return Ok(0);
            }
            let rows = learn::list(&conn, &key, all, now_ns())?;
            render(&rows, out)?;
            Ok(0)
        }
        "forget" => {
            let id = id_arg(rest, "forget")?;
            let mut conn = open_memory(&memory_db, &key)?;
            if learn::forget(&mut conn, id)? {
                writeln!(out, "forgot note {id}")?;
                Ok(0)
            } else {
                writeln!(out, "no learned note {id}")?;
                Ok(1)
            }
        }
        "approve" => {
            let id = id_arg(rest, "approve")?;
            let mut conn = open_memory(&memory_db, &key)?;
            if learn::approve(&mut conn, &key, id, now_ns())? {
                writeln!(out, "approved note {id}")?;
                Ok(0)
            } else {
                writeln!(out, "no pending note {id}")?;
                Ok(1)
            }
        }
        "clear" => {
            if rest != ["--yes"] {
                writeln!(
                    out,
                    "this deletes every learned note for this repository; run `elefante memory clear --yes` to do it"
                )?;
                return Ok(2);
            }
            let mut conn = open_memory(&memory_db, &key)?;
            let removed = learn::clear(&mut conn)?;
            writeln!(out, "cleared {removed} learned note(s)")?;
            Ok(0)
        }
        other => bail!("unknown memory command {other:?}\n\n{USAGE}"),
    }
}
