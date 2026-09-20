use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use serde_json::json;
use tokio::{
    io::BufReader,
    net::{UnixListener, UnixStream},
    sync::{Notify, Semaphore, watch},
    task::JoinSet,
    time::{Instant, timeout},
};

use crate::{
    config::Profile,
    protocol::{self, Operation, Request, Response},
    provider::ChatProvider,
    scheduler::{self, Executor},
    store::{Store, StoreError},
};

struct Ownership {
    _lock: File,
    socket: PathBuf,
}
impl Drop for Ownership {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
    }
}

pub async fn run(
    data_dir: PathBuf,
    profile: Profile,
    fake_delay: Duration,
    max_active_runs: usize,
) -> anyhow::Result<()> {
    let runner_name = profile.name();
    let provider = match &profile {
        Profile::Fake => None,
        Profile::Chat(config) => Some(ChatProvider::new(config.as_ref().clone())?),
    };
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&data_dir)?;
    let metadata = fs::symlink_metadata(&data_dir)?;
    anyhow::ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
        "data directory must be a private directory (mode 0700)"
    );
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(data_dir.join("daemon.lock"))?;
    lock.try_lock()
        .context("another daemon owns this data directory")?;
    let socket = data_dir.join("daemon.sock");
    // Only the lock owner may remove a stale socket. Never unlink arbitrary files.
    if let Ok(metadata) = fs::symlink_metadata(&socket) {
        anyhow::ensure!(
            metadata.file_type().is_socket(),
            "socket path exists and is not a socket"
        );
        fs::remove_file(&socket)?;
    }
    let store = Store::open(&data_dir.join("state.sqlite3"), &profile)?;
    let listener = UnixListener::bind(&socket)?;
    let _ownership = Ownership {
        _lock: lock,
        socket: socket.clone(),
    };
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let pending = Arc::new(Notify::new());
    let (changes, _) = watch::channel(0_u64);
    let (shutdown, shutdown_rx) = watch::channel(false);
    let mut worker = tokio::spawn(scheduler::run(
        store.clone(),
        pending.clone(),
        changes.clone(),
        Executor {
            delay: fake_delay,
            profile,
            provider,
        },
        max_active_runs,
        shutdown_rx,
    ));
    let mut worker_joined = false;
    let slots = Arc::new(Semaphore::new(64));
    let mut clients = JoinSet::new();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    eprintln!(
        "daemon ready: {} ({} runner)",
        socket.display(),
        runner_name
    );
    let outcome = loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (socket, _) = match accepted { Ok(value) => value, Err(error) => break Err(error.into()) };
                let Ok(permit) = slots.clone().try_acquire_owned() else { drop(socket); continue; };
                let store = store.clone();
                let pending = pending.clone();
                let changes = changes.subscribe();
                clients.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = serve(socket, store, pending, changes, runner_name).await { eprintln!("client connection: {error}"); }
                });
            }
            Some(result) = clients.join_next(), if !clients.is_empty() => {
                if let Err(error) = result { eprintln!("client task: {error}"); }
            }
            result = &mut worker => {
                worker_joined = true;
                break match result {
                    Ok(Err(error)) => Err(error),
                    Ok(Ok(())) => Err(anyhow::anyhow!("worker stopped unexpectedly")),
                    Err(error) => Err(error.into()),
                };
            }
            signal = tokio::signal::ctrl_c() => { break signal.map_err(Into::into); }
            _ = terminate.recv() => { break Ok(()); }
        }
    };
    clients.abort_all();
    while clients.join_next().await.is_some() {}
    shutdown.send_replace(true);
    if !worker_joined {
        worker.await.context("scheduler task failed")??;
    }
    // All accepted DB jobs finish before the store barrier returns. Keep the lock until then.
    // Store clones held by aborted tasks have now been dropped.
    store.barrier().await?;
    outcome
}

async fn serve(
    socket: UnixStream,
    store: Store,
    pending: Arc<Notify>,
    changes: watch::Receiver<u64>,
    runner_name: &'static str,
) -> anyhow::Result<()> {
    let (reader, mut writer) = socket.into_split();
    let frame = timeout(
        Duration::from_secs(5),
        protocol::read_frame(&mut BufReader::new(reader)),
    )
    .await;
    let request: Result<Request, _> = match frame {
        Ok(Ok(bytes)) => serde_json::from_slice(&bytes),
        _ => {
            return send(
                &mut writer,
                &Response::error(
                    None,
                    "invalid_frame",
                    "Request is incomplete, oversized, or timed out",
                ),
            )
            .await;
        }
    };
    let request = match request {
        Ok(request) => request,
        Err(_) => {
            return send(
                &mut writer,
                &Response::error(None, "invalid_request", "Invalid request JSON or operation"),
            )
            .await;
        }
    };
    let id = &request.request_id;
    let response = if request.protocol_version != protocol::VERSION {
        Response::error(
            Some(id),
            "unsupported_version",
            "Expected protocol version 1",
        )
    } else if id.is_empty() || id.len() > 128 {
        Response::error(
            None,
            "invalid_request",
            "Request ID must contain 1 to 128 bytes",
        )
    } else {
        match dispatch(&request, &store, pending, changes, runner_name).await {
            Ok(body) => Response::success(id, body),
            Err(error) => {
                if let Some(error) = error.downcast_ref::<StoreError>() {
                    Response::error(Some(id), error.0, error.0)
                } else {
                    eprintln!("request failed: {error:#}");
                    Response::error(
                        Some(id),
                        "internal_error",
                        "Daemon could not complete the operation",
                    )
                }
            }
        }
    };
    send(&mut writer, &response).await
}

async fn send(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &Response,
) -> anyhow::Result<()> {
    timeout(
        Duration::from_secs(5),
        protocol::write_frame(writer, response),
    )
    .await
    .context("response write timed out")?
}

async fn dispatch(
    request: &Request,
    store: &Store,
    pending: Arc<Notify>,
    mut changes: watch::Receiver<u64>,
    runner_name: &'static str,
) -> anyhow::Result<serde_json::Value> {
    match &request.operation {
        Operation::Ping => Ok(json!({"ready": true, "runner": runner_name})),
        Operation::List {
            session_id,
            status,
            after,
            limit,
        } => {
            store
                .list(session_id.clone(), status.clone(), *after, *limit)
                .await
        }
        Operation::Logs {
            run_id,
            after_seq,
            limit,
        } => store.logs(run_id.clone(), *after_seq, *limit).await,
        Operation::Submit {
            session_id,
            input,
            run_timeout_ms,
            idempotency_key,
        } => {
            if input.trim().is_empty() || input.len() > protocol::MAX_INPUT {
                return Err(StoreError("invalid_input").into());
            }
            if session_id.as_ref().is_some_and(|id| {
                id.is_empty()
                    || id.len() > 128
                    || !id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            }) {
                return Err(StoreError("invalid_session_id").into());
            }
            if *run_timeout_ms == 0 || *run_timeout_ms > protocol::MAX_RUN_TIMEOUT_MS {
                return Err(StoreError("invalid_run_timeout").into());
            }
            if idempotency_key.as_ref().is_some_and(|key| {
                key.is_empty()
                    || key.len() > 128
                    || !key
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
            }) {
                return Err(StoreError("invalid_idempotency_key").into());
            }
            let receipt = store
                .submit(
                    session_id.clone(),
                    input.clone(),
                    *run_timeout_ms,
                    idempotency_key.clone(),
                )
                .await?;
            // Notification happens even if the client disconnected after submission.
            pending.notify_one();
            Ok(receipt)
        }
        Operation::Resume {
            run_id,
            question_id,
            input,
        } => {
            if input.trim().is_empty() || input.len() > protocol::MAX_INPUT {
                return Err(StoreError("invalid_input").into());
            }
            if question_id.is_empty() || question_id.len() > 128 {
                return Err(StoreError("invalid_question_id").into());
            }
            let result = store
                .resume(run_id.clone(), question_id.clone(), input.clone())
                .await;
            pending.notify_one();
            result
        }
        Operation::Cancel { run_id } => {
            store.cancel(run_id.clone()).await?;
            pending.notify_one();
            let mut snapshot = serde_json::to_value(store.snapshot(run_id.clone(), false).await?)?;
            snapshot["return_reason"] = json!("snapshot");
            Ok(snapshot)
        }
        Operation::Status { run_id } => {
            let mut snapshot = serde_json::to_value(store.snapshot(run_id.clone(), false).await?)?;
            snapshot["return_reason"] = json!("snapshot");
            Ok(snapshot)
        }
        Operation::Result { run_id } => store.result(run_id.clone()).await,
        Operation::Wait { run_id, timeout_ms } => {
            if *timeout_ms > protocol::MAX_WAIT_MS {
                return Err(StoreError("invalid_timeout").into());
            }
            let deadline = Instant::now() + Duration::from_millis(*timeout_ms);
            loop {
                let snapshot = store.snapshot(run_id.clone(), true).await?;
                let terminal = matches!(
                    snapshot.status.as_str(),
                    "completed" | "failed" | "cancelled" | "timed_out"
                );
                let input_required = snapshot.status == "waiting_for_parent";
                let mut value = serde_json::to_value(snapshot)?;
                if terminal || input_required || Instant::now() >= deadline {
                    value["return_reason"] = json!(if terminal {
                        "terminal"
                    } else if input_required {
                        "input_required"
                    } else {
                        "wait_timeout"
                    });
                    return Ok(value);
                }
                // The receiver was subscribed before reading state. Read again even on timeout.
                tokio::select! {
                    changed = changes.changed() => { changed.context("worker notification channel closed")?; }
                    _ = tokio::time::sleep_until(deadline) => {}
                }
            }
        }
    }
}
