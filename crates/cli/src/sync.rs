//! Mutex helper shared by the daemon's threads.

use std::sync::{Mutex, MutexGuard};

/// Lock `m`. A panic on another thread poisons a mutex, but the daemon's
/// caches and registries stay usable, so the data is taken regardless.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn lock_survives_a_poisoned_mutex() {
        let m = Arc::new(Mutex::new(1));
        let m2 = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let _held = m2.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();
        assert!(m.is_poisoned());
        *lock(&m) += 1;
        assert_eq!(*lock(&m), 2);
    }
}
