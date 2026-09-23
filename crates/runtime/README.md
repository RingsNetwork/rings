# rings-runtime

The runtime contracts shared by every Rings crate that runs on both native Tokio and the
browser's JavaScript event loop.

| Contract | Native | Browser (`browser` feature on a wasm target) |
| --- | --- | --- |
| [`sleep`] | `futures-timer`; never fails | chained `setTimeout` on the window, worker or service-worker scope; fails with [`TimerError`] |
| [`Spawner`] / [`spawn_detached`] | current Tokio runtime; [`RuntimeUnavailable`] outside one | `spawn_local`; always available |
| [`run_detached`] | awaited work owned by the runtime | same |
| [`MaybeSend`] / [`MaybeSendSync`] | `Send` / `Send + Sync` | no bound |

The crate carries no Rings protocol vocabulary; it sits below `rings-transport`, so every
layer above it adapts to the runtime through one set of laws instead of choosing
`futures_timer`, `setTimeout` or a spawn primitive locally. Work whose cancellation the
caller owns is deliberately *not* served here: it keeps its own abort handle.
