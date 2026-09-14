use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use super::*;

struct DropWitness(Arc<AtomicBool>);

impl Drop for DropWitness {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), tokio::test)]
async fn test_step_deadline_drops_work_that_does_not_complete() {
    let dropped = Arc::new(AtomicBool::new(false));
    let witness = dropped.clone();
    let future = async move {
        let _witness = DropWitness(witness);
        futures::future::pending::<()>().await;
        Ok(())
    };

    let result = await_step_deadline(future, Duration::from_millis(1)).await;

    assert!(matches!(result, StepDeadline::TimedOut));
    assert!(dropped.load(Ordering::Acquire));
}
