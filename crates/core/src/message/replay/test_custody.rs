//! The custody law (#936): a relay delivery enters custody with its admission, is queued again
//! after a restart instead of refused as a replay, leaves custody on its release or once spent,
//! never otherwise, and custody is bounded.

use std::sync::Arc;

use super::Admission;
use super::RelayAdmission;
use super::RelayDelivery;
use super::ReplayRecord;
use super::StreamKey;
use super::TransactionDigest;
use super::TransactionReplay;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::replay::custody::RELAY_CUSTODY_CAPACITY;
use crate::message::MessageCategory;
use crate::storage::MemStorage;
use crate::utils::get_epoch_ms;

/// The stream of origin `origin` to destination 99.
fn stream(origin: u32) -> StreamKey {
    StreamKey::new(
        7,
        Did::from(origin),
        Did::from(99_u32),
        MessageCategory::Application,
    )
}

/// The relay delivery of sequence `sequence`, whose digest repeats `sequence`'s low byte, and
/// which is never spent.
fn delivery(sequence: u64) -> RelayDelivery {
    RelayDelivery {
        sequence,
        digest: TransactionDigest::new([sequence.to_le_bytes()[0]; 32]),
        spent_at_ms: u128::MAX,
    }
}

/// `delivery`, spent since the epoch's first instant.
fn spent(delivery: RelayDelivery) -> RelayDelivery {
    RelayDelivery {
        spent_at_ms: 1,
        ..delivery
    }
}

/// A replay runtime over `storage`, as one run of the node holds it.
fn run(storage: &Arc<MemStorage<ReplayRecord>>) -> Arc<TransactionReplay> {
    TransactionReplay::new_shared(Box::new(Arc::clone(storage)))
}

/// Admit `delivery` of `key` as relay mail.
async fn admit(
    replay: &Arc<TransactionReplay>,
    key: StreamKey,
    delivery: RelayDelivery,
) -> Result<RelayAdmission> {
    let admission = Admission {
        spent_at_ms: delivery.spent_at_ms,
        ..Admission::for_test(key, delivery.sequence, delivery.digest, get_epoch_ms())
    };
    replay.admit_relayed(admission).await
}

/// A restart between the admission and the take redelivers the mail once: the delivery is still
/// in custody, so it is queued again; once released it is a replay, and the live path, which
/// never consults custody, refuses it too.
#[tokio::test]
async fn test_a_restart_before_the_take_queues_the_delivery_again() -> Result<()> {
    let storage = Arc::new(MemStorage::new());
    assert_eq!(
        admit(&run(&storage), stream(1), delivery(0)).await?,
        RelayAdmission::Admitted
    );

    let restarted = run(&storage);
    assert_eq!(
        admit(&restarted, stream(1), delivery(0)).await?,
        RelayAdmission::InCustody
    );
    assert!(matches!(
        restarted.admit(stream(1), 0, delivery(0).digest).await,
        Err(Error::TransactionReplay { .. })
    ));
    restarted.release_relayed(stream(1), delivery(0)).await?;

    let after_release = run(&storage);
    assert!(matches!(
        admit(&after_release, stream(1), delivery(0)).await,
        Err(Error::TransactionReplay { .. })
    ));
    Ok(())
}

/// Mail behind live traffic inside the window fills its slot (`Late`) and enters custody; a
/// relay admission refused by the window enters nothing.
#[tokio::test]
async fn test_a_relay_admission_follows_the_window() -> Result<()> {
    let storage = Arc::new(MemStorage::new());
    let replay = run(&storage);
    replay.admit(stream(1), 5, delivery(5).digest).await?;
    assert_eq!(
        admit(&replay, stream(1), delivery(3)).await?,
        RelayAdmission::Admitted
    );
    let fork = RelayDelivery {
        digest: TransactionDigest::new([0xaa; 32]),
        ..delivery(5)
    };
    assert!(matches!(
        admit(&replay, stream(1), fork).await,
        Err(Error::TransactionSequenceFork { .. })
    ));
    assert_eq!(
        admit(&run(&storage), stream(1), delivery(3)).await?,
        RelayAdmission::InCustody
    );
    assert!(matches!(
        admit(&run(&storage), stream(1), fork).await,
        Err(Error::TransactionSequenceFork { .. })
    ));
    Ok(())
}

/// Custody holds at most `RELAY_CUSTODY_CAPACITY` deliveries over the whole store: a new one at
/// the bound is refused with nothing committed, and room returns once one is released.
#[tokio::test]
async fn test_custody_is_bounded_over_the_whole_store() -> Result<()> {
    let storage = Arc::new(MemStorage::new());
    let replay = run(&storage);
    for origin in 0..RELAY_CUSTODY_CAPACITY {
        let origin = u32::try_from(origin).map_err(|_| Error::InvalidCapacity)?;
        admit(&replay, stream(origin), delivery(0)).await?;
    }
    let beyond = stream(u32::MAX);
    assert!(matches!(
        admit(&replay, beyond, delivery(0)).await,
        Err(Error::RelayCustodyFull { .. })
    ));
    assert_eq!(
        replay.admit(beyond, 0, delivery(0).digest).await?,
        super::SequenceVerdict::First,
        "the refused relay admission committed nothing"
    );
    replay.release_relayed(stream(0), delivery(0)).await?;
    assert_eq!(
        admit(&replay, beyond, delivery(1)).await?,
        RelayAdmission::Admitted
    );
    Ok(())
}

/// An entry leaves custody once its transaction is spent, durably, and no sooner: the next
/// transition expires it, so its redelivery after a restart is a replay, while an entry not yet
/// spent stays in custody across the same transitions and restart.
#[tokio::test]
async fn test_custody_expires_a_delivery_only_once_spent() -> Result<()> {
    let storage = Arc::new(MemStorage::new());
    let replay = run(&storage);
    admit(&replay, stream(1), delivery(1)).await?;
    admit(&replay, stream(1), spent(delivery(0))).await?;
    admit(&replay, stream(1), delivery(2)).await?;

    let restarted = run(&storage);
    assert!(matches!(
        admit(&restarted, stream(1), spent(delivery(0))).await,
        Err(Error::TransactionReplay { .. })
    ));
    for sequence in [1, 2] {
        assert_eq!(
            admit(&restarted, stream(1), delivery(sequence)).await?,
            RelayAdmission::InCustody
        );
    }
    Ok(())
}
