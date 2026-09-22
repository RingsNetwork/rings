# Native send ownership

This design resolves the native retire-fence consolidation deferred in #797 / #787.
It changes local ownership, not wire encoding or the runtime-independent notifier.

## State and authorities

For one send, the state is the product of:

- Admission `A`: revocable, cancelled, irrevocable, accepted. The existing atomic
  `SendPermit` / `SendAcceptance` transition machine is the sole authority.
- Cleanup `C`: armed with one physical-close future, closing with that capability
  consumed, finished after the cleanup task terminates.
- Resource location `R`: caller-owned, continuation-owned, released.
- Generation gate `G`: open or retired, shared by all channels on the connection.

`SendLifecycle` owns the one close future. Its `Arc` references confer observation
rights, not additional close capabilities. `OwnedSend` owns destruction ordering.
`QueueSend` owns the channel lease, the single-use permit/proof, and the checked
byte offset. The byte offset is published only after actual enqueue succeeds.

## Transitions

| Event | Condition | Transition / ordering |
| --- | --- | --- |
| Final admission | G is open and permit claim succeeds | A becomes irrevocable under the generation gate; first primitive poll occurs under the same gate |
| First-poll return or panic | Admission guard is held | Release gate before the owner reports failure; never recursively acquire the gate |
| First poll is pending | A is irrevocable | Move the same resource owner into the bounded Tokio continuation |
| Enqueue succeeds | A is irrevocable | Consume proof, publish accepted and checked byte offset; no retirement |
| Failure, timeout, panic, or abandonment | A observed irrevocable | Retire G synchronously, then atomically consume the close capability and spawn cleanup |
| Repeated failure | Close capability already consumed | No second cleanup task for this send |
| Cancellation before claim or after acceptance | A is revocable/cancelled/accepted | No send-driven retirement |
| Caller disappears after handoff | Continuation still pending | Caller reports failure synchronously; continuation retains resources until result, timeout, or executor cancellation |
| Error waiter disappears during cleanup | C is closing | Cleanup task continues independently |
| Cleanup terminates | C is closing | C becomes finished; physical success is recorded only by NativePhysicalCloseWitness |

Failure observation may race with acceptance. Once irrevocable failure has been
observed, later acceptance cannot reopen the generation or reclaim the consumed
close capability. Multiple sends on one generation retain independent send
lifecycles; this is not a connection-wide exactly-once close claim.

## Safety and liveness

1. Only an irrevocable, not-yet-accepted failure requests send-driven retirement.
2. Every failure observer fences before its resource owner releases captured
   resources. The primitive's own internal destructor behavior remains the
   WebRTC implementation's responsibility; our channel lease and proof are held
   outside its async stack, including immediate error and panic paths.
3. The close capability can be consumed at most once per send. Cleanup does not
   borrow either caller or continuation and cannot be cancelled by its waiter.
4. Successful sends advance channel offsets once under the channel lease. Offset
   exhaustion is rejected before permit claim or physical write.
5. Logical retirement, cleanup-task termination, and physical-close success are
   separate facts.

Under a live Tokio executor that schedules ready tasks, pending continuations
retain the existing 25-second completion bound and retirement waits retain the
existing 5-second bound. These are cooperative async deadlines, not guarantees
against a primitive that blocks a thread in poll. Executor shutdown still fences
owned uncertain sends, but cannot guarantee asynchronous physical-close success.
Process aborts and panic=abort provide no Rust destructor guarantee.

## Evidence

The native cancellation regressions retain the existing admission/cancellation,
first-poll panic, detached completion, error-source, timeout, and physical-close
witness coverage. `test_send_lifecycle` adds admission-phase/failure-count cases,
concurrent failures, late acceptance, captured-resource destruction on timeout and
panic, cancellation during close, unpolled executor shutdown, real queue-owner
lease ordering, successful byte commitment, counter exhaustion, and caller/worker
handoff with a shared close capability. These are bounded executable ownership
witnesses, not an exhaustive proof of WebRTC or arbitrary executor schedules.
