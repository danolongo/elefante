use std::{
    env,
    io::{self, Read as _, Write as _},
};

use anyhow::{Context as _, bail};
use elefante_core::{OutputEvent, RunConfig, run_agent_loop};
use elefante_provider::{Effort, OpenRouterProvider, StopReason};
use elefante_tools::ToolExecutor;

const USAGE: &str = "usage: elefante [--model MODEL] [--effort LEVEL] [--max-rounds N] [--max-tokens N] [PROMPT ...]\n\nIf PROMPT is omitted, it is read from stdin. Required: OPENROUTER_API_KEY. Optional: OPENROUTER_MODEL, OPENROUTER_BASE_URL, ELEFANTE_EFFORT.";

struct CliOptions {
    prompt: Option<String>,
    model: String,
    effort: Effort,
    max_rounds: usize,
    max_tokens: u32,
}

impl CliOptions {
    fn parse() -> anyhow::Result<Self> {
        let mut arguments = env::args().skip(1);
        let mut prompt_parts = Vec::new();
        let mut model =
            env::var("OPENROUTER_MODEL").unwrap_or_else(|_| "anthropic/claude-opus-5".to_owned());
        let mut effort =
            parse_effort(&env::var("ELEFANTE_EFFORT").unwrap_or_else(|_| "high".to_owned()))?;
        let mut max_rounds = 24;
        let mut max_tokens = 16_384;

        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "-h" | "--help" => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                "--model" => model = next_value(&mut arguments, "--model")?,
                "--effort" => {
                    effort = parse_effort(&next_value(&mut arguments, "--effort")?)?;
                }
                "--max-rounds" => {
                    max_rounds =
                        parse_number(&next_value(&mut arguments, "--max-rounds")?, "--max-rounds")?;
                }
                "--max-tokens" => {
                    max_tokens =
                        parse_number(&next_value(&mut arguments, "--max-tokens")?, "--max-tokens")?;
                }
                "--" => {
                    prompt_parts.extend(&mut arguments);
                    break;
                }
                flag if flag.starts_with('-') => bail!("unknown option {flag:?}\n\n{USAGE}"),
                part => prompt_parts.push(part.to_owned()),
            }
        }

        Ok(Self {
            prompt: (!prompt_parts.is_empty()).then(|| prompt_parts.join(" ")),
            model,
            effort,
            max_rounds,
            max_tokens,
        })
    }
}

fn next_value(arguments: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<String> {
    arguments
        .next()
        .with_context(|| format!("{flag} requires a value"))
}

fn parse_number<T>(value: &str, flag: &str) -> anyhow::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let parsed = value
        .parse::<T>()
        .with_context(|| format!("invalid value {value:?} for {flag}"))?;
    Ok(parsed)
}

fn parse_effort(value: &str) -> anyhow::Result<Effort> {
    match value {
        "low" => Ok(Effort::Low),
        "medium" => Ok(Effort::Medium),
        "high" => Ok(Effort::High),
        "xhigh" => Ok(Effort::XHigh),
        "max" => Ok(Effort::Max),
        _ => bail!("invalid effort {value:?}; expected low, medium, high, xhigh, or max"),
    }
}

fn read_prompt(argument: Option<String>) -> anyhow::Result<String> {
    let mut prompt = if let Some(prompt) = argument {
        prompt
    } else {
        let mut input = String::new();
        io::stdin()
            .read_to_string(&mut input)
            .context("failed to read prompt from stdin")?;
        input
    };
    while prompt.ends_with(['\n', '\r']) {
        prompt.pop();
    }
    if prompt.trim().is_empty() {
        bail!("prompt cannot be empty\n\n{USAGE}");
    }
    Ok(prompt)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let options = CliOptions::parse()?;
    let prompt = read_prompt(options.prompt)?;
    let api_key = env::var("OPENROUTER_API_KEY").context("OPENROUTER_API_KEY is not set")?;
    let mut provider = OpenRouterProvider::new(api_key);
    if let Ok(base_url) = env::var("OPENROUTER_BASE_URL") {
        provider = provider
            .with_base_url(base_url)
            .context("OPENROUTER_BASE_URL is invalid")?;
    }
    let cwd = env::current_dir().context("failed to determine current directory")?;
    let executor = ToolExecutor::new(&cwd)
        .with_context(|| format!("failed to initialize tools in {}", cwd.display()))?;
    let mut config = RunConfig::new(options.model);
    config.effort = Some(options.effort);
    config.max_rounds = options.max_rounds;
    config.max_tokens = options.max_tokens;

    let outcome = run_agent_loop(&provider, &executor, prompt, config, |event| match event {
        OutputEvent::ReasoningDelta(delta) | OutputEvent::TextDelta(delta) => {
            print!("{delta}");
            let _ = io::stdout().flush();
        }
        OutputEvent::ToolStarted { .. } | OutputEvent::ToolFinished { .. } => {}
    })
    .await?;

    if outcome.stop_reason == StopReason::Refusal {
        eprintln!("\nprovider refused the request");
    } else if !outcome.text.ends_with('\n') {
        println!();
    }
    Ok(())
}
