use crate::{
    config::Profile,
    provider::{ChatProvider, Completion, Failure, Turn, Usage},
    store::{Store, StoreError, Work, now_ms},
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    sync::{Notify, watch},
    task::JoinSet,
};

pub struct Executor {
    pub delay: Duration,
    pub profile: Profile,
    pub provider: Option<ChatProvider>,
}

impl Executor {
    async fn execute(&self, store: &Store, work: &Work) -> anyhow::Result<Result<Turn, Failure>> {
        let outcome = match &self.profile {
            Profile::Fake => {
                tokio::time::sleep(self.delay).await;
                Ok(Turn::Complete(Completion {
                    text: format!("[fake] {}", work.input),
                    finish_reason: "stop".into(),
                    usage: Usage::default(),
                }))
            }
            Profile::Chat(config) => {
                match store
                    .context(work, config.max_context_bytes - config.system_prompt.len())
                    .await
                {
                    Ok(messages) => {
                        match store.mark_requested(work.run_id.clone()).await {
                            Ok(true) => {}
                            Ok(false) => {
                                return Ok(Err(Failure::new(
                                    "stopped",
                                    "Run stopped before request",
                                )));
                            }
                            Err(error)
                                if error
                                    .downcast_ref::<StoreError>()
                                    .is_some_and(|e| e.0 == "model_turn_limit") =>
                            {
                                return Ok(Err(Failure::new(
                                    "model_turn_limit",
                                    "Run exceeds nine model requests",
                                )));
                            }
                            Err(error) => return Err(error),
                        }
                        self.provider
                            .as_ref()
                            .expect("chat profile has a provider")
                            .complete(messages)
                            .await
                    }
                    Err(error)
                        if error
                            .downcast_ref::<StoreError>()
                            .is_some_and(|e| e.0 == "context_limit") =>
                    {
                        Err(Failure::new(
                            "context_limit",
                            "Conversation exceeds the configured context byte limit; start a new session",
                        ))
                    }
                    Err(error) => return Err(error),
                }
            }
        };
        Ok(outcome)
    }
}

pub async fn run(
    store: Store,
    pending: Arc<Notify>,
    changes: watch::Sender<u64>,
    executor: Executor,
    max_active: usize,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let executor = Arc::new(executor);
    let mut jobs = JoinSet::new();
    let mut active = HashMap::<String, watch::Sender<bool>>::new();
    let outcome: anyhow::Result<()> = async {
        loop {
            let (stopping, next_deadline, changed) = store.maintain().await?;
            if changed { changes.send_modify(|r| *r = r.wrapping_add(1)); }
            for id in stopping {
                if let Some(stop) = active.get(&id) { stop.send_replace(true); }
            }
            while jobs.len() < max_active {
                let Some(work) = store.claim_available(active.keys().cloned().collect()).await? else { break; };
                let (stop, mut stop_rx) = watch::channel(false);
                active.insert(work.run_id.clone(), stop);
                let store = store.clone();
                let executor = executor.clone();
                let changes = changes.clone();
                changes.send_modify(|r| *r = r.wrapping_add(1));
                jobs.spawn(async move {
                    // Dropping the provider future closes the local request before releasing the session.
                    let result = tokio::select! {
                        biased;
                        _ = stop_rx.changed() => Err(Failure::new("stopped", "Run stopped")),
                        outcome = executor.execute(&store, &work) => outcome?,
                    };
                    let id = work.run_id.clone();
                    match result {
                        Ok(Turn::Complete(completion)) => store.complete(work, completion).await?,
                        Ok(Turn::Question(question)) => store.pause(work, question).await?,
                        Err(failure) => store.fail(work.run_id, failure).await?,
                    }
                    changes.send_modify(|r| *r = r.wrapping_add(1));
                    Ok::<_,anyhow::Error>(id)
                });
            }
            // Recheck the wall clock at least once a second, including after clock adjustments.
            let delay_ms = next_deadline.map_or(1000, |d| (d-now_ms()).clamp(0,1000) as u64);
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                Some(result) = jobs.join_next(), if !jobs.is_empty() => { active.remove(&result??); }
                _ = pending.notified() => { changes.send_modify(|r| *r = r.wrapping_add(1)); }
                _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
            }
        }
        Ok(())
    }.await;
    // Drain every task before the daemon releases its directory lock, including on errors.
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
    store.barrier().await?;
    outcome
}
