use std::{cell::UnsafeCell, collections::HashMap, future::Future, hash::Hash, sync::Arc};

use parking_lot::{RwLock, RwLockUpgradableReadGuard};
use tokio::sync::{futures::Notified, Notify};

/// SingleFlight represents a class of work and creates a space in which units of work
/// can be executed with duplicate suppression.
#[derive(Debug)]
pub struct SingleFlight<K, T> {
    mapping: Arc<RwLock<HashMap<K, BroadcastOnce<T>>>>,
}

impl<K, T> Default for SingleFlight<K, T> {
    fn default() -> Self {
        Self {
            mapping: Default::default(),
        }
    }
}

struct Shared<T> {
    slot: UnsafeCell<Option<T>>,
    notify: Notify,
}

unsafe impl<T> Send for Shared<T> where T: Send {}
unsafe impl<T> Sync for Shared<T> where T: Sync {}

impl<T> Default for Shared<T> {
    fn default() -> Self {
        Self {
            slot: UnsafeCell::new(None),
            notify: Notify::new(),
        }
    }
}

// BroadcastOnce consists of shared slot and notify.
#[derive(Clone)]
struct BroadcastOnce<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Default for BroadcastOnce<T> {
    fn default() -> Self {
        Self {
            shared: Arc::new(Shared::default()),
        }
    }
}

// After calling BroadcastOnce::waiter we can get a waiter.
// It's in WaitList.
struct BroadcastOnceWaiter<T> {
    notified: Notified<'static>,
    shared: Arc<Shared<T>>,
}

impl<T> std::fmt::Debug for BroadcastOnce<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BroadcastOnce")
    }
}

impl<T> BroadcastOnce<T> {
    fn new() -> Self {
        Self::default()
    }

    fn waiter(&self) -> BroadcastOnceWaiter<T> {
        // Leak Notify to get a Notified<'static>.
        // It's safe since Notify is behind an Arc and we hold a reference.
        let notify = unsafe { &*(&self.shared.notify as *const Notify) };
        BroadcastOnceWaiter {
            notified: notify.notified(),
            shared: self.shared.clone(),
        }
    }

    // Safety: do not call wake multiple times
    unsafe fn wake(&self, value: T) {
        *self.shared.slot.get() = Some(value);
        self.shared.notify.notify_waiters();
    }
}

// We already in WaitList, so wait will be fine, we won't miss
// anything after Waiter generated.
impl<T> BroadcastOnceWaiter<T> {
    // Safety: first call wake, then call wait
    async unsafe fn wait(self) -> T
    where
        T: Clone,
    {
        self.notified.await;
        (*self.shared.slot.get())
            .clone()
            .expect("value not set unexpectedly")
    }
}

impl<K, T> SingleFlight<K, T> {
    /// Create a new BroadcastOnce to do work with.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }
}

impl<K, T> SingleFlight<K, T>
where
    K: Hash + Eq + Clone,
{
    /// Execute and return the value for a given function, making sure that only one
    /// operation is in-flight at a given moment. If a duplicate call comes in, that caller will
    /// wait until the original call completes and return the same value.
    #[allow(clippy::await_holding_lock)]
    pub fn work<F, Fut>(&self, key: K, func: F) -> impl Future<Output = T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
        T: Clone,
    {
        enum Either<L, R> {
            Left(L),
            Right(R),
        }

        // here the lock does not across await
        let m = self.mapping.upgradable_read();
        let val = m.get(&key);
        let either = match val {
            Some(call) => {
                let waiter = call.waiter();
                drop(m);
                Either::Left(waiter)
            }
            None => {
                let call = BroadcastOnce::new();
                {
                    let mut m = RwLockUpgradableReadGuard::upgrade(m);
                    m.insert(key.clone(), call.clone());
                }
                Either::Right((key, func(), self.mapping.clone(), call))
            }
        };
        async move {
            match either {
                Either::Left(waiter) => unsafe { waiter.wait().await },
                Either::Right((key, fut, mapping, call)) => {
                    let output = fut.await;
                    mapping.write().remove(&key);
                    unsafe { call.wake(output.clone()) };
                    output
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{
            AtomicUsize,
            Ordering::{AcqRel, Acquire},
        },
        time::Duration,
    };

    use futures_util::{stream::FuturesUnordered, StreamExt};

    use super::*;

    #[tokio::test]
    async fn direct_call() {
        let group = SingleFlight::new();
        let result = group
            .work("key", || async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                "Result".to_string()
            })
            .await;
        assert_eq!(result, "Result");
    }

    #[tokio::test]
    async fn parallel_call() {
        let call_counter = AtomicUsize::default();

        let group = SingleFlight::new();
        let futures = FuturesUnordered::new();
        for _ in 0..10 {
            futures.push(group.work("key", || async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                call_counter.fetch_add(1, AcqRel);
                "Result".to_string()
            }));
        }

        assert!(futures.all(|out| async move { out == "Result" }).await);
        assert_eq!(
            call_counter.load(Acquire),
            1,
            "future should only be executed once"
        );
    }

    #[tokio::test]
    async fn parallel_call_seq_await() {
        let call_counter = AtomicUsize::default();

        let group = SingleFlight::new();
        let mut futures = Vec::new();
        for _ in 0..10 {
            futures.push(group.work("key", || async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                call_counter.fetch_add(1, AcqRel);
                "Result".to_string()
            }));
        }

        for fut in futures.into_iter() {
            assert_eq!(fut.await, "Result");
        }
        assert_eq!(
            call_counter.load(Acquire),
            1,
            "future should only be executed once"
        );
    }

    #[tokio::test]
    async fn call_with_static_str_key() {
        let group = SingleFlight::new();
        let result = group
            .work("key", || async {
                tokio::time::sleep(Duration::from_millis(1)).await;
                "Result".to_string()
            })
            .await;
        assert_eq!(result, "Result");
    }

    #[tokio::test]
    async fn call_with_static_string_key() {
        let group = SingleFlight::new();
        let result = group
            .work("key".to_string(), || async {
                tokio::time::sleep(Duration::from_millis(1)).await;
                "Result".to_string()
            })
            .await;
        assert_eq!(result, "Result");
    }

    #[tokio::test]
    async fn late_wait() {
        let group = SingleFlight::new();
        let fut_early = group.work("key", || async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            "Result".to_string()
        });
        let fut_late = group.work("key", || async { panic!("unexpected") });
        assert_eq!(fut_early.await, "Result");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(fut_late.await, "Result");
    }
}
