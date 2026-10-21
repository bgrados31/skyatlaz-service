//! Coalesces concurrent cache misses for the same player into one upstream call.
//!
//! Two requests for one uuid that both miss the cache would each spend an
//! upstream request on the same answer. The first one to arrive becomes the
//! leader and does the work; the requests that arrive while that call is in
//! progress wait for its result and are answered with it, success or failure.
//!
//! The table of flights is bounded and holds only weak references, so a flight
//! never outlives the requests that joined it: a leader that is cancelled (the
//! request timeout, a client that went away) drops its guard, the uuid is free
//! for the next caller to lead, and the entry nothing can join is pruned by the
//! next lookup.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use tokio::sync::Mutex as AsyncMutex;

/// One upstream call and the answer shared with the requests waiting for it.
struct Flight<T, E> {
    /// The result, written once by the leader. `None` means the leader is still
    /// working, or was cancelled before it could answer.
    outcome: AsyncMutex<Option<Arc<Result<T, E>>>>,
}

impl<T, E> Flight<T, E> {
    fn new() -> Self {
        Self {
            outcome: AsyncMutex::new(None),
        }
    }
}

/// The profile fetches in progress, keyed by player uuid.
pub struct SingleFlight<T, E> {
    flights: Mutex<HashMap<String, Weak<Flight<T, E>>>>,
    capacity: usize,
}

impl<T, E> SingleFlight<T, E> {
    /// Creates a table that holds at most `capacity` flights at once.
    ///
    /// A capacity of zero would never coalesce anything, which is a mistake and
    /// not a mode, so it is raised to one.
    pub fn new(capacity: usize) -> Self {
        Self {
            flights: Mutex::new(HashMap::new()),
            capacity: capacity.max(1),
        }
    }

    /// Runs `work` once for every concurrent caller that passes the same `key`.
    ///
    /// The first caller leads; the others wait for its result and share it, so a
    /// failure is not retried once per waiter either. A caller that arrives
    /// after the flight is over starts a new one.
    pub async fn run<F, Fut>(&self, key: &str, work: F) -> Arc<Result<T, E>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let flight = self.join(key);

        let mut outcome = flight.outcome.lock().await;
        if let Some(result) = outcome.as_ref() {
            return Arc::clone(result);
        }

        let result = Arc::new(work().await);
        *outcome = Some(Arc::clone(&result));
        // The table is released before the guard drops, so a caller that comes
        // after this flight cannot join it and wait for an answer that is
        // already there. There is no await between the store and the release, so
        // a cancellation cannot split the two.
        self.release(key, &flight);
        drop(outcome);
        result
    }

    /// Returns the flight for `key`, registering a new one when there is none in
    /// progress.
    ///
    /// The guard is dropped before returning, so it never crosses an await: the
    /// caller acquires the flight's async mutex afterwards.
    fn join(&self, key: &str) -> Arc<Flight<T, E>> {
        let mut flights = self.lock();

        // A flight that was cancelled or timed out has no request left to share
        // with, and a live one always has at least the Arc of the request using
        // it, so this drops exactly the entries nothing can join any more.
        flights.retain(|_, flight| flight.strong_count() > 0);

        if let Some(flight) = flights.get(key).and_then(Weak::upgrade) {
            return flight;
        }

        // A table full of live flights is at its bound. This caller runs without
        // a flight: the answer is still correct, only the duplicate upstream
        // call that single-flight exists to remove comes back.
        if flights.len() >= self.capacity {
            return Arc::new(Flight::new());
        }

        let flight = Arc::new(Flight::new());
        flights.insert(key.to_string(), Arc::downgrade(&flight));
        flight
    }

    /// Removes the entry of a finished flight, unless a newer flight for the
    /// same key already replaced it.
    fn release(&self, key: &str, flight: &Arc<Flight<T, E>>) {
        let mut flights = self.lock();
        let is_ours = flights
            .get(key)
            .is_some_and(|registered| Weak::ptr_eq(registered, &Arc::downgrade(flight)));
        if is_ours {
            flights.remove(key);
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Weak<Flight<T, E>>>> {
        self.flights
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// How many entries the table holds, live or dead, for the tests that check
    /// the bound and the cleanup.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::sync::{mpsc, Notify, Semaphore};

    /// How long the fake work of a flight takes. Long enough that the callers
    /// started together arrive while the leader is inside it, which is the only
    /// window in which single-flight does anything.
    const SLOW: Duration = Duration::from_millis(30);

    #[tokio::test]
    async fn fifty_concurrent_callers_share_one_call() {
        let flights = Arc::new(SingleFlight::<u32, &'static str>::new(64));
        let calls = Arc::new(AtomicUsize::new(0));

        let mut callers = Vec::new();
        for _ in 0..50 {
            let flights = Arc::clone(&flights);
            let calls = Arc::clone(&calls);
            callers.push(tokio::spawn(async move {
                flights
                    .run("uuid", || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(SLOW).await;
                        Ok(7u32)
                    })
                    .await
            }));
        }

        for caller in callers {
            let result = caller.await.expect("the caller finishes");
            assert_eq!(*result.as_ref(), Ok(7u32));
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the work ran once per caller"
        );
    }

    #[tokio::test]
    async fn a_failure_is_shared_instead_of_retried_by_every_waiter() {
        let flights = Arc::new(SingleFlight::<u32, &'static str>::new(64));
        let calls = Arc::new(AtomicUsize::new(0));

        let mut callers = Vec::new();
        for _ in 0..20 {
            let flights = Arc::clone(&flights);
            let calls = Arc::clone(&calls);
            callers.push(tokio::spawn(async move {
                flights
                    .run("uuid", || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(SLOW).await;
                        Err("the upstream failed")
                    })
                    .await
            }));
        }

        for caller in callers {
            let result = caller.await.expect("the caller finishes");
            assert_eq!(*result.as_ref(), Err("the upstream failed"));
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the failed call was retried per waiter"
        );
    }

    #[tokio::test]
    async fn a_cancelled_leader_does_not_wedge_the_key() {
        let flights = Arc::new(SingleFlight::<u32, &'static str>::new(8));
        let entered = Arc::new(Notify::new());

        let leader = tokio::spawn({
            let flights = Arc::clone(&flights);
            let entered = Arc::clone(&entered);
            async move {
                flights
                    .run("uuid", || async move {
                        entered.notify_one();
                        std::future::pending::<()>().await;
                        Ok(1u32)
                    })
                    .await
            }
        });

        entered.notified().await;
        leader.abort();
        assert!(leader.await.is_err(), "the leader did not stop");

        // The uuid is usable again: a later caller leads and answers instead of
        // waiting for a flight that will never finish.
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            flights.run("uuid", || async { Ok(9u32) }),
        )
        .await
        .expect("the cancelled flight left the uuid wedged");
        assert_eq!(*result.as_ref(), Ok(9u32));
        assert_eq!(flights.len(), 0, "the cancelled flight stayed in the table");
    }

    #[tokio::test]
    async fn a_cancelled_flight_does_not_accumulate_in_the_table() {
        let flights = Arc::new(SingleFlight::<u32, &'static str>::new(8));
        let entered = Arc::new(Notify::new());

        let leader = tokio::spawn({
            let flights = Arc::clone(&flights);
            let entered = Arc::clone(&entered);
            async move {
                flights
                    .run("uuid", || async move {
                        entered.notify_one();
                        std::future::pending::<()>().await;
                        Ok(1u32)
                    })
                    .await
            }
        });
        entered.notified().await;
        leader.abort();
        assert!(leader.await.is_err(), "the leader did not stop");

        // The lookup for another uuid drops the entry the cancelled flight left
        // behind: a dead entry must not sit in the table next to a live one.
        let held = Arc::new(Semaphore::new(0));
        let running_entered = Arc::new(Notify::new());
        let running = tokio::spawn({
            let flights = Arc::clone(&flights);
            let held = Arc::clone(&held);
            let running_entered = Arc::clone(&running_entered);
            async move {
                flights
                    .run("other", || async move {
                        running_entered.notify_one();
                        held.acquire().await.expect("the permit arrives").forget();
                        Ok(2u32)
                    })
                    .await
            }
        });
        running_entered.notified().await;

        assert_eq!(
            flights.len(),
            1,
            "the cancelled flight's entry stayed in the table"
        );

        held.add_permits(1);
        let result = running.await.expect("the caller finishes");
        assert_eq!(*result.as_ref(), Ok(2u32));
        assert_eq!(flights.len(), 0);
    }

    #[tokio::test]
    async fn the_table_is_bounded_and_forgets_finished_flights() {
        let flights = Arc::new(SingleFlight::<u32, &'static str>::new(2));
        let (entered, mut started) = mpsc::channel(4);
        let permits = Arc::new(Semaphore::new(0));
        let calls = Arc::new(AtomicUsize::new(0));

        let mut callers = Vec::new();
        for key in ["a", "b", "c"] {
            let flights = Arc::clone(&flights);
            let entered = entered.clone();
            let permits = Arc::clone(&permits);
            let calls = Arc::clone(&calls);
            callers.push(tokio::spawn(async move {
                flights
                    .run(key, || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        entered.send(key).await.expect("the channel is open");
                        permits
                            .acquire()
                            .await
                            .expect("the permit arrives")
                            .forget();
                        Ok(1u32)
                    })
                    .await
            }));
        }
        drop(entered);

        for _ in 0..3 {
            started.recv().await.expect("every caller started its call");
        }
        assert!(
            flights.len() <= 2,
            "the table grew past its capacity: {}",
            flights.len()
        );

        permits.add_permits(3);
        for caller in callers {
            caller.await.expect("the caller finishes");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3, "a uuid ran twice");
        assert_eq!(flights.len(), 0, "a finished flight stayed in the table");
    }
}
