#![cfg_attr(not(test), no_std)]
#![allow(async_fn_in_trait)]

//! Isolated, quota-backed configuration spaces for embedded components.
//!
//! The manager deliberately knows nothing about Wi-Fi, TLS, authentication,
//! GPIOs, or any other configuration schema. A component claims one opaque
//! byte space with a maximum payload size; after the claim succeeds, only
//! that component receives the resulting [ConfigSpace] handle and owns the
//! encoding of the bytes stored inside it.
//!
//! Backends define the physical accounting model. This matters for stores
//! such as ESP NVS where "N payload bytes" does not consume exactly N bytes
//! of flash. [ConfigBackend::reservation_units] converts a logical [Budget]
//! into backend-specific capacity units, while [ConfigManager] only prevents
//! over-commit.
//!
//! A successful claim is therefore a boot-lifetime guarantee: later claims
//! cannot consume the capacity already reserved for it.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// Maximum opaque payload a component promises to keep in its space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    max_bytes: usize,
}

impl Budget {
    pub const fn new(max_bytes: usize) -> Self {
        Self { max_bytes }
    }

    pub const fn max_bytes(self) -> usize {
        self.max_bytes
    }
}

/// One durable value returned by a backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Monotonically increasing generation assigned by the backend.
    pub generation: u64,
    /// Opaque component-owned bytes.
    pub data: Vec<u8>,
}

/// Persistence boundary used by [ConfigManager] and [ConfigSpace].
///
/// Implementations own the physical storage details and the translation from
/// logical payload bytes to reservation units. For ESP NVS, for example, an
/// implementation can conservatively account for entry/page overhead instead
/// of pretending one payload byte equals one flash byte.
///
/// commit is expected to publish a complete replacement of one space. A
/// backend which needs power-cut atomicity must provide it internally; the
/// manager never decomposes a component blob into keys.
pub trait ConfigBackend: Clone {
    type Error;

    /// Total reservable units offered by this backend instance.
    fn capacity_units(&self) -> usize;

    /// Physical/logical units needed to guarantee one space with budget.
    ///
    /// None means that this backend cannot support the requested budget at
    /// all, even if otherwise empty.
    fn reservation_units(&self, budget: Budget) -> Option<usize>;

    async fn load(&self, space: &str) -> Result<Option<Snapshot>, Self::Error>;

    /// Atomically replace the current opaque value and return its generation.
    async fn commit(&self, space: &str, data: &[u8]) -> Result<u64, Self::Error>;

    /// Remove the current value and return the new generation.
    async fn clear(&self, space: &str) -> Result<u64, Self::Error>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimError {
    EmptyName,
    ZeroBudget,
    DuplicateName,
    UnsupportedBudget,
    CapacityOverflow,
    NoCapacity {
        requested_units: usize,
        remaining_units: usize,
    },
}

#[derive(Debug)]
pub enum SpaceError<E> {
    Backend(E),
    TooLarge { size: usize, max: usize },
    StoredValueExceedsClaim { size: usize, max: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Claim {
    name: String,
    budget: Budget,
    units: usize,
}

/// Allocates isolated configuration spaces on top of one persistence backend.
///
/// Claims are first-come-first-served within one boot. System firmware that
/// wants deterministic admission should perform all expected claims in a
/// fixed boot sequence before spawning the component tasks.
pub struct ConfigManager<B> {
    backend: B,
    claims: Vec<Claim>,
    used_units: usize,
}

impl<B> ConfigManager<B>
where
    B: ConfigBackend,
{
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            claims: Vec::new(),
            used_units: 0,
        }
    }

    pub fn capacity_units(&self) -> usize {
        self.backend.capacity_units()
    }

    pub fn used_units(&self) -> usize {
        self.used_units
    }

    pub fn remaining_units(&self) -> usize {
        self.capacity_units().saturating_sub(self.used_units)
    }

    pub fn claim_count(&self) -> usize {
        self.claims.len()
    }

    /// Reserve an isolated space for one component.
    ///
    /// A duplicate name is rejected rather than returning a second handle:
    /// ownership of a space is intentionally unique.
    pub fn claim(&mut self, name: &str, budget: Budget) -> Result<ConfigSpace<B>, ClaimError> {
        if name.is_empty() {
            return Err(ClaimError::EmptyName);
        }
        if budget.max_bytes == 0 {
            return Err(ClaimError::ZeroBudget);
        }
        if self.claims.iter().any(|claim| claim.name == name) {
            return Err(ClaimError::DuplicateName);
        }

        let units = self
            .backend
            .reservation_units(budget)
            .ok_or(ClaimError::UnsupportedBudget)?;
        let next_used = self
            .used_units
            .checked_add(units)
            .ok_or(ClaimError::CapacityOverflow)?;

        let capacity = self.capacity_units();
        if next_used > capacity {
            return Err(ClaimError::NoCapacity {
                requested_units: units,
                remaining_units: capacity.saturating_sub(self.used_units),
            });
        }

        self.used_units = next_used;
        self.claims.push(Claim {
            name: String::from(name),
            budget,
            units,
        });

        Ok(ConfigSpace {
            backend: self.backend.clone(),
            name: String::from(name),
            budget,
        })
    }
}

/// Capability handed to exactly one component after a successful claim.
///
/// It exposes only that component's opaque value; there is no API for
/// enumerating or opening another component's space.
#[derive(Clone)]
pub struct ConfigSpace<B> {
    backend: B,
    name: String,
    budget: Budget,
}

impl<B> ConfigSpace<B>
where
    B: ConfigBackend,
{
    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn budget(&self) -> Budget {
        self.budget
    }

    pub async fn load(&self) -> Result<Option<Snapshot>, SpaceError<B::Error>> {
        let value = self
            .backend
            .load(&self.name)
            .await
            .map_err(SpaceError::Backend)?;

        if let Some(snapshot) = &value
            && snapshot.data.len() > self.budget.max_bytes
        {
            return Err(SpaceError::StoredValueExceedsClaim {
                size: snapshot.data.len(),
                max: self.budget.max_bytes,
            });
        }

        Ok(value)
    }

    pub async fn commit(&self, data: &[u8]) -> Result<u64, SpaceError<B::Error>> {
        if data.len() > self.budget.max_bytes {
            return Err(SpaceError::TooLarge {
                size: data.len(),
                max: self.budget.max_bytes,
            });
        }

        self.backend
            .commit(&self.name, data)
            .await
            .map_err(SpaceError::Backend)
    }

    pub async fn clear(&self) -> Result<u64, SpaceError<B::Error>> {
        self.backend
            .clear(&self.name)
            .await
            .map_err(SpaceError::Backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use core::cell::RefCell;
    use core::future::Future;
    use core::pin::Pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    use std::rc::Rc;

    fn block_on<F: Future>(mut future: F) -> F::Output {
        unsafe fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(core::ptr::null(), &VTABLE)
        }
        unsafe fn noop(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);

        let raw = RawWaker::new(core::ptr::null(), &VTABLE);
        let waker = unsafe { Waker::from_raw(raw) };
        let mut cx = Context::from_waker(&waker);
        let mut future = unsafe { Pin::new_unchecked(&mut future) };

        loop {
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[derive(Default)]
    struct MemoryState {
        values: BTreeMap<String, Snapshot>,
        generations: BTreeMap<String, u64>,
    }

    #[derive(Clone)]
    struct MemoryBackend {
        state: Rc<RefCell<MemoryState>>,
        capacity: usize,
        overhead: usize,
    }

    impl MemoryBackend {
        fn new(capacity: usize, overhead: usize) -> Self {
            Self {
                state: Rc::new(RefCell::new(MemoryState::default())),
                capacity,
                overhead,
            }
        }

        fn inject(&self, space: &str, data: Vec<u8>) {
            self.state.borrow_mut().values.insert(
                String::from(space),
                Snapshot {
                    generation: 1,
                    data,
                },
            );
        }
    }

    impl ConfigBackend for MemoryBackend {
        type Error = ();

        fn capacity_units(&self) -> usize {
            self.capacity
        }

        fn reservation_units(&self, budget: Budget) -> Option<usize> {
            budget.max_bytes.checked_add(self.overhead)
        }

        async fn load(&self, space: &str) -> Result<Option<Snapshot>, Self::Error> {
            Ok(self.state.borrow().values.get(space).cloned())
        }

        async fn commit(&self, space: &str, data: &[u8]) -> Result<u64, Self::Error> {
            let mut state = self.state.borrow_mut();
            let generation = state
                .generations
                .get(space)
                .copied()
                .unwrap_or(0)
                .checked_add(1)
                .unwrap();
            state.generations.insert(String::from(space), generation);
            state.values.insert(
                String::from(space),
                Snapshot {
                    generation,
                    data: data.to_vec(),
                },
            );
            Ok(generation)
        }

        async fn clear(&self, space: &str) -> Result<u64, Self::Error> {
            let mut state = self.state.borrow_mut();
            let generation = state
                .generations
                .get(space)
                .copied()
                .unwrap_or(0)
                .checked_add(1)
                .unwrap();
            state.generations.insert(String::from(space), generation);
            state.values.remove(space);
            Ok(generation)
        }
    }

    #[test]
    fn claims_reserve_backend_specific_capacity() {
        let backend = MemoryBackend::new(1_000, 100);
        let mut manager = ConfigManager::new(backend);

        let wifi = manager.claim("wifi", Budget::new(300)).unwrap();
        assert_eq!(wifi.budget().max_bytes(), 300);
        assert_eq!(manager.used_units(), 400);
        assert_eq!(manager.remaining_units(), 600);

        let tls = manager.claim("tls", Budget::new(500)).unwrap();
        assert_eq!(tls.name(), "tls");
        assert_eq!(manager.used_units(), 1_000);
        assert_eq!(manager.remaining_units(), 0);
    }

    #[test]
    fn refuses_overcommit_before_any_write_occurs() {
        let backend = MemoryBackend::new(1_000, 100);
        let mut manager = ConfigManager::new(backend);
        manager.claim("wifi", Budget::new(300)).unwrap();

        assert_eq!(
            manager.claim("tls", Budget::new(600)).err(),
            Some(ClaimError::NoCapacity {
                requested_units: 700,
                remaining_units: 600,
            })
        );
    }

    #[test]
    fn ownership_is_unique() {
        let backend = MemoryBackend::new(1_000, 0);
        let mut manager = ConfigManager::new(backend);
        manager.claim("wifi", Budget::new(100)).unwrap();

        assert_eq!(
            manager.claim("wifi", Budget::new(100)).err(),
            Some(ClaimError::DuplicateName)
        );
    }

    #[test]
    fn rejects_invalid_claims() {
        let backend = MemoryBackend::new(1_000, 0);
        let mut manager = ConfigManager::new(backend);

        assert_eq!(
            manager.claim("", Budget::new(1)).err(),
            Some(ClaimError::EmptyName)
        );
        assert_eq!(
            manager.claim("wifi", Budget::new(0)).err(),
            Some(ClaimError::ZeroBudget)
        );
    }

    #[test]
    fn space_enforces_its_own_payload_limit() {
        let backend = MemoryBackend::new(1_000, 0);
        let mut manager = ConfigManager::new(backend);
        let wifi = manager.claim("wifi", Budget::new(4)).unwrap();

        let err = block_on(wifi.commit(b"12345")).unwrap_err();
        assert!(matches!(
            err,
            SpaceError::TooLarge { size: 5, max: 4 }
        ));
    }

    #[test]
    fn components_only_access_their_own_space() {
        let backend = MemoryBackend::new(1_000, 0);
        let mut manager = ConfigManager::new(backend);
        let wifi = manager.claim("wifi", Budget::new(32)).unwrap();
        let tls = manager.claim("tls", Budget::new(32)).unwrap();

        block_on(wifi.commit(b"wifi-secret")).unwrap();
        block_on(tls.commit(b"tls-secret")).unwrap();

        assert_eq!(
            block_on(wifi.load()).unwrap().unwrap().data,
            b"wifi-secret"
        );
        assert_eq!(
            block_on(tls.load()).unwrap().unwrap().data,
            b"tls-secret"
        );
    }

    #[test]
    fn generation_advances_on_commit_and_clear() {
        let backend = MemoryBackend::new(1_000, 0);
        let mut manager = ConfigManager::new(backend);
        let space = manager.claim("wifi", Budget::new(32)).unwrap();

        assert_eq!(block_on(space.commit(b"a")).unwrap(), 1);
        assert_eq!(block_on(space.commit(b"b")).unwrap(), 2);
        assert_eq!(block_on(space.clear()).unwrap(), 3);
        assert!(block_on(space.load()).unwrap().is_none());
    }

    #[test]
    fn oversized_persisted_value_is_reported_as_corruption() {
        let backend = MemoryBackend::new(1_000, 0);
        backend.inject("wifi", b"12345".to_vec());

        let mut manager = ConfigManager::new(backend);
        let wifi = manager.claim("wifi", Budget::new(4)).unwrap();

        let err = block_on(wifi.load()).unwrap_err();
        assert!(matches!(
            err,
            SpaceError::StoredValueExceedsClaim { size: 5, max: 4 }
        ));
    }
}
