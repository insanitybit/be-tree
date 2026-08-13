//! Per-content-id request coalescing for batched object fetches.

use std::sync::{Arc, Mutex};

use crate::{BlockId, TreeError};

struct Flight<T: Clone> {
    result: Mutex<Option<Result<T, TreeError>>>,
    ready: event_listener::Event,
}

impl<T: Clone> Flight<T> {
    fn pending() -> Self {
        Self {
            result: Mutex::new(None),
            ready: event_listener::Event::new(),
        }
    }

    fn complete(&self, result: Result<T, TreeError>) {
        let mut slot = self.result.lock().expect("flight");
        if slot.is_none() {
            *slot = Some(result);
            drop(slot);
            self.ready.notify(usize::MAX);
        }
    }

    async fn wait(&self) -> Result<T, TreeError> {
        loop {
            // Register before inspecting the result, so completion between the two cannot be missed.
            let listener = self.ready.listen();
            if let Some(result) = self.result.lock().expect("flight").clone() {
                return result;
            }
            listener.await;
        }
    }
}

/// Flights claimed by one fetch wave.
///
/// The caller fetches only [`owned_ids`](Self::owned_ids), completes each one, and can await any
/// requested id without knowing whether another wave owned it. Dropping an incomplete claim wakes
/// waiters with a cancellation error and releases every owned id for retry.
pub(crate) struct Claim<T: Clone> {
    inflight: Arc<Inflight<T>>,
    owned: Vec<BlockId>,
    flights: foldhash::HashMap<BlockId, Arc<Flight<T>>>,
}

impl<T: Clone> Claim<T> {
    pub(crate) fn owned_ids(&self) -> impl Iterator<Item = BlockId> + '_ {
        self.owned.iter().copied()
    }

    pub(crate) fn complete(&self, id: BlockId, result: Result<T, TreeError>) {
        let flight = self.flights.get(&id).expect("completed id was claimed");
        self.inflight.complete(id, flight, result);
    }

    pub(crate) async fn wait(&self, id: BlockId) -> Result<T, TreeError> {
        self.flights
            .get(&id)
            .expect("waited id was claimed")
            .wait()
            .await
    }
}

impl<T: Clone> Drop for Claim<T> {
    fn drop(&mut self) {
        let error = TreeError::Store("in-flight fetch owner was cancelled".into());
        for id in &self.owned {
            let flight = self.flights.get(id).expect("owned id was claimed");
            // `Flight::complete` preserves an earlier result, so this is harmless for completed ids.
            self.inflight.complete(*id, flight, Err(error.clone()));
        }
    }
}

pub(crate) struct Inflight<T: Clone> {
    by_id: Mutex<foldhash::HashMap<BlockId, Arc<Flight<T>>>>,
}

impl<T: Clone> Default for Inflight<T> {
    fn default() -> Self {
        Self {
            by_id: Mutex::new(Default::default()),
        }
    }
}

impl<T: Clone> Inflight<T> {
    /// Atomically claim the ids not already owned by an overlapping request.
    pub(crate) fn claim(self: &Arc<Self>, ids: &[BlockId]) -> Claim<T> {
        let mut by_id = self.by_id.lock().expect("inflight");
        let mut owned = Vec::new();
        let mut flights =
            foldhash::HashMap::with_capacity_and_hasher(ids.len(), Default::default());
        for id in ids.iter().copied() {
            let flight = if let Some(flight) = by_id.get(&id) {
                flight.clone()
            } else {
                let flight = Arc::new(Flight::pending());
                by_id.insert(id, flight.clone());
                owned.push(id);
                flight
            };
            flights.insert(id, flight);
        }
        drop(by_id);
        Claim {
            inflight: self.clone(),
            owned,
            flights,
        }
    }

    fn complete(&self, id: BlockId, flight: &Arc<Flight<T>>, result: Result<T, TreeError>) {
        flight.complete(result);
        let mut by_id = self.by_id.lock().expect("inflight");
        if by_id
            .get(&id)
            .is_some_and(|current| Arc::ptr_eq(current, flight))
        {
            by_id.remove(&id);
        }
    }
}
