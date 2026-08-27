/// The tag of counters in measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MeasureCounter {
    /// The number of sent messages.
    Sent,
    /// The number of failed to sent messages.
    FailedToSend,
    /// The number of received messages.
    Received,
    /// The number of failed to receive messages.
    FailedToReceive,
    /// The number of connected.
    Connect,
    /// The number of disconnect.
    Disconnected,
}
