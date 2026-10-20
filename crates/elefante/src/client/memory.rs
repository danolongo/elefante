//! `elefante memory`: look at what was learned about a repository, and remove it.
//!
//! Every command goes through the daemon (`memory.*` over the socket), starting one when none is
//! listening, so a change takes effect on its next retrieval and one process stays the only writer.
//! Persisted free-form text is injected into prompts, so a person must always be able to see it and
//! take it out.

use std::{env, io::Write, path::Path};

use anyhow::{Context, bail};
use elefante_mem::{consent::ConsentAnswer, learn};
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

fn id_arg(args: &[String], command: &str) -> anyhow::Result<i64> {
    let [id] = args else {
        bail!("`memory {command}` takes exactly one note id\n\n{USAGE}");
    };
    id.parse()
        .with_context(|| format!("{id:?} is not a note id"))
}

/// What one `elefante memory …` invocation asks for.
#[derive(Debug, PartialEq)]
pub enum Command {
    /// Print the usage text and exit with this code (0 for `help`, 2 for no command).
    Usage(i32),
    /// Print this explanation and exit with 2: the command needs something the person must type.
    Confirm(&'static str),
    /// `memory learn on|off`, answered by `memory.consent`.
    Learn(LearnRequest),
    /// A repository command: the daemon method and its params.
    Repository(&'static str, Value),
}

/// Parses the words after `memory`. Nothing here touches the socket, so a malformed command is
/// refused before a daemon is started.
///
/// # Errors
/// Malformed arguments or an unknown command.
pub fn parse(args: &[String], cwd: &Path) -> anyhow::Result<Command> {
    let Some((command, rest)) = args.split_first() else {
        return Ok(Command::Usage(2));
    };
    if let Some(request) = learn_request(args)? {
        return Ok(Command::Learn(request));
    }
    let cwd = cwd.to_string_lossy();
    match command.as_str() {
        "-h" | "--help" | "help" => Ok(Command::Usage(0)),
        "list" => {
            let all = match rest {
                [] => false,
                [flag] if flag == "--all" => true,
                _ => bail!("`memory list` takes only --all\n\n{USAGE}"),
            };
            Ok(Command::Repository(
                "memory.list",
                json!({"cwd":cwd,"all":all}),
            ))
        }
        "forget" | "approve" => {
            let id = id_arg(rest, command)?;
            let method = if command == "forget" {
                "memory.forget"
            } else {
                "memory.approve"
            };
            Ok(Command::Repository(method, json!({"cwd":cwd,"id":id})))
        }
        "clear" => {
            if rest != ["--yes"] {
                return Ok(Command::Confirm(
                    "this deletes every learned note for this repository; run `elefante memory clear --yes` to do it",
                ));
            }
            Ok(Command::Repository("memory.clear", json!({"cwd":cwd})))
        }
        other => bail!("unknown memory command {other:?}\n\n{USAGE}"),
    }
}

/// Renders one successful daemon result for a repository command.
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
