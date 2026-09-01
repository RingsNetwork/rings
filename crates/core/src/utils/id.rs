/// Generate a protocol identifier through the active effect boundary.
///
/// Transactions and chunk-reassembly groups share this source so deterministic
/// simulations can replay every protocol identity exactly.
pub(crate) fn new_uuid() -> uuid::Uuid {
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    if let Some(uuid) = crate::simulation::next_uuid_override() {
        return uuid;
    }
    uuid::Uuid::new_v4()
}
