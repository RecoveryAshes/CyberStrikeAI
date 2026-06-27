mod cancellation;
mod compaction;
mod event_protocol;
mod filesystem_runtime;
mod grpc_protocol;
mod grpc_server;
mod knowledge_runtime;
mod mcp_bridge;
mod mcp_registry;
mod model_stream;
mod permission;
mod plan_store;
mod runtime_state;
mod session_loop;
mod session_store;
mod skill_runtime;
mod submission_loop;
mod tool_registry;
mod tool_runtime;
mod turn_loop;

use std::io::{self, BufRead, Write};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use cancellation::CancellationRegistry;
use event_protocol::{RuntimeCommand, RuntimeEvent};
use runtime_state::RuntimeStateStore;
use submission_loop::SubmissionLoop;

#[tokio::main]
async fn main() -> Result<()> {
    let args = RuntimeArgs::parse();
    let cancellations = CancellationRegistry::default();
    let submission_loop = SubmissionLoop::with_cancellations(cancellations.clone());
    if args.transport == "grpc" {
        let runtime_state = RuntimeStateStore::new_required(args.redis_addr, args.redis_prefix)
            .map_err(anyhow::Error::msg)?;
        return grpc_server::serve_grpc(
            &args.listen,
            submission_loop,
            cancellations,
            runtime_state,
        )
        .await;
    }
    run_jsonl(submission_loop, cancellations)
}

#[derive(Debug, Clone)]
struct RuntimeArgs {
    transport: String,
    listen: String,
    redis_addr: Option<String>,
    redis_prefix: String,
}

impl RuntimeArgs {
    fn parse() -> Self {
        let mut transport = "jsonl".to_string();
        let mut listen = "127.0.0.1:0".to_string();
        let mut redis_addr = None;
        let mut redis_prefix = "csai:agent_runtime:".to_string();
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--transport" => {
                    if let Some(value) = args.next() {
                        transport = value;
                    }
                }
                "--listen" | "--grpc-listen" => {
                    if let Some(value) = args.next() {
                        listen = value;
                    }
                }
                "--redis-addr" => {
                    redis_addr = args
                        .next()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());
                }
                "--redis-prefix" => {
                    if let Some(value) = args.next() {
                        if !value.trim().is_empty() {
                            redis_prefix = value;
                        }
                    }
                }
                _ => {}
            }
        }
        Self {
            transport,
            listen,
            redis_addr,
            redis_prefix,
        }
    }
}

fn run_jsonl(submission_loop: SubmissionLoop, cancellations: CancellationRegistry) -> Result<()> {
    let stdin = io::stdin();
    let (event_tx, event_rx) = mpsc::channel::<RuntimeEvent>();
    let writer = thread::spawn(move || -> Result<()> {
        let mut stdout = io::BufWriter::new(io::stdout().lock());
        for event in event_rx {
            write_event(&mut stdout, &event)?;
            stdout.flush().context("flush runtime event")?;
        }
        Ok(())
    });
    let mut workers: Vec<JoinHandle<()>> = Vec::new();

    for line in stdin.lock().lines() {
        reap_finished_workers(&mut workers);
        let line = line.context("read runtime command")?;
        if line.trim().is_empty() {
            continue;
        }

        let command: RuntimeCommand = match serde_json::from_str(&line) {
            Ok(command) => command,
            Err(err) => {
                let _ = event_tx.send(RuntimeEvent::runtime_error("", "", err.to_string()));
                continue;
            }
        };

        if matches!(
            command,
            RuntimeCommand::StartTurn { .. } | RuntimeCommand::ApprovalResponse { .. }
        ) {
            let loop_state = submission_loop.clone();
            let tx = event_tx.clone();
            workers.push(thread::spawn(move || {
                let completion_ids = command_completion_ids(&command);
                loop_state.handle_with_event_sink(command, &mut |event| {
                    let _ = tx.send(event);
                });
                let _ = tx.send(RuntimeEvent::CommandCompleted {
                    command_id: completion_ids.0,
                    conversation_id: completion_ids.1,
                    runtime_session_id: completion_ids.2,
                });
            }));
        } else {
            let completion_ids = command_completion_ids(&command);
            match command {
                RuntimeCommand::InterruptTurn {
                    conversation_id,
                    reason,
                    continue_after,
                    ..
                } => {
                    if !cancellations.cancel(&conversation_id, reason.clone(), continue_after) {
                        submission_loop.handle_with_event_sink(
                            RuntimeCommand::InterruptTurn {
                                command_id: completion_ids.0.clone(),
                                conversation_id,
                                reason,
                                continue_after,
                            },
                            &mut |event| {
                                let _ = event_tx.send(event);
                            },
                        );
                    }
                }
                other => {
                    submission_loop.handle_with_event_sink(other, &mut |event| {
                        let _ = event_tx.send(event);
                    });
                }
            }
            event_tx
                .send(RuntimeEvent::CommandCompleted {
                    command_id: completion_ids.0,
                    conversation_id: completion_ids.1,
                    runtime_session_id: completion_ids.2,
                })
                .ok();
        }
    }

    for worker in workers {
        worker.join().expect("runtime worker panicked");
    }
    drop(event_tx);
    writer.join().expect("runtime writer panicked")?;

    Ok(())
}

fn reap_finished_workers(workers: &mut Vec<JoinHandle<()>>) {
    let mut index = 0;
    while index < workers.len() {
        if workers[index].is_finished() {
            workers
                .swap_remove(index)
                .join()
                .expect("runtime worker panicked");
        } else {
            index += 1;
        }
    }
}

fn command_completion_ids(command: &RuntimeCommand) -> (String, String, String) {
    match command {
        RuntimeCommand::StartTurn {
            command_id,
            conversation_id,
            runtime_session_id,
            ..
        }
        | RuntimeCommand::ApprovalResponse {
            command_id,
            conversation_id,
            runtime_session_id,
            ..
        } => (
            command_id.clone(),
            conversation_id.clone(),
            runtime_session_id.clone().unwrap_or_default(),
        ),
        RuntimeCommand::InterruptTurn {
            command_id,
            conversation_id,
            ..
        } => (command_id.clone(), conversation_id.clone(), String::new()),
        RuntimeCommand::Shutdown => (String::new(), String::new(), String::new()),
    }
}

fn write_event<W: Write>(writer: &mut W, event: &RuntimeEvent) -> Result<()> {
    serde_json::to_writer(&mut *writer, event).context("serialize runtime event")?;
    writer.write_all(b"\n").context("write runtime event")?;
    Ok(())
}
