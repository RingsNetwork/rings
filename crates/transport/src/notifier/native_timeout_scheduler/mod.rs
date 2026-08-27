use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use super::Notifier;

static TIMER_SCHEDULER: OnceLock<Option<mpsc::Sender<ScheduledWake>>> = OnceLock::new();
static TIMER_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct ScheduledWake {
    deadline: Instant,
    sequence: u64,
    notifier: Notifier,
}

impl ScheduledWake {
    fn new(notifier: Notifier, millis: u64) -> Self {
        let now = Instant::now();
        let duration = Duration::from_millis(millis);
        let deadline = now.checked_add(duration).unwrap_or(now);
        let sequence = TIMER_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
        Self {
            deadline,
            sequence,
            notifier,
        }
    }

    #[cfg(test)]
    fn at(notifier: Notifier, deadline: Instant, sequence: u64) -> Self {
        Self {
            deadline,
            sequence,
            notifier,
        }
    }
}

impl Ord for ScheduledWake {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .deadline
            .cmp(&self.deadline)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

impl PartialOrd for ScheduledWake {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for ScheduledWake {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.sequence == other.sequence
    }
}

impl Eq for ScheduledWake {}

pub(super) fn schedule_wake(notifier: Notifier, millis: u64) {
    let request = ScheduledWake::new(notifier.clone(), millis);
    send_or_wake(notifier, request, scheduler());
}

fn send_or_wake(
    notifier: Notifier,
    request: ScheduledWake,
    sender: Option<&mpsc::Sender<ScheduledWake>>,
) {
    let scheduled = sender
        .map(|sender| sender.send(request).is_ok())
        .unwrap_or(false);
    if !scheduled {
        notifier.wake();
    }
}

fn scheduler() -> Option<&'static mpsc::Sender<ScheduledWake>> {
    TIMER_SCHEDULER.get_or_init(spawn_timer_thread).as_ref()
}

fn spawn_timer_thread() -> Option<mpsc::Sender<ScheduledWake>> {
    let (sender, receiver) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("rings-transport-notifier-timer".to_string())
        .spawn(move || run_timer_thread(receiver));
    match thread {
        Ok(_) => Some(sender),
        Err(error) => {
            tracing::error!("failed to start notifier timer scheduler: {:?}", error);
            None
        }
    }
}

fn run_timer_thread(receiver: mpsc::Receiver<ScheduledWake>) {
    let mut pending: BinaryHeap<ScheduledWake> = BinaryHeap::new();
    loop {
        if let Some(next) = pending.peek() {
            let now = Instant::now();
            if next.deadline <= now {
                if let Some(request) = pending.pop() {
                    request.notifier.wake();
                }
                continue;
            }

            match receiver.recv_timeout(next.deadline.saturating_duration_since(now)) {
                Ok(request) => pending.push(request),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match receiver.recv() {
                Ok(request) => pending.push(request),
                Err(_) => return,
            }
        }
    }
}

#[cfg(test)]
mod tests;
