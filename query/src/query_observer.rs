use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use leptos::leptos_dom::helpers::IntervalHandle;
use slotmap::{new_key_type, SlotMap};

use crate::query::Query;
use crate::{QueryKey, QueryOptions, QueryState, QueryValue};

#[derive(Clone)]
pub struct QueryObserver<K, V> {
    id: ObserverKey,
    query: Arc<Mutex<Option<Query<K, V>>>>,
    fetcher: Option<Fetcher<K, V>>,
    refetch: Arc<Mutex<Option<IntervalHandle>>>,
    options: QueryOptions,
    #[allow(clippy::type_complexity)]
    listeners: Arc<Mutex<SlotMap<ListenerKey, Box<dyn Fn(&QueryState<V>) + Send>>>>,
}

type Fetcher<K, V> = Arc<dyn Fn(K) -> Pin<Box<dyn Future<Output = V> + Send>> + Send + Sync>;

new_key_type! {
    pub struct ListenerKey;
}

impl<K, V> std::fmt::Debug for QueryObserver<K, V>
where
    K: QueryKey + 'static,
    V: QueryValue + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryObserver")
            .field("id", &self.id)
            .field("query", &self.query)
            .field("fetcher", &self.fetcher.is_some())
            .field("refetch", &self.refetch.lock().unwrap().is_some())
            .field("options", &self.options)
            .field("listeners", &self.listeners.lock().unwrap().len())
            .finish()
    }
}

impl<K, V> QueryObserver<K, V>
where
    K: QueryKey + 'static,
    V: QueryValue + 'static,
{
    pub fn with_fetcher<F, Fu>(fetcher: F, options: QueryOptions, query: Query<K, V>) -> Self
    where
        F: Fn(K) -> Fu + Send + Sync + 'static,
        Fu: Future<Output = V> + Send + 'static,
    {
        let fetcher =
            Some(
                Arc::new(move |s| Box::pin(fetcher(s)) as Pin<Box<dyn Future<Output = V> + Send>>)
                    as Fetcher<K, V>,
            );
        let query = Arc::new(Mutex::new(Some(query)));
        let id = next_id();

        #[cfg(any(feature = "csr", feature = "hydrate"))]
        let refetch = {
            use leptos::logging;

            let interval = {
                if let Some(refetch_interval) = options.refetch_interval {
                    let query = query.clone();
                    let timeout = leptos::leptos_dom::helpers::set_interval_with_handle(
                        move || {
                            if let Ok(query) = query.try_lock() {
                                if let Some(query) = query.as_ref() {
                                    query.execute()
                                }
                            } else {
                                logging::debug_warn!("QueryObserver: Query is already borrowed");
                            }
                        },
                        refetch_interval,
                    )
                    .ok();
                    if timeout.is_none() {
                        logging::debug_warn!("QueryObserver: Failed to set refetch interval");
                    }
                    timeout
                } else {
                    None
                }
            };
            Arc::new(Mutex::new(interval))
        };
        #[cfg(not(any(feature = "csr", feature = "hydrate")))]
        let refetch = Arc::new(Mutex::new(None));

        let observer = Self {
            id,
            query: query.clone(),
            fetcher,
            refetch,
            options,
            listeners: Arc::new(Mutex::new(SlotMap::with_key())),
        };

        {
            if let Some(query) = query.lock().unwrap().as_ref() {
                query.subscribe(&observer);
                if query.is_stale() {
                    query.execute()
                }
            }
        }

        observer
    }

    pub fn no_fetcher(options: QueryOptions, query: Option<Query<K, V>>) -> Self {
        let query = Arc::new(Mutex::new(query));
        let id = next_id();

        let observer = Self {
            id,
            query: query.clone(),
            fetcher: None,
            refetch: Arc::new(Mutex::new(None)),
            options,
            listeners: Arc::new(Mutex::new(SlotMap::with_key())),
        };

        {
            let query = query.lock().unwrap();
            if let Some(query) = query.as_ref() {
                query.subscribe(&observer);
            }
        }

        observer
    }

    pub fn get_fetcher(&self) -> Option<Fetcher<K, V>> {
        self.fetcher.clone()
    }

    pub fn get_id(&self) -> ObserverKey {
        self.id
    }

    pub fn get_options(&self) -> &QueryOptions {
        &self.options
    }

    pub fn notify(&self, state: QueryState<V>) {
        let listeners = self.listeners.lock().unwrap();
        for listener in listeners.values() {
            listener(&state);
        }
    }

    pub fn add_listener(&self, listener: impl Fn(&QueryState<V>) + Send + 'static) -> ListenerKey {
        let listener = Box::new(listener);
        let key = self
            .listeners
            .lock()
            .unwrap()
            .insert(listener);
        key
    }

    pub fn remove_listener(&self, key: ListenerKey) -> bool {
        self.listeners
            .lock()
            .unwrap()
            .remove(key)
            .is_some()
    }

    pub fn update_query(&self, new_query: Option<Query<K, V>>) {
        let mut query = self.query.lock().unwrap();
        // Determine if the new query is the same as the current one.
        let is_same_query = query.as_ref().map_or(false, |current_query| {
            new_query.as_ref().map_or(false, |new_query| {
                new_query.get_key() == current_query.get_key()
            })
        });

        // If the new query is the same as the current, do nothing.
        if is_same_query {
            return;
        }

        // If there's an existing query, unsubscribe from it.
        if let Some(current_query) = query.take() {
            current_query.unsubscribe(self);
        }

        // Set the new query (if any) and subscribe to it.
        *query = new_query.clone(); // Use clone to keep ownership with the caller.

        if let Some(ref query) = new_query {
            // Subscribe to the new query and ensure it's executed.
            query.subscribe(self);
            query.ensure_execute();
        }
    }

    pub fn cleanup(&self) {
        {
            let mut query = self.query.lock().unwrap();
            if let Some(query) = query.take() {
                query.unsubscribe(self);
            }
        }

        {
            let mut refetch = self.refetch.lock().unwrap();
            if let Some(interval) = refetch.take() {
                interval.clear();
            }
        }

        if !self
            .listeners
            .lock()
            .unwrap()
            .is_empty()
        {
            leptos::logging::debug_warn!(
                "QueryObserver::cleanup: QueryObserver::listeners is not empty"
            );
        }
    }
}

thread_local! {
    static NEXT_ID: Cell<u32> = const { Cell::new(1) } ;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObserverKey(u32);

fn next_id() -> ObserverKey {
    NEXT_ID.with(|id| {
        let current_id = id.get();
        id.set(current_id + 1);
        ObserverKey(current_id)
    })
}
