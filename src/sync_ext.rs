//! Poison-tolerant locking.
//!
//! jwm spawns worker threads (async blur, wallpaper loaders, KMS helpers) and is
//! built with `panic = "unwind"` so a single worker panic is isolated instead of
//! aborting the process. That isolation is defeated if the worker panics while
//! holding a shared `Mutex`: the lock becomes poisoned and every later
//! `.lock().unwrap()` on the render loop panics in turn, cascading into a full
//! compositor crash. Recovering the guard via `PoisonError::into_inner` keeps the
//! compositor alive on data that is, at worst, slightly inconsistent — strictly
//! better than dying.

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub trait MutexExt<T: ?Sized> {
    /// Lock, recovering the guard even if the mutex was poisoned by a panicking
    /// thread. Use for shared mutexes on crash-sensitive compositor/render paths
    /// where a worker panic should not cascade into a full process crash.
    fn lock_safe(&self) -> MutexGuard<'_, T>;
}

impl<T: ?Sized> MutexExt<T> for Mutex<T> {
    #[inline]
    fn lock_safe(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub trait RwLockExt<T: ?Sized> {
    /// Read-lock, recovering on poison. See [`MutexExt::lock_safe`].
    fn read_safe(&self) -> RwLockReadGuard<'_, T>;
    /// Write-lock, recovering on poison. See [`MutexExt::lock_safe`].
    fn write_safe(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T: ?Sized> RwLockExt<T> for RwLock<T> {
    #[inline]
    fn read_safe(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(PoisonError::into_inner)
    }
    #[inline]
    fn write_safe(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(PoisonError::into_inner)
    }
}
