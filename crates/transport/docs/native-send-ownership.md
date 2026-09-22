# Native send ownership

This design resolves native retire-fence consolidation deferred in #797 / #787.
It changes local ownership, not wire encoding or the runtime-independent notifier.

## Functional core and effect boundaries

`send_model` contains total, deterministic functions over immutable values:

- `failure_effect(AdmissionPhase) -> FailureEffect` selects whether to fence.
- `observation_step(State, Observation) -> (State, Effect)` decides whether an
  owner must report failure. A finished observer is absorbing.
- `close_step(State, Event) -> (State, Effect)` defines the close actor protocol.
- `end_offset(current, bytes) -> Option<u64>` checks byte accounting without IO.

The existing pure `AdmissionPhase::transition` defines permit transitions. Atomic
permit operations interpret it; they are not an alternative state machine.
Preconditions, postconditions, and preservation laws are recorded beside reducers.
The executable exploration calls these production reducers, rather than copying
close logic into a separate specification.

Effects stay in named adapters: `send_lifecycle` handles synchronous fencing,
mailbox delivery and poll/destruction observations; `close_actor` owns physical
close and publication; `send_operation` owns queue IO and offset commitment;
`send_runtime` owns spawning, deadlines and the generation admission lock. These
adapters necessarily perform mutation and IO. The new ownership modules and
refactored backend send adapter use exhaustive matches and Result composition,
without explicit early `return`. Error propagation with `?` remains appropriate
where the resource owner guarantees fence-before-release; it is not moved into an
inner async scope that would release the channel lease too soon. Existing unrelated
backend methods are outside this change.

## Actor and ownership

Each send creates one one-shot close actor before physical send admission. The
actor exclusively owns its mutable state, mailbox receiver, physical-close future
and watch sender. `SendLifecycle` shares only its mailbox address, read-only watch
receiver and synchronous fence adapter. There is no mutex-shared close future or
shared mutable actor state.

The capacity-one mailbox coalesces equivalent failure commands. A sealed
`FencedCommand` can only be constructed after synchronous fencing. Once the actor
consumes that command it drops its receiver and awaits its sole close future.
If all senders disappear without a command it terminates as `Unused` without
polling physical close. A reporter constructed before spawning publishes
`Interrupted` on destruction, even when shutdown prevents the actor's first poll.
Terminal outcomes are absorbing.

This is a one-shot Actor protocol with a synchronous admission boundary. The
connection-wide generation mutex intentionally remains: a message enqueue alone
cannot guarantee that Drop fences before releasing resources. First primitive
poll and generation retirement must have a synchronous linearization point.
The cost is one actor task and bounded mailbox/watch storage per live send,
including sends that eventually succeed. Actor scheduling releases unused close
captures after the last sender disappears; there is no sender/actor ownership cycle.

`OwnedSend` controls destruction order. `QueueSend` holds the channel lease and an
explicit `Ready(permit) | Sending(proof) | Finished` capability state outside the
primitive's async stack. A pending first poll moves that same owner to a bounded
continuation. The primitive's internal destructor behavior remains WebRTC's
responsibility; the outer channel lease remains owned until failure is fenced.

## Transition contract

| Event | Condition | Result / order |
| --- | --- | --- |
| Final admission | Generation open; permit claim succeeds | Mark irrevocable and first-poll primitive under the same generation lease |
| First poll returns or panics | Lease held | Release generation lease before failure reporting; no recursive lock |
| First poll pending | Irrevocable | Move the same resource owner to continuation |
| Enqueue succeeds | Irrevocable | Consume acceptance proof, publish checked byte offset |
| Failure/panic/timeout/abandonment | Snapshot is irrevocable | Synchronously fence, notify actor, then release resources |
| Late acceptance | Failure snapshot already taken | Cannot revoke that failure decision or reopen the generation |
| Fenced command | Actor Idle | Commit Closing, initiate close once |
| Duplicate command | Already queued, closing or terminal | Coalesce; never initiate another close for this send |
| All senders gone without command | Actor Idle | Publish Unused |
| Close returns Ok / Err | Actor Closing | Publish Succeeded / Failed |
| Executor drops actor | Not terminal | Publish Interrupted; infer no physical success |
| Waiter disappears | Actor independent | Cleanup retains its own ownership |

Concurrent sends have separate actors: at-most-once is **per send**, not per
connection generation. Explicit close remains generation-pinned. Revocable,
cancelled and accepted admission snapshots do not request send-driven retirement.
The backend's `NativePhysicalCloseWitness` remains the authority for actual
physical success; a cleanup timeout can precede eventual backend closure.

## Verification and scope

`test_send_model` performs breadth-first exploration to a fixed point, with no
arbitrary trace-depth cutoff (460 reachable states), for one send with caller and worker observers. It
composes admission, first-poll gate, resource location, both observers' individual
snapshot/fence/enqueue/release steps, bounded mailbox, actor state, executor state
and independent physical completion. Cancellation can race permit claim; failure
observation can race later acceptance; shutdown can interrupt pending admission,
first polling or close; physical completion can follow a cleanup timeout.

The assertions require fence-before-command and fence-before-failed-resource-release,
at-most-one close initiation, and real physical success before an actor success
result. Semantic coverage assertions ensure the important races are reachable.
Mutation checks deliberately omit fencing, permit duplicate close, or report
shutdown as success, and require shortest counterexample traces for each defect.

`test_close_actor` checks the real shell against reducer traces: bounded duplicate
requests, Closing snapshots, successful and failed IO, unused actor destruction,
and runtime shutdown before/after starting close. Existing lifecycle/cancellation
tests exercise real permit atomics, polling, channel leases, offset exhaustion,
first-poll panic, late acceptance, concurrent failures and handoff cancellation.

This is exhaustive safety checking of a finite abstraction plus implementation
conformance/regression evidence. It is not a proof of arbitrary WebRTC internals,
Tokio's scheduler, Rust's memory model or unbounded multi-send systems. Atomic
operations and locks are abstracted as linearizable steps; IO bytes are abstracted
away. The existing cross-channel gate regressions separately exercise concurrent
admission against retirement. Unwinding is assumed; process abort and panic=abort
provide no destructor guarantee.

Liveness is conditional, not established by the safety exploration alone: under
a live, fairly scheduling executor and cooperative primitive polling, a delivered
command progresses through Closing to a terminal outcome. Existing completion and
retirement bounds remain 25 and 5 seconds respectively. Deadlines cannot preempt
a blocking poll; shutdown preserves synchronous fencing but cannot guarantee
asynchronous physical-close completion.
