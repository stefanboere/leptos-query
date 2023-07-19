use crate::instant::Instant;
use crate::query_result::QueryResult;
use crate::util::{time_until_stale, use_timeout};
use crate::{CacheEntry, QueryClient, QueryOptions, QueryState};
use leptos::*;
use std::any::{Any, TypeId};
use std::cell::{Cell, RefCell};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::rc::Rc;
use std::time::Duration;

/// Provides a Query Client to the current scope.
pub fn provide_query_client(cx: Scope) {
    provide_context(cx, QueryClient::new(cx));
}

/// Retrieves a Query Client from the current scope.
pub fn use_query_client(cx: Scope) -> QueryClient {
    use_context::<QueryClient>(cx).expect("Query Client Missing.")
}

/// Creates a query. Useful for data fetching, caching, and synchronization with server state.
///
/// A Query provides:
/// - caching
/// - de-duplication
/// - invalidation
/// - background refetching
/// - refetch intervals
/// - memory management with cache lifetimes
///
///
/// Details:
/// A query is unique per Key `K`.
/// A query Key type `K` must only correspond to ONE UNIQUE Value `V` Type.
/// Meaning a query Key type `K` cannot correspond to multiple Value `V` Types.
///
/// Example
/// ```
///
/// // Create a Newtype for MonkeyId.
/// #[derive(Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
/// struct MonkeyId(String);
///
///
/// // Monkey fetcher.
/// async fn get_monkey(id: MonkeyId) -> Monkey {
/// ...
/// }
///
/// // Query for a Monkey.
/// fn use_monkey_query(cx: Scope, id: MonkeyId) -> QueryResult<Monkey> {
///     leptos_query::use_query(
///         cx,
///         id,
///         |id| async move { get_monkey(id).await },
///         QueryOptions {
///             default_value: None,
///             refetch_interval: None,
///             resource_option: ResourceOption::NonBlocking,
///             stale_time: Some(Duration::from_secs(5)),
///             cache_time: Some(Duration::from_secs(30)),
///         },
///     )
/// }
///
/// #[component]
/// fn MonkeyView(cx: Scope, id: MonkeyId) -> impl IntoView {
///     let query = use_monkey_query(cx, id);
///     let QueryResult {
///         data,
///         is_loading,
///         is_refetching,
///         ..
///     } = query;
///
///     view! { cx,
///       // You can use the query result data here.
///       // Everything is reactive.
///     }
/// }
///
/// ```
///
pub fn use_query<K, V, Fu>(
    cx: Scope,
    key: K,
    query: impl Fn(K) -> Fu + 'static,
    options: QueryOptions<V>,
) -> QueryResult<V>
where
    Fu: Future<Output = V> + 'static,
    K: Hash + Eq + PartialEq + Clone + 'static,
    V: Clone + Serializable + 'static,
{
    let cache_time = options.cache_time.clone();
    let state = use_cache(cx, {
        let key = key.clone();
        move |(root_scope, cache)| {
            let entry = cache.entry(key.clone());

            let state = match entry {
                Entry::Occupied(entry) => {
                    let entry = entry.into_mut();
                    entry.set_options(cx, options);
                    entry
                }
                Entry::Vacant(entry) => {
                    let state = QueryState::new(root_scope, key.clone(), query, options);
                    entry.insert(state.clone())
                }
            };
            state.observers.set(state.observers.get() + 1);
            state.clone()
        }
    });

    // Keep track of the number of observers for this query.
    let observers = state.observers.clone();
    on_cleanup(cx, {
        let observers = observers.clone();
        move || {
            observers.set(observers.get() - 1);
        }
    });

    // Ensure that the Query is removed from cache up after the specified cache_time.
    let root_scope = use_query_client(cx).cx;
    cache_cleanup::<K, V>(
        root_scope,
        key,
        state.updated_at.into(),
        cache_time,
        observers,
    );

    QueryResult::from_state(cx, state)
}

// Will cleanup the cache corresponding to the key when the cache_time has elapsed, and the query has not been updated.
fn cache_cleanup<K, V>(
    cx: Scope,
    key: K,
    last_updated: Signal<Option<Instant>>,
    cache_time: Option<Duration>,
    observers: Rc<Cell<usize>>,
) where
    K: Hash + Eq + PartialEq + Clone + 'static,
    V: 'static,
{
    use_timeout(cx, move || match (last_updated.get(), cache_time) {
        (Some(last_updated), Some(cache_time)) => {
            let timeout = time_until_stale(last_updated, cache_time);
            let key = key.clone();
            let observers = observers.clone();
            set_timeout_with_handle(
                move || {
                    let removed =
                        use_cache::<K, V, Option<QueryState<K, V>>>(cx, move |(_, cache)| {
                            cache.remove(&key)
                        });
                    if let Some(query) = removed {
                        if observers.get() == 0 {
                            query.dispose();
                            drop(query)
                        }
                    };
                },
                timeout,
            )
            .ok()
        }
        _ => None,
    });
}

fn use_cache<K, V, R>(
    cx: Scope,
    func: impl FnOnce((Scope, &mut HashMap<K, QueryState<K, V>>)) -> R + 'static,
) -> R
where
    K: 'static,
    V: 'static,
{
    let client = use_query_client(cx);
    let mut cache = client.cache.borrow_mut();
    let entry = cache.entry(TypeId::of::<K>());

    let cache = entry.or_insert_with(|| {
        let wrapped: CacheEntry<K, V> = Rc::new(RefCell::new(HashMap::new()));
        let boxed = Box::new(wrapped) as Box<dyn Any>;
        boxed
    });

    let mut cache = cache
        .downcast_ref::<CacheEntry<K, V>>()
        .expect("Query Cache Type Mismatch.")
        .borrow_mut();

    func((client.cx, &mut cache))
}
