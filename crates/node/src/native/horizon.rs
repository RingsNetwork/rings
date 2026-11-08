//! The native daemon's signing horizon and in-process delegation renewal (#1063, #1153).
//!
//! A node whose own delegation reaches its signing horizon `h = e − Δ` can send nothing. Without a
//! delegator key configured, only a restart with a fresh delegation lets it send again
//! (`processor::horizon`); left running, it would keep serving its local API while every peer
//! dropped it, so the daemon ends at `h` with [`Error::DelegationHorizonReached`] (#1063).
//!
//! When `Config::delegator_key` is configured, entering `SigningState::Expiring` at `h − R`
//! (`SIGNING_HORIZON_LEAD_MS`) renews the delegation in process (#1153):
//! 1. Read the ECDSA delegator secret key file and verify `Did::from(delegator_key.address()) ==
//!    processor.did()`.
//! 2. Mint a fresh [`DelegateeKey`] for the current delegation's `ttl_ms()` stamped at `now_ms`.
//! 3. Verify `h' > h` (**Monotone horizon**) and `new_key.delegator_did() == processor.did()`
//!    (**No unsigned gap**).
//! 4. Persist `new_key.dump()` atomically to `delegatee_key` (`0600` sibling temporary file +
//!    `sync_all()` + `fs::rename`).
//! 5. Swap `new_key` into the running [`Processor`] and `SwarmTransport` (retaining the previous
//!    key until its expiry `e` so mail sealed to it before republication settles still opens) and
//!    immediately republish the node's descriptors.
//!
//! Laws (#1153):
//! - **Monotone horizon**: After a successful renewal, `h' > h`. A renewal never shortens the
//!   horizon.
//! - **No unsigned gap**: At every instant in `[renewal, e)`, the node holds some key whose
//!   delegation verifies.
//! - **Fallback**: No delegator key, or a failed mint/persistence, logs a warning, keeps the old
//!   key, and gives the #1063 behaviour (warn at `h − R` and exit at `h`).
//!
//! ```text
//!   loop:  t ← now()
//!          Signing(r)       ─▶ sleep min(r − lead, P)
//!          Expiring(r)      ─▶ if delegator_key: renew(h' > h) ─▶ continue (Signing)
//!                              else: warn once;  sleep min(r, P)
//!          Mute             ─▶ return DelegationHorizonReached
//! ```
//!
//! The horizon is an instant of the epoch clock, while a sleep runs on the monotonic clock, which
//! stops while the machine is suspended. So no sleep is longer than the poll period
//! [`HORIZON_POLL`] (`P`), and every wake re-reads the epoch clock. A host suspended across the
//! horizon, or an epoch clock that stepped forward, ends the wait within `P` of resuming; one that
//! stepped back only sleeps again.

use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::ecc::SecretKey;

use super::config::Config;
use crate::error::Error;
use crate::error::Result;
use crate::processor::DelegationHorizon;
use crate::processor::Processor;
use crate::processor::SigningState;
use crate::processor::SIGNING_HORIZON_LEAD_MS;
use crate::util::ensure_parent_dir;
use crate::util::expand_home;

/// `P`: the longest single sleep of [`signing_horizon`] and [`run_signing_horizon`], so a daemon
/// whose host was suspended across its horizon ends within one period of resuming (module
/// documentation).
pub const HORIZON_POLL: Duration = Duration::from_secs(60);

/// Key-file paths used by [`run_signing_horizon`] to renew a node's delegation in process before
/// its signing horizon (#1153).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegationRenewalPaths {
    /// Destination path where the renewed `DelegateeKey::dump()` is persisted atomically.
    pub delegatee_key_path: PathBuf,
    /// Path to the ECDSA delegator secret key file (`Config::delegator_key`).
    pub delegator_key_path: PathBuf,
}

impl DelegationRenewalPaths {
    /// Build renewal paths from `config` when `config.delegator_key` is present.
    pub fn from_config(config: &Config) -> Result<Option<Self>> {
        config
            .delegator_key
            .as_deref()
            .map(|delegator_key| {
                expand_home(&config.delegatee_key).and_then(|delegatee_key_path| {
                    expand_home(delegator_key).map(|delegator_key_path| Self {
                        delegatee_key_path,
                        delegator_key_path,
                    })
                })
            })
            .transpose()
    }
}

/// Pure parser for the `rings init` ECDSA secret key file format: the first non-empty, non-`#`
/// trimmed line, with an optional `0x` prefix stripped.
fn parse_delegator_secret_key(raw: &str, display_path: &Path) -> Result<SecretKey> {
    raw.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .ok_or_else(|| {
            Error::InvalidConfig(format!(
                "{}: ECDSA key file contains no key entries",
                display_path.display()
            ))
        })
        .map(|line| line.strip_prefix("0x").unwrap_or(line))
        .and_then(|key| {
            SecretKey::from_str(key).map_err(|_| {
                Error::InvalidConfig(format!(
                    "{}: ECDSA key file contains an invalid key",
                    display_path.display()
                ))
            })
        })
}

/// Read an ECDSA secret key file in the `rings init` format: the first non-empty, non-`#`
/// trimmed line, with an optional `0x` prefix stripped.
pub fn read_delegator_secret_key(path: impl AsRef<Path>) -> Result<SecretKey> {
    expand_home(path.as_ref()).and_then(|path| {
        fs::read_to_string(&path)
            .map_err(|error| Error::OpenFileError(format!("{}: {error}", path.display())))
            .and_then(|raw| parse_delegator_secret_key(&raw, &path))
    })
}

/// Atomically write `delegatee_key.dump()` to `path` via a mode-`0600` sibling temporary file,
/// `sync_all()`, `fs::rename`, and parent directory `sync_all()` on Unix.
pub fn write_delegatee_key_atomically(
    path: impl AsRef<Path>,
    delegatee_key: &DelegateeKey,
) -> Result<PathBuf> {
    let target = expand_home(path.as_ref())?;
    ensure_parent_dir(&target)?;
    let parent = target.parent().ok_or_else(|| {
        Error::CreateFileError(format!(
            "{}: path has no parent directory",
            target.display()
        ))
    })?;
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("delegatee_key");
    let temp_path = parent.join(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let dumped = delegatee_key.dump().map_err(Error::CoreError)?;
    let write_result = (|| -> std::io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temp_path)?;
        file.write_all(dumped.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp_path, &target)?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(Error::CreateFileError(format!(
            "{}: {error}",
            target.display()
        )));
    }
    Ok(target)
}

/// Attempt one in-process delegation renewal for `processor` at `now_ms`.
async fn try_renew_delegation(
    processor: &Processor,
    paths: &DelegationRenewalPaths,
    now_ms: u128,
) -> Result<DelegationHorizon> {
    let expected_did = processor.did();
    let next_horizon = processor.renew_delegatee_key_with(
        now_ms,
        |current| {
            read_delegator_secret_key(&paths.delegator_key_path)
                .and_then(|delegator_key| {
                    (Did::from(delegator_key.address()) == expected_did)
                        .then_some(delegator_key)
                        .ok_or_else(|| {
                            Error::VerifyError(
                                "delegator key DID does not match node DID".to_string(),
                            )
                        })
                })
                .and_then(|delegator_key| {
                    DelegateeKey::new_with_seckey_at(
                        &delegator_key,
                        current.delegation().ttl_ms(),
                        now_ms,
                    )
                    .map_err(Error::CoreError)
                })
        },
        |next| write_delegatee_key_atomically(&paths.delegatee_key_path, next).map(|_| ()),
    )?;
    processor.republish_registrations().await;
    Ok(next_horizon)
}

/// Pure decision ADT produced by one step of the [`HorizonSupervisorState`] Mealy machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HorizonAction {
    /// Horizon reached (`t >= h`): log and terminate with [`DelegationHorizon::reached`].
    ExitMute,
    /// Attempt in-process renewal at `now_ms` inside `[renew_at_ms(c), h)`.
    AttemptRenewal {
        /// Remaining time until `h`.
        remaining_ms: u128,
    },
    /// Sleep for `wait_ms` (bounded by [`HORIZON_POLL`]), optionally emitting the one-time
    /// expiring warning for `horizon_ms`.
    Sleep {
        /// Unclamped wait duration in milliseconds.
        wait_ms: u128,
        /// Remaining time until `h` if a one-time expiring warning should be logged first.
        warn_remaining_ms: Option<u128>,
    },
}

/// Pure Mealy state machine tracking per-horizon warning and renewal-attempt idempotency:
/// `HorizonSupervisorState × (DelegationHorizon, u128, u128, bool) -> (HorizonSupervisorState, HorizonAction)`.
///
/// Laws:
/// - **At-most-once renewal per horizon**: For a given `h`, [`HorizonAction::AttemptRenewal`] is
///   emitted at most once (`attempted_horizon_ms != Some(h)`).
/// - **At-most-once warning per horizon**: For a given `h`, `warn_remaining_ms` is `Some(_)` at
///   most once (`warned_horizon_ms != Some(h)`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct HorizonSupervisorState {
    warned_horizon_ms: Option<u128>,
    attempted_horizon_ms: Option<u128>,
}

impl HorizonSupervisorState {
    /// Pure state transition computing the next supervisor state and [`HorizonAction`] at `now_ms`.
    fn step(
        self,
        horizon: DelegationHorizon,
        created_at_ms: u128,
        now_ms: u128,
        can_renew: bool,
    ) -> (Self, HorizonAction) {
        let h = horizon.horizon_ms();
        match horizon.state_at(now_ms) {
            SigningState::Mute => (self, HorizonAction::ExitMute),
            SigningState::Signing { remaining_ms } => (self, HorizonAction::Sleep {
                wait_ms: remaining_ms - SIGNING_HORIZON_LEAD_MS,
                warn_remaining_ms: None,
            }),
            SigningState::Expiring { remaining_ms } => {
                let renew_at_ms = horizon.renew_at_ms(created_at_ms);
                if can_renew && now_ms < renew_at_ms {
                    (self, HorizonAction::Sleep {
                        wait_ms: renew_at_ms - now_ms,
                        warn_remaining_ms: None,
                    })
                } else if can_renew && self.attempted_horizon_ms != Some(h) {
                    (
                        Self {
                            attempted_horizon_ms: Some(h),
                            ..self
                        },
                        HorizonAction::AttemptRenewal { remaining_ms },
                    )
                } else {
                    let (next, wait_ms, warn_remaining_ms) = self.fallback_sleep(h, remaining_ms);
                    (next, HorizonAction::Sleep {
                        wait_ms,
                        warn_remaining_ms,
                    })
                }
            }
        }
    }

    /// Pure transition into the fallback expiring sleep for `horizon_ms`, emitting the one-time
    /// warning payload iff `self.warned_horizon_ms != Some(horizon_ms)`.
    fn fallback_sleep(self, horizon_ms: u128, remaining_ms: u128) -> (Self, u128, Option<u128>) {
        let warn_remaining_ms =
            (self.warned_horizon_ms != Some(horizon_ms)).then_some(remaining_ms);
        (
            Self {
                warned_horizon_ms: Some(horizon_ms),
                ..self
            },
            remaining_ms,
            warn_remaining_ms,
        )
    }
}

/// Pure conversion of an unclamped horizon wait in milliseconds into a poll-bounded [`Duration`].
fn bounded_horizon_sleep(wait_ms: u128) -> Duration {
    Duration::from_millis(u64::try_from(wait_ms).unwrap_or(u64::MAX)).min(HORIZON_POLL)
}

fn log_mute_and_reach(horizon: DelegationHorizon) -> Error {
    tracing::error!(
        expires_at_ms = %horizon.expires_at_ms(),
        horizon_ms = %horizon.horizon_ms(),
        "the node's delegation reached its signing horizon: no frame can leave, so \
         the node exits for its supervisor to restart it with a fresh delegatee key"
    );
    horizon.reached()
}

fn log_expiring_warning(horizon_ms: u128, remaining_ms: u128) {
    tracing::warn!(
        remaining_ms = %remaining_ms,
        horizon_ms = %horizon_ms,
        "the node's delegation nears its signing horizon: the node exits then; \
         rotate its delegatee key and restart it before"
    );
}

/// Supervise `processor`'s signing horizon on the epoch clock `now_ms`, renewing the delegation
/// in process at `horizon.renew_at_ms(created_at_ms)` when `renewal` is configured, and falling
/// back to the #1063 exit at `h` otherwise (#1153).
pub async fn run_signing_horizon(
    processor: Arc<Processor>,
    renewal: Option<DelegationRenewalPaths>,
    now_ms: impl Fn() -> u128,
) -> Error {
    let mut supervisor = HorizonSupervisorState::default();
    loop {
        let now = now_ms();
        let delegation = processor.delegatee_key().delegation();
        let horizon = DelegationHorizon::of(&delegation);
        let h = horizon.horizon_ms();
        let (next_state, action) =
            supervisor.step(horizon, delegation.created_at_ms(), now, renewal.is_some());
        supervisor = next_state;

        let (wait_ms, warn_remaining_ms) = match (action, renewal.as_ref()) {
            (HorizonAction::ExitMute, _) => return log_mute_and_reach(horizon),
            (HorizonAction::AttemptRenewal { remaining_ms }, Some(paths)) => {
                match try_renew_delegation(&processor, paths, now).await {
                    Ok(next_horizon) => {
                        tracing::info!(
                            previous_horizon_ms = %h,
                            next_horizon_ms = %next_horizon.horizon_ms(),
                            expires_at_ms = %next_horizon.expires_at_ms(),
                            "renewed the node's delegation in process before its signing horizon"
                        );
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            remaining_ms = %remaining_ms,
                            horizon_ms = %h,
                            "failed to renew the node's delegation in process; falling \
                             back to exiting at the signing horizon"
                        );
                        let (fallback_state, wait_ms, warn_remaining_ms) =
                            supervisor.fallback_sleep(h, remaining_ms);
                        supervisor = fallback_state;
                        (wait_ms, warn_remaining_ms)
                    }
                }
            }
            (HorizonAction::AttemptRenewal { remaining_ms }, None) => {
                let (fallback_state, wait_ms, warn_remaining_ms) =
                    supervisor.fallback_sleep(h, remaining_ms);
                supervisor = fallback_state;
                (wait_ms, warn_remaining_ms)
            }
            (
                HorizonAction::Sleep {
                    wait_ms,
                    warn_remaining_ms,
                },
                _,
            ) => (wait_ms, warn_remaining_ms),
        };

        if let Some(remaining_ms) = warn_remaining_ms {
            log_expiring_warning(h, remaining_ms);
        }
        tokio::time::sleep(bounded_horizon_sleep(wait_ms)).await;
    }
}

/// Wait until `horizon` on the epoch clock `now_ms` (module diagram), warning once inside the
/// lead, and return the error that ends the daemon there. It never returns before the horizon.
pub async fn signing_horizon(horizon: DelegationHorizon, now_ms: impl Fn() -> u128) -> Error {
    let mut warned = false;
    let h = horizon.horizon_ms();
    loop {
        match horizon.state_at(now_ms()) {
            SigningState::Mute => return log_mute_and_reach(horizon),
            SigningState::Signing { remaining_ms } => {
                tokio::time::sleep(bounded_horizon_sleep(
                    remaining_ms - SIGNING_HORIZON_LEAD_MS,
                ))
                .await;
            }
            SigningState::Expiring { remaining_ms } => {
                if !std::mem::replace(&mut warned, true) {
                    log_expiring_warning(h, remaining_ms);
                }
                tokio::time::sleep(bounded_horizon_sleep(remaining_ms)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests;
