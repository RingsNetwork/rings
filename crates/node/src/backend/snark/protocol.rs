#![warn(missing_docs)]
//! The SNARK effect/routing layer: the pure [`SnarkProtocol`] router plus the impure
//! compute job ([`snark_compute`]) that runs the proving/verification crypto in the shell.

use std::sync::Arc;

use bytes::Bytes;
use rings_core::dht::Did;
use serde::Deserialize;
use serde::Serialize;

use super::SNARKBehaviour;
use super::SNARKTaskManager;
use super::TaskId;
use super::NAMESPACE;
use crate::backend::ext::Ctx;
use crate::backend::ext::Effect;
use crate::backend::ext::Event;
use crate::backend::ext::Protocol;
use crate::backend::ext::Transition;
use crate::backend::types::snark::SNARKProofTask;
use crate::backend::types::snark::SNARKTask;
use crate::backend::types::snark::SNARKTaskMessage;
use crate::backend::types::snark::SNARKVerifyTask;
use crate::error::Error;
use crate::error::Result;

/// A SNARK compute request, handed to the shell via [`Effect::Compute`]. Carries the
/// task plus enough context to route the result. Never on the wire (local only).
#[derive(Serialize, Deserialize)]
pub(super) enum ComputeJob {
    /// Prove `task` (prover side), then reply to `reply_to`.
    Prove {
        /// Task id.
        task_id: TaskId,
        /// DID to send the resulting verify task back to.
        reply_to: Did,
        /// Proof task to fold/prove (boxed: far larger than the other variant).
        task: Box<SNARKProofTask>,
    },
    /// Verify `verify_task` against the locally-stored proof task (verifier side).
    Verify {
        /// Task id (used to look up the stored proof task).
        task_id: TaskId,
        /// Verify task received from the prover.
        verify_task: SNARKVerifyTask,
    },
}

/// The result of a [`ComputeJob`], re-injected as a self-event for the pure `step`.
#[derive(Serialize, Deserialize)]
enum ComputeResult {
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

/// The impure SNARK compute job (the shell side). Runs the heavy crypto and, for
/// verification, reads the stored proof task and records the boolean result.
pub(super) fn snark_compute(manager: Arc<SNARKTaskManager>, input: Bytes) -> Result<Bytes> {
    let job: ComputeJob = bincode::deserialize(input.as_ref()).map_err(|_| Error::DecodeError)?;
    let result = match job {
        ComputeJob::Prove {
            task_id,
            reply_to,
            task,
        } => {
            let verify_task = SNARKBehaviour::handle_snark_proof_task(task.as_ref())?;
            ComputeResult::Proved {
                task_id,
                reply_to,
                verify_task,
            }
        }
        ComputeJob::Verify {
            task_id,
            verify_task,
        } => {
            let Some(task) = manager.task.get(&task_id) else {
                return Err(Error::ExtensionError(format!(
                    "no pending SNARK proof task for {task_id}; cannot verify"
                )));
            };
            let verified = SNARKBehaviour::handle_snark_verify_task(&verify_task, task.value())?;
            manager.verified.insert(task_id, verified);
            ComputeResult::Verified { task_id }
        }
    };
    bincode::serialize(&result)
        .map(Bytes::from)
        .map_err(|_| Error::EncodeError)
}

/// SNARK relay protocol: a pure router over the `snark` namespace.
///
/// ```text
///   step (Ctx (), Event{from, p}) =
///     | from = self  ∧ p = Proved(id, to, vt)  ↦ ((), [Send to (Verify id vt)])
///     | from = self  ∧ p = Verified(id)        ↦ ((), ε)
///     | p = SNARKProof(t)                       ↦ ((), [Compute (Prove id from t)])
///     | p = SNARKVerify(vt)                     ↦ ((), [Compute (Verify id vt)])
/// ```
///
/// Provenance splits the two payload codecs purely: `from = self` is a re-injected
/// `ComputeResult`; any other `from` is a network [`SNARKTaskMessage`]. The heavy
/// proving/verification is described as [`Effect::Compute`] and performed by
/// `snark_compute` in the shell, so `step` stays pure.
#[derive(Clone)]
pub struct SnarkProtocol;

impl Protocol for SnarkProtocol {
    type State = ();

    fn namespace(&self) -> &str {
        NAMESPACE
    }

    fn init(&self) {}

    fn step(&self, ctx: Ctx<'_, ()>, event: &Event) -> Transition<()> {
        if event.from == ctx.did {
            let Ok(result) = bincode::deserialize::<ComputeResult>(event.payload.as_ref()) else {
                return Transition::pure(());
            };
            return match result {
                ComputeResult::Proved {
                    task_id,
                    reply_to,
                    verify_task,
                } => {
                    let msg = SNARKTaskMessage {
                        task_id,
                        task: SNARKTask::SNARKVerify(verify_task),
                    };
                    match bincode::serialize(&msg) {
                        Ok(payload) => Transition::with((), vec![Effect::Send {
                            to: reply_to,
                            namespace: NAMESPACE.to_string(),
                            payload: Bytes::from(payload),
                        }]),
                        Err(_) => Transition::pure(()),
                    }
                }
                ComputeResult::Verified { .. } => Transition::pure(()),
            };
        }

        let Ok(msg) = bincode::deserialize::<SNARKTaskMessage>(event.payload.as_ref()) else {
            return Transition::pure(());
        };
        let job = match msg.task {
            SNARKTask::SNARKProof(task) => ComputeJob::Prove {
                task_id: msg.task_id,
                reply_to: event.from,
                task,
            },
            SNARKTask::SNARKVerify(verify_task) => ComputeJob::Verify {
                task_id: msg.task_id,
                verify_task,
            },
        };
        match bincode::serialize(&job) {
            Ok(input) => Transition::with((), vec![Effect::Compute {
                namespace: NAMESPACE.to_string(),
                input: Bytes::from(input),
            }]),
            Err(_) => Transition::pure(()),
        }
    }
}
