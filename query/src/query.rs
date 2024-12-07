use std::{
    collections::HashMap,
    future::Future,
    sync::{Mutex, Arc},
    time::Duration,
};

use futures_channel::oneshot;
use leptos::{prelude::*, logging, task::spawn_local};

use crate::{
    garbage_collector::GarbageCollector,
    query_cache::CacheNotification,
    query_is_suppressed,
    query_observer::{ObserverKey, QueryObserver},
    use_query_client,
    util::time_until_stale,
    QueryData, QueryState,
};

#[derive(Clone)]
pub struct Query<K, V> {
    key: K,

    // Cancellation
    current_request: Arc<Mutex<Option<oneshot::Sender<()>>>>,

    // State
    state: Arc<Mutex<QueryState<V>>>,

    // Synchronization
    observers: Arc<Mutex<HashMap<ObserverKey, QueryObserver<K, V>>>>,
    garbage_collector: Arc<Mutex<Option<GarbageCollector<K, V>>>>,
}

impl<K: PartialEq, V> PartialEq for Query<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl<K: PartialEq, V> Eq for Query<K, V> {}

impl<K, V> std::fmt::Debug for Query<K, V>
where
    K: crate::QueryKey,
    V: crate::QueryValue,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Query")
            .field("key", &self.key)
            .field("state", &self.state)
            .field("observers", &self.observers.lock().unwrap().len())
            .field("gc", &self.garbage_collector)
            .finish()
    }
}

impl<K, V> Query<K, V>
where
    K: crate::QueryKey + 'static,
    V: crate::QueryValue + 'static,
{
    pub fn new(key: K) -> Self {
        let query = Query {
            key: key.clone(),
            current_request: Arc::new(Mutex::new(None)),
            observers: Arc::new(Mutex::new(HashMap::new())),
            state: Arc::new(Mutex::new(QueryState::Created)),
            garbage_collector: Arc::new(Mutex::new(None)),
        };

        let gc = GarbageCollector::new(query.clone());

        *query.garbage_collector.lock().unwrap() = Some(gc);

        query
    }

    pub fn set_state(&self, state: QueryState<V>) {
        // Notify observers.
        {
            let observers = self.observers.lock().expect("set state borrow");
            for observer in observers.values() {
                observer.notify(state.clone())
            }
        }

        let invalid = matches!(state, QueryState::Invalid(_));

        {
            *self.state.lock().unwrap() = state;
        }

        // Notify cache. This has to be at the end due to sending the entire query in the notif.
        use_query_client()
            .cache
            .notify(CacheNotification::UpdatedState(self.clone()));

        if invalid {
            self.execute();
        }
    }

    pub fn update_state(&self, update_fn: impl FnOnce(&mut QueryState<V>)) {
        let mut state = {
            let self_state = self.state.lock().unwrap();
            self_state.clone()
        };
        update_fn(&mut state);
        self.set_state(state);
    }

    /// Be careful with this function. Used to avoid cloning.
    /// If update returns Ok(_) the state will be updated and subscribers will be notified.
    /// If update returns Err(_) the state will not be updated and subscribers will not be notified.
    /// Err(_) should always contain the previous state.
    pub fn maybe_map_state(
        &self,
        update_fn: impl FnOnce(QueryState<V>) -> Result<QueryState<V>, QueryState<V>>,
    ) -> bool {
        let current_state = {
            let self_state = self.state.lock().unwrap();
            self_state.clone()
        };

        match update_fn(current_state) {
            Ok(new_state) => {
                self.set_state(new_state);
                true
            }
            Err(old_state) => {
                let mut self_state = self.state.lock().unwrap();
                *self_state = old_state;
                false
            }
        }
    }

    /// Marks the resource as invalid, which will cause it to be refetched on next read.
    pub fn mark_invalid(&self) -> bool {
        let mut updated = false;
        self.maybe_map_state(|state| {
            if let QueryState::Loaded(data) = state {
                updated = true;
                Ok(QueryState::Invalid(data))
            } else {
                Err(state)
            }
        });
        updated
    }

    pub fn subscribe(&self, observer: &QueryObserver<K, V>) {
        let observer_id = observer.get_id();
        let mut observers = self
            .observers
            .lock()
            .expect("subscribe borrow_mut");

        // Check if the observer is already subscribed to avoid duplicate subscriptions
        if let std::collections::hash_map::Entry::Vacant(e) = observers.entry(observer_id) {
            e.insert(observer.clone());
            self.disable_gc();
            self.update_gc_time(observer.get_options().gc_time);

            use_query_client()
                .cache
                .notify::<K, V>(CacheNotification::NewObserver(
                    crate::query_cache::NewObserver {
                        key: self.key.clone(),
                        options: observer.get_options().clone(),
                    },
                ));
        }
    }

    pub fn unsubscribe(&self, observer: &QueryObserver<K, V>) {
        let mut observers = self
            .observers
            .lock()
            .expect("unsubscribe borrow_mut");
        if observers.remove(&observer.get_id()).is_some() {
            use_query_client()
                .cache
                .notify::<K, V>(CacheNotification::ObserverRemoved(self.key.clone()))
        }

        if observers.is_empty() {
            drop(observers);
            self.enable_gc();
        }
    }

    pub fn update_gc_time(&self, gc_time: Option<Duration>) {
        self.garbage_collector
            .lock()
            .unwrap()
            .as_ref()
            .expect("update_gc_time borrow")
            .update_gc_time(gc_time);
    }

    pub fn enable_gc(&self) {
        self.garbage_collector
            .lock()
            .unwrap()
            .as_ref()
            .expect("enable_gc borrow")
            .enable_gc();
    }

    pub fn disable_gc(&self) {
        self.garbage_collector
            .lock()
            .unwrap()
            .as_ref()
            .expect("disable_gc borrow")
            .disable_gc();
    }

    pub fn get_state(&self) -> QueryState<V> {
        self.state.lock().unwrap().clone()
    }

    // Useful to avoid clones.
    pub fn with_state<T>(&self, func: impl FnOnce(&QueryState<V>) -> T) -> T {
        let state = self.state.lock().unwrap();
        func(&state)
    }

    /**
     * Execution and Cancellation.
     */

    pub fn execute(&self) {
        let observers = self.observers.lock().expect("execute borrow");
        let fetcher = observers.values().find_map(|f| f.get_fetcher());

        if let Some(fetcher) = fetcher {
            if !query_is_suppressed() {
                spawn_local(execute_query(self.clone(), move |k| fetcher(k)));
            }
        }
    }

    // Only scenario where two requests can exist at the same time is the first is cancelled.
    pub fn new_execution(&self) -> Option<oneshot::Receiver<()>> {
        let mut current_request = self.current_request.lock().unwrap();
        if current_request.is_none() {
            let (sender, receiver) = oneshot::channel();
            *current_request = Some(sender);
            Some(receiver)
        } else {
            None
        }
    }

    pub fn finalize_execution(&self) {
        *self.current_request.lock().unwrap() = None;
    }

    pub fn cancel(&self) -> bool {
        let mut current_request = self.current_request.lock().unwrap();
        if let Some(current_request) = current_request.take() {
            let cancellation = current_request.send(());
            if cancellation.is_err() {
                logging::error!("Failed to cancel request {:?}", self.key);
            }
            cancellation.is_ok()
        } else {
            false
        }
    }

    pub fn needs_execute(&self) -> bool {
        self.with_state(|s| matches!(s, QueryState::Created))
            || self.with_state(|s| matches!(s, QueryState::Invalid(_)))
            || self.is_stale()
    }

    pub fn ensure_execute(&self) {
        if self.needs_execute() {
            self.execute();
        }
    }

    pub fn is_stale(&self) -> bool {
        let stale_time = self
            .observers
            .lock()
            .unwrap()
            .iter()
            .flat_map(|(_, o)| o.get_options().stale_time)
            .min();
        let updated_at = self.with_state(|s| s.updated_at());

        match (updated_at, stale_time) {
            (Some(updated_at), Some(stale_time)) => {
                time_until_stale(updated_at, stale_time).is_zero()
            }
            _ => false,
        }
    }

    pub fn get_updated_at(&self) -> Option<crate::Instant> {
        self.with_state(|s| s.updated_at())
    }

    pub fn get_key(&self) -> &K {
        &self.key
    }

    pub fn get_gc(&self) -> Option<GarbageCollector<K, V>> {
        self.garbage_collector.lock().unwrap().clone()
    }
}

impl<K, V> Query<K, V>
where
    K: crate::QueryKey + 'static,
    V: crate::QueryValue + 'static,
{
    pub fn dispose(&self) {
        #[cfg(debug_assertions)]
        if !self.observers.lock().unwrap().is_empty() {
            logging::debug_warn!("Query has active observers");
        }
    }
}

pub async fn execute_query<K, V, Fu>(query: Query<K, V>, fetcher: impl Fn(K) -> Fu)
where
    K: crate::QueryKey + 'static,
    V: crate::QueryValue + 'static,
    Fu: Future<Output = V>,
{
    if !crate::query_is_suppressed() {
        match query.new_execution() {
            None => {}
            Some(cancellation) => {
                match query.get_state() {
                    // First load.
                    QueryState::Created => {
                        query.set_state(QueryState::Loading);
                        let fetch = std::pin::pin!(fetcher(query.key.clone()));
                        match execute_with_cancellation(fetch, cancellation).await {
                            Ok(data) => {
                                let data = QueryData::now(data);
                                query.set_state(QueryState::Loaded(data));
                            }
                            Err(_) => {
                                query.set_state(QueryState::Created);
                            }
                        }
                    }
                    // Subsequent loads.
                    QueryState::Loaded(data) | QueryState::Invalid(data) => {
                        query.set_state(QueryState::Fetching(data));
                        let fetch = std::pin::pin!(fetcher(query.key.clone()));
                        match execute_with_cancellation(fetch, cancellation).await {
                            Ok(data) => {
                                let data = QueryData::now(data);
                                query.set_state(QueryState::Loaded(data));
                            }
                            Err(_) => {
                                query.maybe_map_state(|state| {
                                    if let QueryState::Fetching(data) = state {
                                        Ok(QueryState::Loaded(data))
                                    } else {
                                        Err(state)
                                    }
                                });
                            }
                        }
                    }
                    QueryState::Loading | QueryState::Fetching(_) => {
                        logging::debug_warn!("Query is already loading, this is likely a bug.");
                        debug_assert!(false, "Query is already loading, this is likely a bug.");
                    }
                }
                query.finalize_execution();
            }
        }
    }
}

#[cfg(any(feature = "hydrate", feature = "csr"))]
async fn execute_with_cancellation<V, Fu>(
    fut: Fu,
    cancellation: oneshot::Receiver<()>,
) -> Result<V, ()>
where
    Fu: std::future::Future<Output = V> + Unpin,
{
    use futures::future::Either;

    let result = futures::future::select(fut, cancellation).await;

    match result {
        Either::Left((result, _)) => Ok(result),
        Either::Right((cancelled, _)) => {
            if let Err(_) = cancelled {
                logging::debug_warn!("Query cancellation was incorrectly dropped.");
            }

            Err(())
        }
    }
}

// No cancellation on server side.
#[cfg(not(any(feature = "hydrate", feature = "csr")))]
async fn execute_with_cancellation<V, Fu>(
    fut: Fu,
    cancellation: oneshot::Receiver<()>,
) -> Result<V, ()>
where
    Fu: std::future::Future<Output = V> + Unpin,
{
    #[allow(clippy::let_underscore_future)]
    let _ = cancellation;
    let result = fut.await;
    Ok(result)
}
