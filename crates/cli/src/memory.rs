//! `elefante memory`: look at what was learned about a repository, and remove it.
//!
//! These commands open the repository's store directly and never need the daemon. Persisted
//! free-form text is injected into prompts, so a person must always be able to see it and take
//! it out. A running daemon notices a change within its cache lifetime (about thirty seconds).

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

pub const USAGE: &str = "usage: elefante memory <command>\n\ncommands:\n  list [--all]     show what was learned about this repository (--all includes superseded)\n  forget <id>      delete one learned note\n  approve <id>     accept a note only the assistant had vouched for\n  clear --yes      delete every learned note for this repository\n  learn on|off     allow or decline learning from conversations";

/// Returns the daemon answer for a `memory learn` command, or `None` for another subcommand.
///
/// # Errors
/// A learn command must contain exactly `on` or `off`.
pub fn learn_answer(args: &[String]) -> anyhow::Result<Option<ConsentAnswer>> {
    match args {
        [command, setting] if command == "learn" => match setting.as_str() {
            "on" => Ok(Some(ConsentAnswer::Learn)),
            "off" => Ok(Some(ConsentAnswer::Off)),
            _ => bail!("`memory learn` expects on or off\n\n{USAGE}"),
        },
        [command, ..] if command == "learn" => {
            bail!("`memory learn` takes exactly one setting\n\n{USAGE}")
        }
        _ => Ok(None),
    }
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

fn set_learning(
    answer: ConsentAnswer,
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
            answer,
            extractor: configured_extractor().trim().to_owned(),
        },
    )
    .context("cannot save memory consent")?;
    writeln!(
        out,
        "memory learning is {}",
        if answer == ConsentAnswer::Learn {
            "granted"
        } else {
            "declined"
        }
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
    if let Some(answer) = learn_answer(args)? {
        return set_learning(answer, key_path, out);
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
