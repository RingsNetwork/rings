#![warn(missing_docs)]
//! The SNARK extension: the pure [`SnarkProtocol`] router and its `SnarkShell`
//! interpreter, which owns the task store and runs the proving/verification crypto.
//!
//! Proving/verification is SNARK's **own** effect (`SnarkEffect::Prove`/`Verify`), not a
//! core capability — it is feature-gated and a node need not enable it. The shell runs the
//! heavy crypto and re-injects the result as a self-event, so `step` stays pure.

use std::sync::Arc;

use bytes::Bytes;
use rings_core::dht::Did;
use serde::Deserialize;
use serde::Serialize;

use super::SNARKBehaviour;
use super::SNARKTaskManager;
use super::TaskId;
use super::NAMESPACE;
use crate::error::Error;
use crate::extension::ext::Ctx;
use crate::extension::ext::Interpret;
use crate::extension::ext::Protocol;
use crate::extension::ext::Reject;
use crate::extension::ext::Scope;
use crate::extension::ext::Transition;
use crate::extension::ext::Wire;
use crate::extension::types::snark::SNARKProofTask;
use crate::extension::types::snark::SNARKTask;
use crate::extension::types::snark::SNARKTaskMessage;
use crate::extension::types::snark::SNARKVerifyTask;

/// The result of a SNARK compute job, re-injected as a self-event for the pure `step`.
#[derive(Serialize, Deserialize)]
pub enum ComputeResult {
    /// A proof was produced; `step` sends the verify task back to `reply_to`.
    Proved {
        /// Task id.
        task_id: TaskId,
        /// DID to reply to.
        reply_to: Did,
        /// The produced verify task.
        verify_task: SNARKVerifyTask,
    },
    /// Verification finished; the boolean is already stored in the task manager.
    Verified {
        /// Task id.
        task_id: TaskId,
    },
}

/// SNARK's typed input: a self re-injected [`ComputeResult`], or a network task message.
pub enum SnarkEvent {
    /// A compute result fed back into the router (provenance = self).
    Result(ComputeResult),
    /// A task message from an authenticated peer.
    Task {
        /// Authenticated sender (the prover to reply to, for a proof request).
        from: Did,
        /// The task message.
        msg: SNARKTaskMessage,
    },
}

/// SNARK's own effect algebra (run by `SnarkShell`).
pub enum SnarkEffect {
    /// Send a (verify) task message to a peer over the overlay.
    SendTask {
        /// Destination.
        to: Did,
        /// The message.
        msg: SNARKTaskMessage,
    },
    /// Prove `task` (heavy crypto), then reply the verify task to `reply_to`.
    Prove {
        /// Task id.
        task_id: TaskId,
        /// DID to reply to.
        reply_to: Did,
        /// Proof task (boxed: far larger than the other variants).
        task: Box<SNARKProofTask>,
    },
    /// Verify `verify_task` against the locally-stored proof task.
    Verify {
        /// Task id (used to look up the stored proof task).
        task_id: TaskId,
        /// Verify task received from the prover.
        verify_task: SNARKVerifyTask,
    },
}

/// SNARK relay protocol: a pure router over the `snark` namespace.
///
/// ```text
///   step (Ctx (), Result(Proved id to vt)) ↦ ((), [SendTask to (Verify id vt)])
///   step (Ctx (), Result(Verified id))     ↦ ((), ε)
///   step (Ctx (), Task(from, Proof t))     ↦ ((), [Prove id from t])
///   step (Ctx (), Task(from, Verify vt))   ↦ ((), [Verify id vt])
/// ```
///
/// The heavy proving/verification is SNARK's own `SnarkEffect`, performed by
/// `SnarkShell`; `step` stays pure.
#[derive(Clone)]
pub struct SnarkProtocol;

impl Protocol for SnarkProtocol {
    type State = ();
    type Event = SnarkEvent;
    type Effect = SnarkEffect;

    fn namespace(&self) -> &str {
        NAMESPACE
    }

    fn init(&self) {}

    fn decode(&self, wire: Wire<'_>) -> Result<SnarkEvent, Reject> {
        if wire.from == wire.me {
            let result = bincode::deserialize::<ComputeResult>(wire.payload)
                .map_err(|e| Reject(format!("bad snark result: {e}")))?;
            Ok(SnarkEvent::Result(result))
        } else {
            let msg = bincode::deserialize::<SNARKTaskMessage>(wire.payload)
                .map_err(|e| Reject(format!("bad snark task: {e}")))?;
            Ok(SnarkEvent::Task {
                from: wire.from,
                msg,
            })
        }
    }

    fn step(&self, _ctx: Ctx<'_, ()>, event: SnarkEvent) -> Transition<(), SnarkEffect> {
        match event {
            SnarkEvent::Result(ComputeResult::Proved {
                task_id,
                reply_to,
                verify_task,
            }) => Transition::with((), vec![SnarkEffect::SendTask {
                to: reply_to,
                msg: SNARKTaskMessage {
                    task_id,
                    task: SNARKTask::SNARKVerify(verify_task),
                },
            }]),
            SnarkEvent::Result(ComputeResult::Verified { .. }) => Transition::pure(()),
            SnarkEvent::Task { from, msg } => match msg.task {
                SNARKTask::SNARKProof(task) => Transition::with((), vec![SnarkEffect::Prove {
                    task_id: msg.task_id,
                    reply_to: from,
                    task,
                }]),
                SNARKTask::SNARKVerify(verify_task) => {
                    Transition::with((), vec![SnarkEffect::Verify {
                        task_id: msg.task_id,
                        verify_task,
                    }])
                }
            },
        }
    }
}

/// SNARK's interpreter: owns the task store and runs the proving/verification crypto.
pub struct SnarkShell {
    manager: Arc<SNARKTaskManager>,
}

impl SnarkShell {
    /// Build over a shared task store.
    pub fn new(manager: Arc<SNARKTaskManager>) -> Self {
        Self { manager }
    }

    /// Serialize a [`ComputeResult`] for re-injection as a self-event for the pure `step`. The
    /// router re-delivers it to this same namespace with `from = this node`.
    fn reinject(&self, result: &ComputeResult) -> crate::error::Result<Vec<Bytes>> {
        let payload = bincode::serialize(result).map_err(|_| Error::EncodeError)?;
        Ok(vec![Bytes::from(payload)])
    }
}

#[cfg_attr(feature = "browser", async_trait::async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait::async_trait)]
impl Interpret for SnarkShell {
    type Effect = SnarkEffect;

    async fn run(&self, scope: &Scope, effect: SnarkEffect) -> crate::error::Result<Vec<Bytes>> {
        match effect {
            SnarkEffect::SendTask { to, msg } => {
                let payload = bincode::serialize(&msg).map_err(|_| Error::EncodeError)?;
                scope.send(to, Bytes::from(payload)).await?;
                Ok(Vec::new())
            }
            SnarkEffect::Prove {
                task_id,
                reply_to,
                task,
            } => {
                let verify_task = SNARKBehaviour::handle_snark_proof_task(task.as_ref())?;
                self.reinject(&ComputeResult::Proved {
                    task_id,
                    reply_to,
                    verify_task,
                })
            }
            SnarkEffect::Verify {
                task_id,
                verify_task,
            } => {
                let Some(task) = self.manager.task.get(&task_id) else {
                    return Err(Error::ExtensionError(format!(
                        "no pending SNARK proof task for {task_id}; cannot verify"
                    )));
                };
                let verified =
                    SNARKBehaviour::handle_snark_verify_task(&verify_task, task.value())?;
                drop(task);
                self.manager.verified.insert(task_id, verified);
                self.reinject(&ComputeResult::Verified { task_id })
            }
        }
    }
}
