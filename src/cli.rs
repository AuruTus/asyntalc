use std::{io::Read, path::PathBuf, time::Duration};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};
use tokio::{io::BufReader, net::UnixStream};

use crate::{
    config::{self, Profile},
    daemon,
    protocol::{self, Operation, Request, Response},
};

#[derive(Parser)]
#[command(version, about)]
pub struct Cli {
    /// Private directory shared by this daemon and its clients.
    #[arg(long, global = true, default_value = ".asyntalc")]
    data_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon with a provider configuration or the development fake runner.
    Daemon {
        /// Explicitly acknowledge the development-only fake runner.
        #[arg(
            long,
            value_enum,
            required_unless_present = "config",
            conflicts_with = "config"
        )]
        runner: Option<Runner>,
        /// TOML configuration for an OpenAI-compatible Chat Completions endpoint.
        #[arg(long, conflicts_with = "runner")]
        config: Option<PathBuf>,
        #[arg(long, requires = "runner", value_parser = clap::value_parser!(u64).range(0..=30_000))]
        fake_delay_ms: Option<u64>,
        /// Maximum concurrent runs across independent sessions.
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..=64))]
        max_active_runs: u64,
    },
    Ping,
    /// Show the daemon's effective workspace permissions and provider destination.
    Scope,
    /// Inspect recorded workspace calls without file contents; continue from next_after.
    Tools {
        #[arg(long)]
        run: String,
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(i64).range(0..))]
        after: i64,
        #[arg(long, default_value_t = protocol::default_page_limit(), value_parser = clap::value_parser!(u32).range(1..=100))]
        limit: u32,
        #[arg(long, value_enum, default_value = "json")]
        output: JsonOutput,
    },
    /// Discover runs in submission order; continue from next_after.
    List {
        #[arg(long)]
        session: Option<String>,
        #[arg(long, value_parser = protocol::RUN_STATUSES)]
        status: Option<String>,
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(i64).range(0..))]
        after: i64,
        #[arg(long, default_value_t = protocol::default_page_limit(), value_parser = clap::value_parser!(u32).range(1..=100))]
        limit: u32,
        #[arg(long, value_enum, default_value = "json")]
        output: JsonOutput,
    },
    /// Read a finite page of durable lifecycle events for one run.
    Logs {
        #[arg(long)]
        run: String,
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(i64).range(0..))]
        after_seq: i64,
        #[arg(long, default_value_t = protocol::default_page_limit(), value_parser = clap::value_parser!(u32).range(1..=100))]
        limit: u32,
        #[arg(long, value_enum, default_value = "json")]
        output: JsonOutput,
    },
    Submit {
        #[arg(long)]
        session: Option<String>,
        /// Run lifetime including queue time, independent of wait timeout.
        #[arg(long, default_value_t = protocol::default_run_timeout_ms(), value_parser = clap::value_parser!(u64).range(1..=86_400_000))]
        run_timeout_ms: u64,
        /// Reuse this key with identical input/options to retry submission safely.
        #[arg(long)]
        idempotency_key: Option<String>,
        /// UTF-8 prompt file, or - for stdin.
        #[arg(long)]
        input: PathBuf,
        #[arg(long, value_enum, default_value = "json")]
        output: JsonOutput,
    },
    /// Answer a persisted parent question and requeue the same run.
    Resume {
        #[arg(long)]
        run: String,
        #[arg(long)]
        question: String,
        /// UTF-8 answer file, or - for stdin.
        #[arg(long)]
        input: PathBuf,
        #[arg(long, value_enum, default_value = "json")]
        output: JsonOutput,
    },
    Cancel {
        #[arg(long)]
        run: String,
        #[arg(long, value_enum, default_value = "json")]
        output: JsonOutput,
    },
    Status {
        #[arg(long)]
        run: String,
        #[arg(long, value_enum, default_value = "json")]
        output: JsonOutput,
    },
    Wait {
        #[arg(long)]
        run: String,
        /// Bounded server wait in milliseconds; does not cancel the run.
        #[arg(long, default_value_t = 20_000, value_parser = clap::value_parser!(u64).range(0..=30_000))]
        timeout_ms: u64,
        #[arg(long, value_enum, default_value = "json")]
        output: JsonOutput,
    },
    Result {
        #[arg(long)]
        run: String,
        #[arg(long, value_enum, default_value = "json")]
        output: ResultOutput,
    },
}

#[derive(Clone, ValueEnum)]
enum Runner {
    Fake,
}
#[derive(Clone, ValueEnum)]
enum JsonOutput {
    Json,
}
#[derive(Clone, ValueEnum)]
enum ResultOutput {
    Json,
    Text,
}

pub async fn run(args: Cli) -> anyhow::Result<()> {
    let (operation, text) = match args.command {
        Command::Daemon {
            runner: _,
            config,
            fake_delay_ms,
            max_active_runs,
        } => {
            let profile = match config {
                Some(path) => Profile::Chat(Box::new(config::load(&path)?)),
                None => Profile::Fake,
            };
            return daemon::run(
                args.data_dir,
                profile,
                Duration::from_millis(fake_delay_ms.unwrap_or(100)),
                max_active_runs as usize,
            )
            .await;
        }
        Command::Ping => (Operation::Ping, false),
        Command::Scope => (Operation::Scope, false),
        Command::Tools {
            run, after, limit, ..
        } => (
            Operation::Tools {
                run_id: run,
                after,
                limit,
            },
            false,
        ),
        Command::List {
            session,
            status,
            after,
            limit,
            ..
        } => (
            Operation::List {
                session_id: session,
                status,
                after,
                limit,
            },
            false,
        ),
        Command::Logs {
            run,
            after_seq,
            limit,
            ..
        } => (
            Operation::Logs {
                run_id: run,
                after_seq,
                limit,
            },
            false,
        ),
        Command::Submit {
            session,
            input,
            run_timeout_ms,
            idempotency_key,
            ..
        } => {
            // Read before any network request, with a bound even for stdin.
            let input = match read_input(input) {
                Ok(input) => input,
                Err(error) => {
                    println!(
                        "{}",
                        serde_json::to_string(&Response::error(
                            None,
                            "invalid_input",
                            &error.to_string()
                        ))?
                    );
                    return Err(error);
                }
            };
            (
                Operation::Submit {
                    session_id: session,
                    input,
                    run_timeout_ms,
                    idempotency_key,
                },
                false,
            )
        }
        Command::Resume {
            run,
            question,
            input,
            ..
        } => {
            let input = match read_input(input) {
                Ok(input) => input,
                Err(error) => {
                    println!(
                        "{}",
                        serde_json::to_string(&Response::error(
                            None,
                            "invalid_input",
                            &error.to_string()
                        ))?
                    );
                    return Err(error);
                }
            };
            (
                Operation::Resume {
                    run_id: run,
                    question_id: question,
                    input,
                },
                false,
            )
        }
        Command::Cancel { run, .. } => (Operation::Cancel { run_id: run }, false),
        Command::Status { run, .. } => (Operation::Status { run_id: run }, false),
        Command::Wait {
            run, timeout_ms, ..
        } => (
            Operation::Wait {
                run_id: run,
                timeout_ms,
            },
            false,
        ),
        Command::Result { run, output } => (
            Operation::Result { run_id: run },
            matches!(output, ResultOutput::Text),
        ),
    };
    let request = Request {
        protocol_version: protocol::VERSION,
        request_id: format!("req_{}", uuid::Uuid::new_v4()),
        operation,
    };
    let response = match exchange(&args.data_dir, &request).await {
        Ok(response) => response,
        Err(error) => {
            if !text {
                println!(
                    "{}",
                    serde_json::to_string(&Response::error(
                        Some(&request.request_id),
                        "transport_error",
                        &error.to_string()
                    ))?
                );
            }
            return Err(error);
        }
    };
    if text {
        if response.ok {
            print!(
                "{}",
                response.body["result"]["text"]
                    .as_str()
                    .context("invalid result response")?
            );
        }
    } else {
        println!("{}", serde_json::to_string(&response)?);
    }
    if !response.ok {
        bail!("{}", response.body["error"]);
    }
    Ok(())
}

fn read_input(path: PathBuf) -> anyhow::Result<String> {
    let reader: Box<dyn Read> = if path.as_os_str() == "-" {
        Box::new(std::io::stdin())
    } else {
        Box::new(std::fs::File::open(&path).context("cannot open input file")?)
    };
    let mut bytes = Vec::new();
    reader
        .take((protocol::MAX_INPUT + 1) as u64)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= protocol::MAX_INPUT,
        "input exceeds 64 KiB limit"
    );
    let input = String::from_utf8(bytes).context("input must be UTF-8")?;
    anyhow::ensure!(!input.trim().is_empty(), "input is empty");
    Ok(input)
}

async fn exchange(data_dir: &std::path::Path, request: &Request) -> anyhow::Result<Response> {
    let timeout_ms = match request.operation {
        Operation::Wait { timeout_ms, .. } => timeout_ms + 5_000,
        _ => 5_000,
    };
    tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        let mut socket = UnixStream::connect(data_dir.join("daemon.sock"))
            .await
            .context("cannot connect to daemon; start `asyntalc daemon --config FILE` or `asyntalc daemon --runner fake` first")?;
        protocol::write_frame(&mut socket, request).await?;
        let bytes = protocol::read_frame(&mut BufReader::new(socket)).await?;
        let response: Response = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            response.protocol_version == protocol::VERSION,
            "unsupported response version"
        );
        anyhow::ensure!(
            response.request_id.as_deref() == Some(&request.request_id),
            "response request ID mismatch"
        );
        Ok(response)
    })
    .await
    .context("daemon response timed out; submitted work may still be running")?
}
