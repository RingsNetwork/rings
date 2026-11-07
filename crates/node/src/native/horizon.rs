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
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::Duration;

    use rand::rngs::OsRng;
    use rings_core::delegation::DelegateeKey;
    use rings_core::ecc::SecretKey;
    use rings_core::utils::get_epoch_ms;
    use tokio::time::Instant;

    use super::read_delegator_secret_key;
    use super::run_signing_horizon;
    use super::signing_horizon;
    use super::write_delegatee_key_atomically;
    use super::DelegationRenewalPaths;
    use super::HORIZON_POLL;
    use crate::error::Error;
    use crate::mail::descriptor::MailRecipient;
    use crate::mail::envelope::seal;
    use crate::mail::open::open_mail_with_keys;
    use crate::mail::open::Opening;
    use crate::processor::DelegationHorizon;
    use crate::processor::Processor;
    use crate::processor::ProcessorConfig;
    use crate::processor::SIGNING_HORIZON_LEAD_MS;
    use crate::test_support::TestTask;
    use crate::tests::test_processor_builder;
    use crate::tests::TEST_ICE_SERVERS;
    use crate::tests::TEST_NETWORK_ID;

    /// The epoch reading at the paused runtime's start.
    const EPOCH_MS: u128 = 1_800_000_000_000;

    /// An epoch clock that reads the paused runtime's time from `EPOCH_MS`, plus `jump_ms` once
    /// `after` of it has passed: a host suspended at `after` for `jump_ms`, whose monotonic
    /// clock (the paused runtime's) did not advance meanwhile.
    fn injected_clock(after: Duration, jump_ms: u128) -> impl Fn() -> u128 {
        let start = Instant::now();
        move || {
            let elapsed = start.elapsed();
            let jumped = if elapsed >= after { jump_ms } else { 0 };
            EPOCH_MS + elapsed.as_millis() + jumped
        }
    }

    /// Build a test processor whose initial delegation is stamped at `ts_ms` with `ttl_ms`.
    fn processor_at(delegator: &SecretKey, ttl_ms: u64, ts_ms: u128) -> Arc<Processor> {
        let delegatee_key = match DelegateeKey::new_with_seckey_at(delegator, ttl_ms, ts_ms) {
            Ok(key) => key,
            Err(error) => panic!("test delegatee key mint must succeed: {error}"),
        };
        let config = ProcessorConfig::new(
            TEST_NETWORK_ID,
            TEST_ICE_SERVERS.to_string(),
            delegatee_key,
            3,
        );
        let processor = match test_processor_builder(&config).build() {
            Ok(processor) => processor,
            Err(error) => panic!("test processor build must succeed: {error}"),
        };
        Arc::new(processor)
    }

    /// `signing → mute` (#1063): a delegation two leads past the start is waited out on the
    /// injected clock, in sleeps of at most `P`, and the daemon's error names the horizon at
    /// exactly that instant. The health predicate (`overlay_ready` reads `signs_at`) holds one
    /// millisecond before and fails at the end.
    #[tokio::test(start_paused = true)]
    async fn test_the_daemon_ends_at_its_horizon() {
        let now_ms = injected_clock(Duration::MAX, 0);
        let horizon = DelegationHorizon::expiring_at(EPOCH_MS + 2 * SIGNING_HORIZON_LEAD_MS);
        let h = horizon.horizon_ms();

        let error = signing_horizon(horizon, &now_ms).await;

        assert!(matches!(
            error,
            Error::DelegationHorizonReached { horizon_ms, .. } if horizon_ms == h
        ));
        assert_eq!(now_ms(), h);
        assert!(horizon.signs_at(h - 1) && !horizon.signs_at(h));
    }

    /// A host suspended across the horizon (#1138 review): ten seconds in, the epoch clock jumps
    /// three leads, past the horizon, while the monotonic clock does not move. The daemon ends
    /// at its next wake, within one poll period of the start, not after the day-long sleep the
    /// monotonic clock alone would have waited.
    #[tokio::test(start_paused = true)]
    async fn test_a_daemon_suspended_across_its_horizon_ends_within_one_poll() {
        let suspended_at = Duration::from_secs(10);
        let now_ms = injected_clock(suspended_at, 3 * SIGNING_HORIZON_LEAD_MS);
        let horizon = DelegationHorizon::expiring_at(EPOCH_MS + 2 * SIGNING_HORIZON_LEAD_MS);
        let start = Instant::now();

        let error = signing_horizon(horizon, &now_ms).await;

        assert!(matches!(error, Error::DelegationHorizonReached { .. }));
        assert!(start.elapsed() > suspended_at && start.elapsed() <= HORIZON_POLL);
    }

    /// A daemon already past its horizon ends at once, without waiting.
    #[tokio::test(start_paused = true)]
    async fn test_a_daemon_past_its_horizon_ends_at_once() {
        let now_ms = injected_clock(Duration::MAX, 0);
        let horizon = DelegationHorizon::expiring_at(EPOCH_MS);

        let error = signing_horizon(horizon, &now_ms).await;

        assert!(matches!(error, Error::DelegationHorizonReached { .. }));
        assert_eq!(now_ms(), EPOCH_MS);
    }

    /// In-process delegation renewal (#1153): a node with `delegator_key` configured renews at
    /// `h − R` across multiple horizons (`h0 < h1 < h2`), atomically persists each new
    /// `DelegateeKey` (`0600`, no leftover temp files), updates both `Processor` and `Swarm`,
    /// immediately republishes its online descriptor with the new `delegatee_public_key`, opens
    /// mail sealed to the prior key before `e0`, and falls back to exiting at `h2` if the
    /// delegator key is removed before the next renewal.
    #[tokio::test(start_paused = true)]
    async fn test_run_signing_horizon_renews_in_process_across_multiple_horizons_and_republishes() {
        let base_ms = get_epoch_ms().max(EPOCH_MS);
        let start = Instant::now();
        let now_ms = move || base_ms + start.elapsed().as_millis();

        let delegator = SecretKey::random();
        let ttl_ms = u64::try_from(3 * SIGNING_HORIZON_LEAD_MS).unwrap_or(u64::MAX);
        let processor = processor_at(&delegator, ttl_ms, base_ms);
        let initial_key = processor.delegatee_key();
        let initial_pubkey = initial_key.delegatee_public_key();
        let h0 = processor.signing_horizon().horizon_ms();
        let e0 = processor.signing_horizon().expires_at_ms();

        let root = std::env::temp_dir().join(format!("rings-horizon-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).expect("create temp horizon dir");
        let delegator_key_path = root.join("ecdsa_key");
        let delegatee_key_path = root.join("delegatee_key");
        fs::write(
            &delegator_key_path,
            format!("# rings init secret key\n0x{}\n", delegator.to_string()),
        )
        .expect("write delegator key");
        assert_eq!(
            read_delegator_secret_key(&delegator_key_path).expect("read delegator key"),
            delegator
        );
        write_delegatee_key_atomically(&delegatee_key_path, &initial_key)
            .expect("write initial delegatee key");

        // Publish initial online-node descriptor before renewal.
        processor.republish_registrations().await;
        let initial_online = processor
            .lookup_online_nodes(false)
            .await
            .expect("list initial online nodes");
        assert_eq!(
            initial_online
                .iter()
                .find(|node| node.did == processor.did())
                .map(|node| node.delegatee_public_key),
            Some(initial_pubkey)
        );

        // Seal mail to the initial key before renewal so we can verify decryption after rotation.
        let recipient_before_renewal = MailRecipient {
            did: processor.did(),
            encryption_key: initial_pubkey,
        };
        let mail_bytes = b"Message-ID: <across-renewal@example.org>\r\n\r\nStill readable.\r\n";
        let sealed_to_initial = seal(mail_bytes, &recipient_before_renewal, &mut OsRng)
            .expect("seal mail to initial key");

        let paths = DelegationRenewalPaths {
            delegatee_key_path: delegatee_key_path.clone(),
            delegator_key_path: delegator_key_path.clone(),
        };
        let task_processor = Arc::clone(&processor);
        let mut horizon_task = TestTask::spawn(async move {
            run_signing_horizon(task_processor, Some(paths), now_ms).await
        });

        // Advance to `h0 - R + 1 ms`: first renewal must have completed.
        let to_first_renewal =
            u64::try_from((h0 - SIGNING_HORIZON_LEAD_MS) - base_ms + 1).unwrap_or(u64::MAX);
        tokio::time::sleep(Duration::from_millis(to_first_renewal)).await;
        tokio::task::yield_now().await;

        let h1 = processor.signing_horizon().horizon_ms();
        let k1 = processor.delegatee_key();
        assert!(
            h1 > h0,
            "Monotone horizon law: h1 ({h1}) must exceed h0 ({h0})"
        );
        assert_ne!(k1.delegatee_public_key(), initial_pubkey);
        assert_eq!(k1.delegator_did(), processor.did());
        assert_eq!(
            processor.swarm.delegatee_key().delegatee_public_key(),
            k1.delegatee_public_key()
        );

        // On-disk delegatee_key was atomically updated with mode 0600 and no leftover temp files.
        let disk_raw_1 = fs::read_to_string(&delegatee_key_path).expect("read renewed key 1");
        let disk_key_1 = DelegateeKey::from_str(&disk_raw_1).expect("parse renewed key 1");
        assert_eq!(disk_key_1.delegatee_public_key(), k1.delegatee_public_key());
        #[cfg(unix)]
        {
            let mode = fs::metadata(&delegatee_key_path)
                .expect("stat delegatee_key")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        let leftover_temps = fs::read_dir(&root)
            .expect("read temp dir")
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(leftover_temps, 0);

        // Online-node descriptor was republished immediately with k1's delegatee_public_key.
        let online_after_1 = processor
            .lookup_online_nodes(false)
            .await
            .expect("list online nodes after first renewal");
        assert_eq!(
            online_after_1
                .iter()
                .find(|node| node.did == processor.did())
                .map(|node| node.delegatee_public_key),
            Some(k1.delegatee_public_key())
        );

        // Advance past the original horizon `h0`: the node is still signing, holds a valid
        // delegation, and opens mail sealed to `k0` before `e0`.
        let past_h0 = u64::try_from(h0 + 1 - now_ms()).unwrap_or(0);
        tokio::time::sleep(Duration::from_millis(past_h0)).await;
        assert!(
            !horizon_task.is_finished(),
            "node must keep running past h0"
        );
        assert!(processor.signing_horizon().signs_at(now_ms()));
        let sig = processor
            .swarm
            .delegatee_key()
            .sign(b"frame-past-h0")
            .expect("sign past h0");
        assert!(processor
            .delegatee_key()
            .delegation()
            .verify_at(b"frame-past-h0", sig, now_ms())
            .is_ok());
        assert!(now_ms() < e0);
        let opened = open_mail_with_keys(
            &sealed_to_initial.elements,
            processor.did(),
            &processor.decryption_keys_at(now_ms()),
            |_| true,
        );
        assert!(matches!(
            opened.as_slice(),
            [(Opening::Opened(ref mail), Some(by_key))]
                if mail.message.as_slice() == mail_bytes && *by_key == initial_pubkey
        ));

        // Advance to `h1 - R + 1 ms`: second renewal must complete (`h2 > h1 > h0`).
        let to_second_renewal =
            u64::try_from((h1 - SIGNING_HORIZON_LEAD_MS) - now_ms() + 1).unwrap_or(0);
        tokio::time::sleep(Duration::from_millis(to_second_renewal)).await;
        tokio::task::yield_now().await;

        let h2 = processor.signing_horizon().horizon_ms();
        let k2 = processor.delegatee_key();
        assert!(
            h2 > h1,
            "Monotone horizon law: h2 ({h2}) must exceed h1 ({h1})"
        );
        assert_ne!(k2.delegatee_public_key(), k1.delegatee_public_key());
        let snapshot = processor
            .operator_snapshot()
            .await
            .expect("operator snapshot");
        assert_eq!(snapshot.session_key.rotation_succeeded_total, 2);
        assert_eq!(snapshot.session_key.rotation_failed_total, 0);

        let online_after_2 = processor
            .lookup_online_nodes(false)
            .await
            .expect("list online nodes after second renewal");
        assert_eq!(
            online_after_2
                .iter()
                .find(|node| node.did == processor.did())
                .map(|node| node.delegatee_public_key),
            Some(k2.delegatee_public_key())
        );

        // Remove the delegator key file before `h2 - R`: the third renewal fails and falls back
        // to exiting at `h2` with `Error::DelegationHorizonReached`.
        fs::remove_file(&delegator_key_path).expect("remove delegator key for fallback test");
        let exit_error = (&mut horizon_task)
            .await
            .expect("horizon task joins cleanly");
        assert!(matches!(
            exit_error,
            Error::DelegationHorizonReached { horizon_ms, .. } if horizon_ms == h2
        ));
        assert_eq!(now_ms(), h2);
        let snapshot_end = processor
            .operator_snapshot()
            .await
            .expect("operator snapshot at exit");
        assert_eq!(snapshot_end.session_key.rotation_succeeded_total, 2);
        assert_eq!(snapshot_end.session_key.rotation_failed_total, 1);

        let _ = fs::remove_dir_all(&root);
    }

    /// Fallback law (#1153): without `delegator_key` configured, or when `delegator_key` has a
    /// DID different from the node's DID, `run_signing_horizon` keeps the old key and exits at
    /// `h` with `Error::DelegationHorizonReached` (#1063 behaviour).
    #[tokio::test(start_paused = true)]
    async fn test_run_signing_horizon_falls_back_when_delegator_key_is_absent_or_mismatched() {
        let ttl_ms = u64::try_from(2 * SIGNING_HORIZON_LEAD_MS).unwrap_or(u64::MAX);

        // 1. No `delegator_key` configured (`renewal = None`).
        {
            let now_ms = injected_clock(Duration::MAX, 0);
            let delegator = SecretKey::random();
            let processor = processor_at(&delegator, ttl_ms, EPOCH_MS);
            let h0 = processor.signing_horizon().horizon_ms();

            let error = run_signing_horizon(Arc::clone(&processor), None, &now_ms).await;
            assert!(matches!(
                error,
                Error::DelegationHorizonReached { horizon_ms, .. } if horizon_ms == h0
            ));
            assert_eq!(now_ms(), h0);
        }

        // 2. Mismatched `delegator_key` (different account DID).
        {
            let now_ms = injected_clock(Duration::MAX, 0);
            let delegator = SecretKey::random();
            let wrong_delegator = SecretKey::random();
            let processor = processor_at(&delegator, ttl_ms, EPOCH_MS);
            let initial_pubkey = processor.delegatee_key().delegatee_public_key();
            let h0 = processor.signing_horizon().horizon_ms();

            let root = std::env::temp_dir()
                .join(format!("rings-horizon-fallback-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).expect("create temp fallback dir");
            let delegator_key_path = root.join("wrong_ecdsa_key");
            let delegatee_key_path = root.join("delegatee_key");
            fs::write(&delegator_key_path, wrong_delegator.to_string())
                .expect("write wrong delegator key");
            write_delegatee_key_atomically(&delegatee_key_path, &processor.delegatee_key())
                .expect("write initial delegatee key");

            let paths = DelegationRenewalPaths {
                delegatee_key_path: delegatee_key_path.clone(),
                delegator_key_path,
            };
            let error = run_signing_horizon(Arc::clone(&processor), Some(paths), &now_ms).await;
            assert!(matches!(
                error,
                Error::DelegationHorizonReached { horizon_ms, .. } if horizon_ms == h0
            ));
            assert_eq!(now_ms(), h0);
            assert_eq!(
                processor.delegatee_key().delegatee_public_key(),
                initial_pubkey
            );
            let snapshot = processor
                .operator_snapshot()
                .await
                .expect("operator snapshot");
            assert_eq!(snapshot.session_key.rotation_succeeded_total, 0);
            assert_eq!(snapshot.session_key.rotation_failed_total, 1);

            let _ = fs::remove_dir_all(&root);
        }
    }

    /// Short-TTL delegation renewal (#1153): a delegation whose lifetime is shorter than
    /// `SIGNING_HORIZON_LEAD_MS` starts in `SigningState::Expiring` at `t = c0`, waits until its
    /// half-life `c0 + (h0 − c0) / 2` before renewing to `h1 > h0` with zero failed attempts, and
    /// does not churn keys before `c1 + (h1 − c1) / 2`.
    #[tokio::test(start_paused = true)]
    async fn test_run_signing_horizon_renews_short_ttl_delegation_at_half_life_without_churn() {
        let base_ms = get_epoch_ms().max(EPOCH_MS);
        let start = Instant::now();
        let now_ms = move || base_ms + start.elapsed().as_millis();

        let delegator = SecretKey::random();
        let short_ttl_ms: u64 = 600_000;
        let processor = processor_at(&delegator, short_ttl_ms, base_ms);
        let initial_key = processor.delegatee_key();
        let initial_pubkey = initial_key.delegatee_public_key();
        let h0 = processor.signing_horizon().horizon_ms();
        let first_renew_at_ms = base_ms + (h0 - base_ms) / 2;

        let root =
            std::env::temp_dir().join(format!("rings-horizon-short-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).expect("create temp short-ttl dir");
        let delegator_key_path = root.join("ecdsa_key");
        let delegatee_key_path = root.join("delegatee_key");
        fs::write(&delegator_key_path, delegator.to_string()).expect("write delegator key");
        write_delegatee_key_atomically(&delegatee_key_path, &initial_key)
            .expect("write initial delegatee key");

        let paths = DelegationRenewalPaths {
            delegatee_key_path,
            delegator_key_path: delegator_key_path.clone(),
        };
        let task_processor = Arc::clone(&processor);
        let mut horizon_task = TestTask::spawn(async move {
            run_signing_horizon(task_processor, Some(paths), now_ms).await
        });

        // One millisecond before the half-life `first_renew_at_ms`, the node has neither renewed
        // nor recorded a failed attempt at `t = c0`.
        let before_first = u64::try_from(first_renew_at_ms - 1 - base_ms).unwrap_or(0);
        tokio::time::sleep(Duration::from_millis(before_first)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            processor.delegatee_key().delegatee_public_key(),
            initial_pubkey
        );
        let snap_before = processor
            .operator_snapshot()
            .await
            .expect("snapshot before first renewal");
        assert_eq!(snap_before.session_key.rotation_succeeded_total, 0);
        assert_eq!(snap_before.session_key.rotation_failed_total, 0);

        // Advance 2 ms across `first_renew_at_ms`: exactly one renewal occurs (`h1 > h0`).
        tokio::time::sleep(Duration::from_millis(2)).await;
        tokio::task::yield_now().await;
        let k1 = processor.delegatee_key();
        let h1 = processor.signing_horizon().horizon_ms();
        assert!(h1 > h0);
        assert_ne!(k1.delegatee_public_key(), initial_pubkey);
        let snap_after_1 = processor
            .operator_snapshot()
            .await
            .expect("snapshot after first renewal");
        assert_eq!(snap_after_1.session_key.rotation_succeeded_total, 1);
        assert_eq!(snap_after_1.session_key.rotation_failed_total, 0);

        // One poll period later (still before `c1 + (h1 - c1) / 2`), the key has not churned.
        tokio::time::sleep(HORIZON_POLL).await;
        tokio::task::yield_now().await;
        assert_eq!(
            processor.delegatee_key().delegatee_public_key(),
            k1.delegatee_public_key()
        );
        let snap_mid = processor
            .operator_snapshot()
            .await
            .expect("snapshot mid-window");
        assert_eq!(snap_mid.session_key.rotation_succeeded_total, 1);
        assert_eq!(snap_mid.session_key.rotation_failed_total, 0);

        // Remove the delegator key before the second renewal and verify clean fallback exit at `h1`.
        fs::remove_file(&delegator_key_path).expect("remove delegator key");
        let exit_error = (&mut horizon_task)
            .await
            .expect("horizon task joins cleanly");
        assert!(matches!(
            exit_error,
            Error::DelegationHorizonReached { horizon_ms, .. } if horizon_ms == h1
        ));
        assert_eq!(now_ms(), h1);

        let _ = fs::remove_dir_all(&root);
    }
}
