use super::INBOUND_CALLBACK_TIMEOUT;
use crate::dht::Did;
use crate::error::Error;
use crate::swarm::callback::CallbackError;

pub(super) enum InboundFailure {
    Core(Error),
    Validation(CallbackError),
    Callback(CallbackError),
    ValidationTimeout {
        peer: Option<Did>,
    },
    ProcessingTimeout {
        peer: Option<Did>,
    },
    TimerUnavailable {
        peer: Option<Did>,
        operation: &'static str,
    },
}

pub(super) fn inbound_failure_error(failure: InboundFailure) -> Error {
    match failure {
        InboundFailure::Core(error) => error,
        InboundFailure::Validation(source) => Error::InboundValidationFailed { source },
        InboundFailure::Callback(source) => Error::InboundCallbackFailed { source },
        InboundFailure::ValidationTimeout { peer } => Error::InboundValidationTimeout {
            peer,
            timeout_ms: INBOUND_CALLBACK_TIMEOUT.as_millis(),
        },
        InboundFailure::ProcessingTimeout { peer } => Error::InboundProcessingTimeout {
            peer,
            timeout_ms: INBOUND_CALLBACK_TIMEOUT.as_millis(),
        },
        InboundFailure::TimerUnavailable { peer, operation } => {
            Error::InboundTimerUnavailable { peer, operation }
        }
    }
}
