use rings_transport::connections::dummy_controlled;

pub(crate) struct ControlledDeliveryGuard;

impl ControlledDeliveryGuard {
    pub(crate) fn new() -> Self {
        dummy_controlled::enable(true);
        Self
    }
}

impl Drop for ControlledDeliveryGuard {
    fn drop(&mut self) {
        dummy_controlled::enable(false);
    }
}

pub(super) struct PendingDataChannelOpenGuard;

impl PendingDataChannelOpenGuard {
    pub(super) fn new() -> Self {
        dummy_controlled::set_wait_for_data_channel_open_pending(true);
        Self
    }
}

impl Drop for PendingDataChannelOpenGuard {
    fn drop(&mut self) {
        dummy_controlled::set_wait_for_data_channel_open_pending(false);
    }
}

pub(crate) struct PendingSendGuard;

impl PendingSendGuard {
    pub(crate) fn new() -> Self {
        dummy_controlled::set_send_message_pending(true);
        Self
    }
}

impl Drop for PendingSendGuard {
    fn drop(&mut self) {
        dummy_controlled::set_send_message_pending(false);
    }
}

pub(super) struct PausedDispatchGuard;

impl PausedDispatchGuard {
    pub(super) fn new() -> Self {
        dummy_controlled::pause_send_message_at_dispatch();
        Self
    }
}

impl Drop for PausedDispatchGuard {
    fn drop(&mut self) {
        dummy_controlled::release_send_message_gate();
    }
}

pub(super) struct PausedIrrevocableSendGuard;

impl PausedIrrevocableSendGuard {
    pub(super) fn new() -> Self {
        dummy_controlled::pause_irrevocable_send();
        Self
    }
}

impl Drop for PausedIrrevocableSendGuard {
    fn drop(&mut self) {
        dummy_controlled::release_irrevocable_send_gate();
    }
}

pub(super) struct PendingAfterSentCountGuard;

impl PendingAfterSentCountGuard {
    pub(super) fn new(threshold: usize) -> Self {
        dummy_controlled::set_send_message_pending_after_sent_count(Some(threshold));
        Self
    }
}

impl Drop for PendingAfterSentCountGuard {
    fn drop(&mut self) {
        dummy_controlled::set_send_message_pending_after_sent_count(None);
    }
}

pub(super) struct PendingDeliveryGuard;

impl PendingDeliveryGuard {
    pub(super) fn new() -> Self {
        dummy_controlled::set_delivery_future_pending(true);
        Self
    }
}

impl Drop for PendingDeliveryGuard {
    fn drop(&mut self) {
        dummy_controlled::set_delivery_future_pending(false);
    }
}

pub(super) struct PendingCloseGuard;

impl PendingCloseGuard {
    pub(super) fn new() -> Self {
        dummy_controlled::set_close_pending(true);
        Self
    }
}

impl Drop for PendingCloseGuard {
    fn drop(&mut self) {
        dummy_controlled::set_close_pending(false);
    }
}

pub(super) struct PausedDeliveryGuard;

impl PausedDeliveryGuard {
    pub(super) fn new() -> Self {
        dummy_controlled::pause_next_delivery_future();
        Self
    }
}

impl Drop for PausedDeliveryGuard {
    fn drop(&mut self) {
        dummy_controlled::release_delivery_future_gate();
    }
}

pub(super) struct MaxMessageSizeGuard;

impl MaxMessageSizeGuard {
    pub(super) fn new(size: usize) -> Self {
        dummy_controlled::set_max_message_size(size);
        Self
    }
}

impl Drop for MaxMessageSizeGuard {
    fn drop(&mut self) {
        dummy_controlled::set_max_message_size(0);
    }
}
