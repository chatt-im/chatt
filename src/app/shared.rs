use std::{
    ops::{Deref, DerefMut},
    sync::Arc,
};

use parking_lot::{ArcRwLockWriteGuard, RawRwLock, RwLock};

/// Core-owned handle to state that render threads read under short-lived guards.
///
/// The owned write guard keeps the existing core code ergonomic: while a core
/// batch is active, field access dereferences directly to `T`. The runtime drops
/// the guard before waking render threads and reacquires it before processing the
/// next event batch.
pub(crate) struct CoreRw<T: ?Sized> {
    shared: Arc<RwLock<T>>,
    guard: Option<ArcRwLockWriteGuard<RawRwLock, T>>,
}

impl<T> CoreRw<T> {
    pub(crate) fn new(value: T) -> Self {
        let shared = Arc::new(RwLock::new(value));
        let guard = Some(shared.write_arc());
        Self { shared, guard }
    }
}

impl<T: ?Sized> CoreRw<T> {
    pub(crate) fn shared(&self) -> Arc<RwLock<T>> {
        self.shared.clone()
    }

    pub(crate) fn release(&mut self) {
        self.guard = None;
    }

    pub(crate) fn acquire(&mut self) {
        if self.guard.is_none() {
            self.guard = Some(self.shared.write_arc());
        }
    }

    fn guard(&self) -> &ArcRwLockWriteGuard<RawRwLock, T> {
        self.guard
            .as_ref()
            .expect("core state accessed outside a mutation batch")
    }

    fn guard_mut(&mut self) -> &mut ArcRwLockWriteGuard<RawRwLock, T> {
        self.guard
            .as_mut()
            .expect("core state accessed outside a mutation batch")
    }
}

impl<T: ?Sized> Deref for CoreRw<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.guard()
    }
}

impl<T: ?Sized> DerefMut for CoreRw<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard_mut()
    }
}
