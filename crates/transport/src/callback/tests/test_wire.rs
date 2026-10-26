use bytes::Bytes;

use crate::core::credit::LaneSignal;
use crate::core::transport::BorrowedTransportMessage;
use crate::core::transport::TransportMessage;

#[test]
fn test_borrowed_and_owned_transport_envelopes_share_the_complete_wire_schema() {
    let messages = [
        TransportMessage::Custom(Bytes::from_static(b"payload")),
        TransportMessage::Signal(LaneSignal::Credit {
            limit: u64::MAX,
            generation: u64::MAX,
        }),
        TransportMessage::Signal(LaneSignal::Shrink {
            limit: 1,
            generation: 2,
        }),
        TransportMessage::Signal(LaneSignal::Returned {
            bound: 3,
            generation: 4,
        }),
    ];

    for message in messages {
        let raw = rings_codec::serialize(&message).expect("transport frame must serialize");
        let (borrowed, remaining) =
            rings_codec::deserialize_prefix::<BorrowedTransportMessage>(&raw)
                .expect("borrowed envelope must decode every owned variant");
        assert!(remaining.is_empty());
        match (message, borrowed) {
            (TransportMessage::Custom(owned), BorrowedTransportMessage::Custom(view)) => {
                assert_eq!(owned.as_ref(), view);
            }
            (TransportMessage::Signal(owned), BorrowedTransportMessage::Signal(view)) => {
                assert_eq!(owned, view);
            }
            _ => panic!("borrowed and owned envelopes disagree on the variant"),
        }
    }
}
