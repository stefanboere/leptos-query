use std::{
    any::{Any, TypeId},
    collections::{hash_map::Entry, HashMap},
    sync::{Arc, Mutex}
};

use leptos::prelude::*;
use slotmap::SlotMap;

use crate::{
    cache_observer::{CacheEvent, CacheObserver},
    query::Query,
    query_persister::QueryPersister,
    QueryKey, QueryOptions, QueryValue,
};

#[derive(Clone)]
pub struct QueryCache {
    owner: Owner,
    #[allow(clippy::type_complexity)]
    cache: Arc<Mutex<HashMap<(TypeId, TypeId), Box<dyn CacheEntryTrait + Send>>>>,
    #[allow(clippy::type_complexity)]
    observers: Arc<Mutex<SlotMap<CacheObserverKey, Box<dyn CacheObserver + Send>>>>,
    persister: Arc<Mutex<Option<Arc<dyn QueryPersister + Send + Sync>>>>,
    size: RwSignal<usize>,
}

slotmap::new_key_type! {
    pub struct CacheObserverKey;
}

struct CacheEntry<K, V>(HashMap<K, Query<K, V>>);

// Trait to enable cache introspection among distinct cache entry maps.
trait CacheEntryTrait: CacheSize + CacheInvalidate + CacheClear + CacheUpdateObserver {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<K, V> CacheEntryTrait for CacheEntry<K, V>
where
    K: crate::QueryKey + 'static,
    V: crate::QueryValue + 'static,
{
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

trait CacheSize {
    #[allow(dead_code)]
    fn size(&self) -> usize;
}

impl<K, V> CacheSize for CacheEntry<K, V> {
    fn size(&self) -> usize {
        self.0.len()
    }
}

trait CacheInvalidate {
    fn invalidate(&self);
}

impl<K, V> CacheInvalidate for CacheEntry<K, V>
where
    K: QueryKey + 'static,
    V: QueryValue + 'static,
{
    fn invalidate(&self) {
        for (_, query) in self.0.iter() {
            query.mark_invalid();
        }
    }
}

trait CacheClear {
    fn clear(&mut self, cache: &QueryCache);
}

impl<K, V> CacheClear for CacheEntry<K, V>
where
    K: QueryKey + 'static,
    V: QueryValue + 'static,
{
    fn clear(&mut self, cache: &QueryCache) {
        for (_, query) in self.0.drain() {
            query.dispose();
            cache.notify_query_eviction(query.get_key());
        }
    }
}

// Update an observer with all existing cache entries, upon subscription.
trait CacheUpdateObserver {
    fn update_observer(&self, observer: &dyn CacheObserver);
}

impl<K, V> CacheUpdateObserver for CacheEntry<K, V>
where
    K: QueryKey + 'static,
    V: QueryValue + 'static,
{
    fn update_observer(&self, observer: &dyn CacheObserver) {
        for (_, query) in self.0.iter() {
            let event = CacheEvent::created(query.clone());
            observer.process_cache_event(event);
        }
    }
}

impl QueryCache {
    pub fn new(owner: Owner) -> Self {
        Self {
            owner,
            cache: Arc::new(Mutex::new(HashMap::new())),
            observers: Arc::new(Mutex::new(SlotMap::with_key())),
            size: RwSignal::new(0),
            persister: Arc::new(Mutex::new(None)),
        }
    }

    pub fn get_or_create_query<K, V>(&self, key: K) -> Query<K, V>
    where
        K: QueryKey + 'static,
        V: QueryValue + 'static,
    {
        let query_cache = self;

        let mut created = false;

        let query = self.use_cache(|cache| {
            let entry = cache.entry(key.clone());

            let query = match entry {
                Entry::Occupied(entry) => {
                    let entry = entry.into_mut();
                    entry
                }
                Entry::Vacant(entry) => {
                    let query = query_cache.owner.with(|| Query::new(key));
                    query_cache.notify_new_query(query.clone());
                    created = true;
                    entry.insert(query)
                }
            };
            query.clone()
        });

        #[cfg(any(feature = "hydrate", feature = "csr"))]
        if created {
            let persister = {
                let p = self.persister.lock().unwrap();
                p.clone()
            };
            if let Some(persister) = persister {
                let query = query.clone();
                leptos::task::spawn_local({
                    async move {
                        let key = crate::cache_observer::make_cache_key(query.get_key());
                        let result = persister.retrieve(key.as_str()).await;

                        // ensure query is not already loaded.
                        if query.with_state(|s| matches!(s, crate::QueryState::Loaded(_))) {
                            return;
                        }

                        if let Some(serialized) = result {
                            match serialized.try_into() {
                                Ok(data) => {
                                    // If the query is currently fetching, then we should preserve the fetching state.
                                    if query.with_state(|s| {
                                        matches!(
                                            s,
                                            crate::QueryState::Loading
                                                | crate::QueryState::Fetching(_)
                                        )
                                    }) {
                                        query.set_state(crate::QueryState::Fetching(data));
                                    } else {
                                        query.set_state(crate::QueryState::Loaded(data));
                                    }
                                }
                                Err(e) => {
                                    leptos::logging::debug_warn!(
                                        "Error deserializing query state: {:?}",
                                        e
                                    );
                                }
                            }
                        }
                    }
                });
            }
        }

        // It's necessary to delay the size update until we are out of the borrow, to avoid borrow errors.
        if created {
            self.size.update(|size| *size += 1);
        }

        query
    }

    pub fn get_query<K, V>(&self, key: &K) -> Option<Query<K, V>>
    where
        K: QueryKey + 'static,
        V: QueryValue + 'static,
    {
        self.use_cache_option(move |cache| cache.get(key).cloned())
    }

    pub fn get_query_signal<K, V>(&self, key: impl Fn() -> K + Send + Sync + 'static) -> Memo<Query<K, V>>
    where
        K: QueryKey + 'static,
        V: QueryValue + 'static,
    {
        let client = self.clone();

        // This memo is crucial to avoid crazy amounts of lookups.
        Memo::new(move |_| {
            let key = key();
            client.get_or_create_query(key)
        })
    }

    pub fn size(&self) -> Signal<usize> {
        cfg_if::cfg_if! {
            if #[cfg(debug_assertions)] {
                let size_signal = self.size;
                let cache = self.cache.clone();
                Memo::new(move |_| {
                    let size = size_signal.get();
                    let real_size: usize = {
                        let cache = cache.lock().unwrap();
                        cache.values().map(|b| b.size()).sum()
                    };
                    assert!(size == real_size, "Cache size mismatch");
                    size
                }).into()
            } else {
                self.size.into()
            }
        }
    }

    pub fn evict_query<K, V>(&self, key: &K) -> bool
    where
        K: QueryKey + 'static,
        V: QueryValue + 'static,
    {
        let result = self.use_cache_option_mut::<K, V, _, _>(move |cache| cache.remove(key));

        if let Some(query) = result {
            self.notify_query_eviction(query.get_key());
            // With cache clears, the size may already be zero.
            self.size.update(|size| {
                if *size > 0 {
                    *size -= 1
                }
            });
            query.dispose();
            true
        } else {
            false
        }
    }

    pub fn invalidate_all_queries(&self) {
        for cache in self.cache.lock().unwrap()
            .values()
        {
            cache.invalidate();
        }
    }

    pub fn clear_all_queries(&self) {
        {
            let mut caches = self.cache.lock().unwrap();

            for cache in caches.values_mut() {
                cache.clear(self);
            }
        }
        // Though persister receives removal events, there may be queries in persister that are not yet in cache.
        // So we should clear them all.
        #[cfg(any(feature = "hydrate", feature = "csr"))]
        {
            let persister = {
                let persister = self.persister.lock().unwrap();
                persister.clone()
            };
            if let Some(persister) = persister {
                leptos::task::spawn_local(async move {
                    persister.clear().await;
                });
            }
        }

        // Need to queue microtask to avoid borrow errors.
        let size = self.size;
        queue_microtask(move || {
            size.set(0);
        })
    }

    pub fn use_cache_option<K, V, F, R>(&self, func: F) -> Option<R>
    where
        K: QueryKey + 'static,
        V: QueryValue + 'static,
        F: FnOnce(&HashMap<K, Query<K, V>>) -> Option<R>,
        R: 'static,
    {
        let cache = self.cache.lock().unwrap();
        let type_key = (TypeId::of::<K>(), TypeId::of::<V>());
        let cache = cache.get(&type_key)?;
        let cache = cache
            .as_any()
            .downcast_ref::<CacheEntry<K, V>>()
            .expect(EXPECT_CACHE_ERROR);
        func(&cache.0)
    }

    pub fn use_cache_option_mut<K, V, F, R>(&self, func: F) -> Option<R>
    where
        K: QueryKey + 'static,
        V: QueryValue + 'static,
        F: FnOnce(&mut HashMap<K, Query<K, V>>) -> Option<R>,
        R: 'static,
    {
        let mut cache = self.cache.lock().unwrap();
        let type_key = (TypeId::of::<K>(), TypeId::of::<V>());
        let cache = cache.get_mut(&type_key)?;
        let cache = cache
            .as_any_mut()
            .downcast_mut::<CacheEntry<K, V>>()
            .expect(EXPECT_CACHE_ERROR);
        func(&mut cache.0)
    }

    pub fn use_cache<K, V, R>(&self, func: impl FnOnce(&mut HashMap<K, Query<K, V>>) -> R) -> R
    where
        K: QueryKey + 'static,
        V: QueryValue + 'static,
    {
        let mut cache = self.cache.lock().unwrap();

        let type_key = (TypeId::of::<K>(), TypeId::of::<V>());

        let cache: &mut Box<dyn CacheEntryTrait + Send> = match cache.entry(type_key) {
            Entry::Occupied(o) => o.into_mut(),
            Entry::Vacant(v) => {
                let wrapped: CacheEntry<K, V> = CacheEntry(HashMap::new());
                v.insert(Box::new(wrapped))
            }
        };

        let cache: &mut CacheEntry<K, V> = cache
            .as_any_mut()
            .downcast_mut::<CacheEntry<K, V>>()
            .expect(EXPECT_CACHE_ERROR);

        func(&mut cache.0)
    }

    pub fn use_cache_entry<K, V>(
        &self,
        key: K,
        func: impl FnOnce((Owner, Option<&Query<K, V>>)) -> Option<Query<K, V>>,
    ) where
        K: QueryKey + 'static,
        V: QueryValue + 'static,
    {
        let query_cache = self;

        let mut created = false;

        self.use_cache(|cache| match cache.entry(key) {
            Entry::Vacant(entry) => {
                if let Some(query) = func((query_cache.owner.clone(), None)) {
                    entry.insert(query.clone());
                    // Report insert.
                    created = true;
                    self.notify_new_query(query)
                }
            }
            Entry::Occupied(mut entry) => {
                let query = entry.get();
                if let Some(query) = func((query_cache.owner.clone(), Some(query))) {
                    entry.insert(query);
                }
            }
        });

        // It's necessary to delay the size update until we are out of the borrow, to avoid borrow errors.
        if created {
            self.size.update(|size| *size += 1);
        }
    }

    pub fn register_observer(&self, observer: impl CacheObserver + Send + 'static) -> CacheObserverKey {
        // Update all existing cache entries with the new observer.
        {
            self.cache.lock().unwrap().values().for_each(|cache| {
                cache.update_observer(&observer);
            });
        }

        self.observers
            .lock()
            .unwrap()
            .insert(Box::new(observer))
    }

    pub fn unregister_observer(&self, key: CacheObserverKey) -> Option<Box<dyn CacheObserver + Send>> {
        self.observers
            .lock()
            .unwrap()
            .remove(key)
    }

    pub fn add_persister(&self, persister: impl QueryPersister + Send + Sync + 'static) {
        let persister = Arc::new(persister) as Arc<dyn QueryPersister + Send + Sync>;
        *self.persister.lock().unwrap() = Some(persister);
    }

    pub fn remove_persister(&self) -> Option<Arc<dyn QueryPersister + Send + Sync>> {
        self.persister.lock().unwrap().take()
    }

    pub fn notify<K, V>(&self, notification: CacheNotification<K, V>)
    where
        K: QueryKey + 'static,
        V: QueryValue + 'static,
    {
        let event = match notification {
            CacheNotification::UpdatedState(query) => CacheEvent::updated(query),
            CacheNotification::NewObserver(observer) => {
                CacheEvent::observer_added(&observer.key, observer.options)
            }
            CacheNotification::ObserverRemoved(key) => CacheEvent::observer_removed(&key),
        };
        self.notify_observers(event);
    }

    pub fn notify_new_query<K, V>(&self, query: Query<K, V>)
    where
        K: QueryKey + 'static,
        V: QueryValue + 'static,
    {
        let event = CacheEvent::created(query);
        self.notify_observers(event);
    }

    pub fn notify_query_eviction<K>(&self, key: &K)
    where
        K: QueryKey + 'static,
    {
        let event = CacheEvent::removed(key);
        self.notify_observers(event);
    }

    pub fn notify_observers(&self, notification: CacheEvent) {
        let observers = self
            .observers
            .lock()
            .unwrap();
        for observer in observers.values() {
            observer.process_cache_event(notification.clone())
        }
    }
}

pub enum CacheNotification<K, V> {
    UpdatedState(Query<K, V>),
    NewObserver(NewObserver<K>),
    ObserverRemoved(K),
}

pub struct NewObserver<K> {
    pub key: K,
    pub options: QueryOptions,
}

const EXPECT_CACHE_ERROR: &str =
    "Error: Query Cache Type Mismatch. This should not happen. Please file a bug report.";
