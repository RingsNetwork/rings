//! Injectable cryptographic entropy capability for protocol and session state machines.

use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use rand::rngs::OsRng;
use rand::rngs::StdRng;
use rand::CryptoRng;
use rand::RngCore;
use rand::SeedableRng;

const UUID_VERSION_4: u8 = 0x40;
const UUID_RFC4122_VARIANT: u8 = 0x80;

/// Object-safe cryptographically strong random byte stream usable across threads.
pub trait CryptoRngCore: RngCore + CryptoRng + Send {}

impl<T: RngCore + CryptoRng + Send> CryptoRngCore for T {}

/// Default entropy source backed by the operating system CSPRNG (or the active thread-local
/// `dummy_simulation` override during deterministic `dummy` tests).
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemEntropy;

impl RngCore for SystemEntropy {
    fn next_u32(&mut self) -> u32 {
        let mut bytes = [0_u8; 4];
        self.fill_bytes(&mut bytes);
        u32::from_le_bytes(bytes)
    }

    fn next_u64(&mut self) -> u64 {
        let mut bytes = [0_u8; 8];
        self.fill_bytes(&mut bytes);
        u64::from_le_bytes(bytes)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        if crate::simulation::try_fill_entropy_bytes(dest) {
            return;
        }
        #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
        if crate::dummy_simulation::try_fill_entropy_bytes(dest) {
            return;
        }
        OsRng.fill_bytes(dest);
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand::Error> {
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        if crate::simulation::try_fill_entropy_bytes(dest) {
            return Ok(());
        }
        #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
        if crate::dummy_simulation::try_fill_entropy_bytes(dest) {
            return Ok(());
        }
        OsRng.try_fill_bytes(dest)
    }
}

impl CryptoRng for SystemEntropy {}

/// Exclusive lock guard over a [`SharedRng`] stream.
///
/// Implements [`RngCore`] and [`CryptoRng`] as a `Sized` type so generic cryptographic builders
/// (`rng: &mut (impl CryptoRng + RngCore)`) can draw multiple values atomically without
/// interleaving concurrent callers.
pub struct SharedRngGuard<'a> {
    guard: MutexGuard<'a, Box<dyn CryptoRngCore>>,
}

impl fmt::Debug for SharedRngGuard<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedRngGuard").finish_non_exhaustive()
    }
}

impl RngCore for SharedRngGuard<'_> {
    fn next_u32(&mut self) -> u32 {
        self.guard.next_u32()
    }

    fn next_u64(&mut self) -> u64 {
        self.guard.next_u64()
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.guard.fill_bytes(dest);
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand::Error> {
        self.guard.try_fill_bytes(dest)
    }
}

impl CryptoRng for SharedRngGuard<'_> {}

/// Shared, cloneable cryptographic RNG capability.
///
/// Production code defaults to [`SharedRng::system`], while tests and trace replay harnesses
/// inject [`SharedRng::seed_from_u64`], [`SharedRng::from_seed`], or [`SharedRng::from_rng`].
#[derive(Clone)]
pub struct SharedRng {
    inner: Arc<Mutex<Box<dyn CryptoRngCore>>>,
}

impl Default for SharedRng {
    fn default() -> Self {
        Self::system()
    }
}

impl fmt::Debug for SharedRng {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedRng").finish_non_exhaustive()
    }
}

impl SharedRng {
    /// Construct an entropy capability backed by [`SystemEntropy`].
    pub fn system() -> Self {
        Self::from_rng(SystemEntropy)
    }

    /// Construct a deterministic CSPRNG capability seeded from `seed`.
    pub fn seed_from_u64(seed: u64) -> Self {
        Self::from_rng(StdRng::seed_from_u64(seed))
    }

    /// Construct a deterministic CSPRNG capability seeded from 32 bytes.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self::from_rng(StdRng::from_seed(seed))
    }

    /// Wrap an arbitrary caller-supplied cryptographic RNG.
    pub fn from_rng<R: CryptoRngCore + 'static>(rng: R) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Box::new(rng))),
        }
    }

    /// Acquire exclusive access to the underlying RNG stream.
    pub fn lock_rng(&self) -> SharedRngGuard<'_> {
        let guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        SharedRngGuard { guard }
    }

    /// Run `f` with exclusive access to the underlying RNG stream.
    pub fn with_rng<T>(&self, f: impl FnOnce(&mut SharedRngGuard<'_>) -> T) -> T {
        let mut guard = self.lock_rng();
        f(&mut guard)
    }

    /// Draw an RFC 4122 version-4 UUID from this capability's byte stream.
    #[cfg(any(feature = "std", feature = "wasm"))]
    pub fn next_uuid(&self) -> uuid::Uuid {
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        if let Some(uuid) = crate::simulation::next_uuid_override() {
            return uuid;
        }
        let mut bytes = [0_u8; 16];
        self.lock_rng().fill_bytes(&mut bytes);
        let [_, _, _, _, _, _, ref mut version, _, ref mut variant, ..] = bytes;
        *version = (*version & 0x0f) | UUID_VERSION_4;
        *variant = (*variant & 0x3f) | UUID_RFC4122_VARIANT;
        uuid::Uuid::from_bytes(bytes)
    }
}

impl RngCore for SharedRng {
    fn next_u32(&mut self) -> u32 {
        self.lock_rng().next_u32()
    }

    fn next_u64(&mut self) -> u64 {
        self.lock_rng().next_u64()
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.lock_rng().fill_bytes(dest);
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand::Error> {
        self.lock_rng().try_fill_bytes(dest)
    }
}

impl CryptoRng for SharedRng {}
