//! Locks that one panic doesn't break for everyone else.
//!
//! A thread that panics while holding a `std::sync::Mutex` poisons it,
//! and every later `lock().unwrap()` on it panics too: in peer tasks,
//! calls and the window alike, so one fault would stop the lot. What a
//! lock guards here is always whole between statements (a list, a
//! map, a few flags), so the next one to take it carries on with it as
//! it is, as `store` always has.

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub trait Locked<T: ?Sized> {
    /// The lock, poisoned or not.
    fn locked(&self) -> MutexGuard<'_, T>;
}

impl<T: ?Sized> Locked<T> for Mutex<T> {
    fn locked(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub trait Shared<T: ?Sized> {
    /// To read, poisoned or not.
    fn reading(&self) -> RwLockReadGuard<'_, T>;
    /// To write, poisoned or not.
    fn writing(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T: ?Sized> Shared<T> for RwLock<T> {
    fn reading(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(PoisonError::into_inner)
    }
    fn writing(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_lock_held_by_a_thread_that_panicked_is_still_there_for_the_rest() {
        let list = Arc::new(Mutex::new(vec![1]));
        let held = list.clone();
        let panicked = std::thread::spawn(move || {
            let mut list = held.locked();
            list.push(2);
            panic!("a fault while holding it");
        })
        .join();
        assert!(panicked.is_err());
        assert!(list.is_poisoned());
        list.locked().push(3);
        assert_eq!(*list.locked(), vec![1, 2, 3]);
    }
}
