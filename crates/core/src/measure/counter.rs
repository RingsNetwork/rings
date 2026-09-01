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

impl MeasureCounter {
    /// Discard byte payloads and name the compatibility counter for an event.
    pub const fn from_event(event: rings_measure::MeasurementEvent) -> Self {
        match event {
            rings_measure::MeasurementEvent::Connected => Self::Connect,
            rings_measure::MeasurementEvent::Disconnected => Self::Disconnected,
            rings_measure::MeasurementEvent::Sent { .. } => Self::Sent,
            rings_measure::MeasurementEvent::FailedToSend => Self::FailedToSend,
            rings_measure::MeasurementEvent::Received { .. } => Self::Received,
            rings_measure::MeasurementEvent::FailedToReceive => Self::FailedToReceive,
        }
    }

    /// Construct a zero-byte event for a legacy counter increment.
    ///
    /// This bridge preserves reliability counts but cannot invent byte credit.
    pub const fn into_event(self) -> rings_measure::MeasurementEvent {
        match self {
            Self::Sent => rings_measure::MeasurementEvent::Sent { useful_bytes: 0 },
            Self::FailedToSend => rings_measure::MeasurementEvent::FailedToSend,
            Self::Received => rings_measure::MeasurementEvent::Received { useful_bytes: 0 },
            Self::FailedToReceive => rings_measure::MeasurementEvent::FailedToReceive,
            Self::Connect => rings_measure::MeasurementEvent::Connected,
            Self::Disconnected => rings_measure::MeasurementEvent::Disconnected,
        }
    }
}
