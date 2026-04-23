//! **PEDAGOGICAL DEMO ONLY.** This crate exercises the loom
//! workspace scaffolding and serves as a template that Phase 3+
//! primitives copy.
//!
//! Do NOT depend on `Spinlock` from any other crate. Spinlocks
//! are almost always the wrong tool; when Phase 3+ lands real
//! primitives, use those.
//!
//! Ordering discipline: `Ordering` is imported from `std` on
//! both arms — loom re-exports the same type. The cfg split is
//! for `AtomicBool` and the cell module only.

use std::sync::atomic::Ordering;

#[cfg(loom)]
use loom::sync::atomic::AtomicBool;
#[cfg(not(loom))]
use std::sync::atomic::AtomicBool;

// Cell shim. The non-loom arm wraps std::cell::UnsafeCell with
// the SAME surface loom's UnsafeCell exposes (`new`, `with`,
// `with_mut`) — no escape hatch to raw pointers beyond what
// loom itself offers. This keeps the pattern loom-faithful:
// Phase 3+ authors copying this file cannot write code that
// "works under cargo test, broken under --cfg loom".
#[cfg(not(loom))]
mod cell {
    pub(crate) struct UnsafeCell<T>(std::cell::UnsafeCell<T>);

    impl<T> UnsafeCell<T> {
        pub(crate) const fn new(data: T) -> Self {
            Self(std::cell::UnsafeCell::new(data))
        }
        pub(crate) fn with<R>(&self, f: impl FnOnce(*const T) -> R) -> R {
            f(self.0.get())
        }
        pub(crate) fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
            f(self.0.get())
        }
    }
}

#[cfg(loom)]
mod cell {
    pub(crate) use loom::cell::UnsafeCell;
}

use cell::UnsafeCell;

#[doc(hidden)]
pub struct Spinlock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

// SAFETY: `locked` serializes access; the only path to `data`
// goes through `lock`, which enforces mutual exclusion.
// `T: Send` is required; `T: Sync` is not, because only one
// thread reads/writes `data` at a time. Precedent: std's
// `unsafe impl<T: ?Sized + Send> Sync for Mutex<T>`.
unsafe impl<T: Send> Send for Spinlock<T> {}
unsafe impl<T: Send> Sync for Spinlock<T> {}

impl<T> Spinlock<T> {
    pub fn new(data: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    pub fn lock(&self) -> SpinlockGuard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            #[cfg(loom)]
            loom::thread::yield_now();
            #[cfg(not(loom))]
            std::hint::spin_loop();
        }
        SpinlockGuard { lock: self }
    }
}

#[must_use = "SpinlockGuard holds the lock for its lifetime; a dropped guard immediately releases"]
#[doc(hidden)]
pub struct SpinlockGuard<'a, T> {
    lock: &'a Spinlock<T>,
}

impl<T> SpinlockGuard<'_, T> {
    /// Loom-aware read access. Prefer this in loom tests so the
    /// access is observed by the model.
    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        // SAFETY: guard's existence proves we hold the lock;
        // the cell shim ensures loom sees the access on the
        // loom arm.
        self.lock.data.with(|p| unsafe { f(&*p) })
    }

    /// Loom-aware write access. Prefer this in loom tests.
    pub fn with_mut<R>(&mut self, f: impl FnOnce(&mut T) -> R) -> R {
        // SAFETY: guard's existence proves exclusive access.
        self.lock.data.with_mut(|p| unsafe { f(&mut *p) })
    }
}

// Deref/DerefMut on the non-loom arm are a convenience for
// normal (non-loom) use. Under loom we suppress them so tests
// are forced to use .with()/.with_mut(), which route through
// loom's cell tracking. The non-loom impls ALSO route through
// .with()/.with_mut() (via the shim) so the API shape is
// congruent with loom's — no raw-pointer escape hatch.
#[cfg(not(loom))]
impl<T> std::ops::Deref for SpinlockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: by construction (`Spinlock::lock`), a
        // `SpinlockGuard<'a, T>` exists only after a successful
        // `compare_exchange_weak(false, true, Acquire, _)` on
        // `self.lock.locked`, and `Drop` releases with
        // `Ordering::Release`. While this guard lives, no other
        // thread can obtain a second guard on the same Spinlock,
        // so we hold exclusive access to `data`. The guard's 'a
        // lifetime bounds the returned reference; the closure's
        // return type unifies with `&T` so the pointer
        // dereference is lifetime-bounded to `&self`. Precedent:
        // std::sync::Mutex's `unsafe impl<T: ?Sized + Send> Sync
        // for Mutex<T>` relies on the same construction-time
        // invariant.
        self.lock.data.with(|p| unsafe { &*p })
    }
}

#[cfg(not(loom))]
impl<T> std::ops::DerefMut for SpinlockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: same guard-existence invariant as in `deref`,
        // upgraded to exclusive access because we hold `&mut self`
        // on the guard — no aliased `Deref` reference can be live
        // concurrently with this `DerefMut` call.
        self.lock.data.with_mut(|p| unsafe { &mut *p })
    }
}

impl<T> Drop for SpinlockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}
