use crate::query::Query;
use crate::query_observer::{ListenerKey, QueryObserver};
use crate::query_result::QueryResult;
use crate::{
    query_is_suppressed, use_query_client, QueryOptions, QueryState, RefetchFn, ResourceOption,
};
// TODO use leptos::leptos_dom::HydrationCtx;
use leptos::prelude::*;
use leptos::logging;
use std::future::Future;
use std::sync::{Arc, Mutex};
use serde::{Serialize, Deserialize};

/// Creates a query. Useful for data fetching, caching, and synchronization with server state.
///
/// A Query provides:
/// - Caching
/// - De-duplication
/// - Invalidation
/// - Background refetching
/// - Refetch intervals
/// - Memory management with cache lifetimes
///
///
/// Example
/// ```
/// use leptos::prelude::*;
/// use leptos_query::*;
/// use std::time::Duration;
/// use serde::*;
///
/// // Query key.
/// #[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
/// struct UserId(i32);
///
/// // Data type.
/// #[derive(Debug, Clone, Deserialize, Serialize)]
/// struct UserData {
///     name: String,
/// }
///
/// // Fetcher
/// async fn get_user(id: UserId) -> UserData {
///     todo!()
/// }
///
/// // Query for a User.
/// fn use_user_query(id: impl Fn() -> UserId + 'static) -> QueryResult<UserData, impl RefetchFn> {
///     leptos_query::use_query(
///         id,
///         get_user,
///         QueryOptions {
///             stale_time: Some(Duration::from_secs(5)),
///             gc_time: Some(Duration::from_secs(60)),
///             ..QueryOptions::default()
///         },
///     )
/// }
///
/// ```
///
pub fn use_query<K, V, Fu>(
    key: impl Fn() -> K + Send + Sync + 'static,
    fetcher: impl Fn(K) -> Fu + Send + Sync + 'static,
    options: QueryOptions,
) -> QueryResult<V, impl RefetchFn>
where
    K: crate::QueryKey + 'static,
    V: crate::QueryValue + 'static,
    Fu: Future<Output = V> + Send + 'static,
{
    let options = options.validate();
    // Find relevant state.
    let query = use_query_client().cache.get_query_signal(key);

    let query_state = register_observer_handle_cleanup(fetcher, query, options.clone());

    let resource_fetcher = move |query: Query<K, V>| {
        async move {
            match query.get_state() {
                // Immediately provide cached value.
                QueryState::Loaded(data)
                | QueryState::Invalid(data)
                | QueryState::Fetching(data) => ResourceData(Some(data.data)),

                // Suspend indefinitely and wait for interruption.
                QueryState::Created | QueryState::Loading => {
                    let future = futures::future::pending();
                    let () = future.await;
                    ResourceData(None)
                }
            }
        }
    };

    let resource: Resource<ResourceData<V>> = {
        match options.resource_option.unwrap_or_default() {
            ResourceOption::NonBlocking => Resource::new(
                move || query.get(),
                resource_fetcher,
            ),
            ResourceOption::Blocking => {
                Resource::new_blocking(move || query.get(), resource_fetcher)
            }
            ResourceOption::Local => todo!() /* TODO, local resource has a different type now */
        }
    };

    // Ensure latest data in resource.
    Effect::new_isomorphic(move |_| {
        query_state.track();
        // If query is supressed, we have to make sure we don't refetch to avoid calling spawn_local.
        if !query_is_suppressed() {
            resource.refetch();
        }
    });

    // First read.
    {
        let query = query.get_untracked();

        if // TODO resource.loading().get_untracked() && !HydrationCtx::is_hydrating() &&
             query.with_state(|state| matches!(state, QueryState::Created))
        {
            query.execute()
        }
    }

    let data = Signal::derive({
        move || {
            let read = resource.get().and_then(|r| r.0);
            let _ = read;

            // SSR edge case.
            // Given hydrate can happen before resource resolves, signals on the client can be out of sync with resource.
            // Need to force insert the resource data into the query state.
            #[cfg(feature = "hydrate")]
            if let Some(ref data) = read {
                let query = query.get_untracked();
                if query.with_state(|state| matches!(state, QueryState::Created)) {
                    let data = crate::QueryData::now(data.clone());
                    query.set_state(QueryState::Loaded(data));
                }
            }
            read
        }
    });

    QueryResult {
        data,
        state: query_state,
        is_loading: Signal::derive(move || {
            query_state.with(|state| matches!(state, QueryState::Loading))
        }),
        is_fetching: Signal::derive(move || {
            query_state.with(|state| matches!(state, QueryState::Loading | QueryState::Fetching(_)))
        }),
        is_invalid: Signal::derive(move || {
            query_state.with(|state| matches!(state, QueryState::Invalid(_)))
        }),
        refetch: move || query.with_untracked(|q| q.execute()),
    }
}

/// Wrapper type to enable using `Serializable`
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResourceData<V>(Option<V>);

pub(crate) fn register_observer_handle_cleanup<K, V, Fu>(
    fetcher: impl Fn(K) -> Fu + Send + Sync + 'static,
    query: Memo<Query<K, V>>,
    options: QueryOptions,
) -> Signal<QueryState<V>>
where
    K: crate::QueryKey + Send + Sync + 'static,
    V: crate::QueryValue + Send + Sync + 'static,
    Fu: Future<Output = V> + Send + 'static,
{
    let state_signal = RwSignal::new(query.get_untracked().get_state());
    let observer = Arc::new(QueryObserver::with_fetcher(
        fetcher,
        options,
        query.get_untracked(),
    ));
    let listener = Arc::new(Mutex::new(None::<ListenerKey>));

    Effect::new_isomorphic({
        let observer = observer.clone();
        let listener = listener.clone();
        move |_| {
            // Ensure listener is set
            {
                let mut listener = listener.lock().unwrap();
                if listener.is_none() {
                    let listener_id = observer.add_listener(move |state| {
                        state_signal.set(state.clone());
                    });
                    *listener = Some(listener_id);
                }
            }

            // Update
            let query = query.get();
            state_signal.set(query.get_state());
            observer.update_query(Some(query));
        }
    });

    on_cleanup(move || {
        {
            let mut listener = listener.lock().unwrap();
            if let Some(listener_id) = listener.take() {
                if !observer.remove_listener(listener_id) {
                    logging::debug_warn!("Failed to remove listener.");
                }
            }
        }
        observer.cleanup()
    });

    state_signal.into()
}
