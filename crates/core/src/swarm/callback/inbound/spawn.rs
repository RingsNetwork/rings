//! Runtime-specific inbound actor spawning boundary.

use super::InboundActor;
use crate::swarm::detached::spawn_detached;

/// Run the actor detached. Post: `true` iff a runtime took it.
pub(super) fn spawn_actor(actor: InboundActor) -> bool {
    spawn_detached(Box::pin(actor.run())).is_ok()
}
