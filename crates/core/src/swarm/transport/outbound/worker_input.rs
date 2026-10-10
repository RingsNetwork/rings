//! The outbound worker's input: the next command, settled delivery, or reserved credit, chosen
//! by whichever is ready first.

use futures::pin_mut;
use futures::select;
use futures::FutureExt;
use futures::StreamExt;

use super::class_credit::CreditWaitOutput;
use super::DeliveryEvent;
use super::OutboundCommand;
use super::OutboundWorker;

impl OutboundWorker {
    /// Wait for the next input: a command, a settled delivery, or a reserved credit.
    pub(super) async fn wait_for_input(&mut self) {
        enum WorkerInput {
            Command(Option<OutboundCommand>),
            Delivery(DeliveryEvent),
            Credit(CreditWaitOutput),
        }

        // An empty `FuturesUnordered` is a terminated fused stream, so `select!` skips it.
        let input = {
            let command = self.receiver.next().fuse();
            pin_mut!(command);
            select! {
                command = command => WorkerInput::Command(command),
                event = self.deliveries.select_next_some() => WorkerInput::Delivery(event),
                credit = self.credit_waits.select_next_some() => WorkerInput::Credit(credit),
            }
        };
        match input {
            WorkerInput::Command(Some(command)) => self.handle_commands([command]),
            WorkerInput::Command(None) => self.input_closed = true,
            WorkerInput::Delivery(event) => self.handle_delivery(event),
            WorkerInput::Credit((class, wait, attempt, credit)) => {
                self.settle_credit(class, wait, attempt, credit);
            }
        }
    }
}
