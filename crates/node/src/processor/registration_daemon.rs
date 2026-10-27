//! The registration daemons of a processor, one per registration task. A daemon publishes at
//! every heartbeat, and early at the first admitted edge after a heartbeat that began alone
//! (#855). It asks for store reports until its registration is ready (#866).

use super::*;

/// Sleep `interval` unless `stop` or `sibling_stop` fires first: `Ok(true)` once the interval
/// has elapsed, `Ok(false)` on a stop. The pause of every periodic node task between two runs.
pub(crate) async fn sleep_unless_stopped(
    interval: Duration,
    stop: &StopToken,
    sibling_stop: &StopToken,
) -> Result<bool> {
    use futures::future::select;
    use futures::pin_mut;

    let stopped = async {
        let own_stop = stop.stopped();
        let maintenance_stop = sibling_stop.stopped();
        pin_mut!(own_stop, maintenance_stop);
        let _ = select(own_stop, maintenance_stop).await;
    };
    let interval_elapsed = sleep(interval);
    pin_mut!(stopped, interval_elapsed);
    match select(interval_elapsed, stopped).await {
        futures::future::Either::Left((result, _)) => {
            result?;
            Ok(true)
        }
        futures::future::Either::Right(((), _)) => Ok(false),
    }
}

/// What starts a registration publish after the first: its heartbeat, or an early publish at
/// its first admitted edge (#855).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegistrationTurn {
    /// The heartbeat interval elapsed.
    Heartbeat,
    /// An armed registration saw an admitted edge (`registration::republish_due`).
    Early,
}

impl Processor {
    /// Publish one registration of `task`, the registration task numbered `index`. Until the
    /// task is ready (`registration::RegistrationReadiness`, #866), its appends ask their owners
    /// for store reports, and a publish that began with an admitted edge (`edges_at_start`)
    /// awaits them as a node task, which makes the task ready once all report it stored.
    async fn run_registration_once(
        &self,
        task: &dyn RegistrationTask,
        index: usize,
        stop: StopToken,
        edges_at_start: usize,
    ) -> Result<()> {
        let context = RegistrationContext::new_with_stop(self, stop);
        let context = match self.registration_readiness.is_ready(index) {
            true => context,
            false => context.reporting(),
        };
        let published = task.register_once(&context).await;
        if let (Ok(()), Some(receipts)) = (&published, context.into_receipts()) {
            if edges_at_start > 0 {
                let wait = self.registration_readiness.wait(index, receipts);
                if self.tasks.spawn(wait).is_err() {
                    tracing::debug!("no runtime to await the store reports of a registration");
                }
            }
        }
        published
    }

    pub(super) async fn registration_task_daemon_with(
        &self,
        task: &dyn RegistrationTask,
        index: usize,
        stop: StopToken,
        sibling_stop: StopToken,
    ) {
        let mut turn = RegistrationTurn::Heartbeat;
        loop {
            if stop.should_stop() || sibling_stop.should_stop() {
                return;
            }
            let edges_at_start = self.swarm.admitted_edges();
            let armed = early_publish_armed(turn == RegistrationTurn::Heartbeat, edges_at_start);
            let published = self
                .run_registration_once(task, index, stop.clone(), edges_at_start)
                .await;
            // A publish is a state change tests probe for.
            #[cfg(all(test, feature = "node"))]
            crate::tests::activity::record_activity();
            if let Err(error) = published {
                if matches!(error, Error::RegistrationStopped) {
                    tracing::debug!(
                        "Stopping {} registration task after cooperative stop",
                        task.name()
                    );
                    return;
                }
                tracing::warn!("Failed to run {} registration task: {error:?}", task.name());
            }
            if stop.should_stop() || sibling_stop.should_stop() {
                return;
            }
            match self
                .next_registration_turn(task.interval(), armed, &stop, &sibling_stop)
                .await
            {
                Ok(Some(next)) => turn = next,
                Ok(None) => return,
                Err(error) => {
                    tracing::warn!(
                        "Stopping {} registration task after timer error: {error:?}",
                        task.name()
                    );
                    return;
                }
            }
        }
    }

    /// The next turn of a registration daemon: its heartbeat after `interval`, or earlier, at
    /// the first admitted edge of an `armed` registration (`registration::republish_due`, #855);
    /// `None` once a stop is requested. Woken by the timer, an edge change or the stop, never by
    /// polling.
    async fn next_registration_turn(
        &self,
        interval: Duration,
        armed: bool,
        stop: &StopToken,
        sibling_stop: &StopToken,
    ) -> Result<Option<RegistrationTurn>> {
        let heartbeat = sleep_unless_stopped(interval, stop, sibling_stop);
        let first_edge = self.first_admitted_edge(armed);
        futures::pin_mut!(heartbeat, first_edge);
        match futures::future::select(heartbeat, first_edge).await {
            futures::future::Either::Left((slept, _)) => {
                Ok(slept?.then_some(RegistrationTurn::Heartbeat))
            }
            futures::future::Either::Right(((), _)) => Ok(Some(RegistrationTurn::Early)),
        }
    }

    /// Resolves once an early publish is due for an `armed` registration, woken by the swarm's
    /// edge changes (its edge-watch law); never for one not armed.
    async fn first_admitted_edge(&self, armed: bool) {
        if !armed {
            return std::future::pending().await;
        }
        loop {
            let changed = self.swarm.edges_changed();
            if republish_due(armed, self.swarm.admitted_edges()) {
                return;
            }
            changed.await;
        }
    }

    pub(super) async fn registration_daemons_with(&self, stop: StopToken, sibling_stop: StopToken) {
        join_all(
            self.registration_tasks
                .iter()
                .enumerate()
                .map(|(index, task)| {
                    self.registration_task_daemon_with(
                        task.as_ref(),
                        index,
                        stop.clone(),
                        sibling_stop.clone(),
                    )
                }),
        )
        .await;
    }

    /// Resolves once every registration of this node is stored at its owners, as they
    /// reported it (#866): for each registration task, a publish that began with an admitted
    /// edge was reported stored by the owner of every remote placement
    /// (`registration::RegistrationReadiness`). It resolves at once for a node with no
    /// registration, and needs a running listener otherwise. A registry read by another node
    /// then reaches the registrations at their owners, unless an owner has lost its placement
    /// since.
    pub async fn registrations_published(&self) {
        self.registration_readiness.all().await
    }
}
