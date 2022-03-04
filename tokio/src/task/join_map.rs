use crate::runtime::Handle;
use crate::task::{JoinError, JoinHandle, LocalSet};
use crate::util::idle_notified_set::{self, IdleNotifiedSet};
use hashbrown::{hash_map, HashMap};
use std::borrow::Borrow;
use std::collections::hash_map::RandomState;
use std::fmt;
use std::future::Future;
use std::hash::{BuildHasher, Hash, Hasher};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// A collection of tasks spawned on a Tokio runtime, associated with hash map
/// keys.
///
/// This type is very similar to the [`JoinSet`] type, with the addition of a
/// set of keys associated with each task. These keys allow [cancelling a
/// task][abort] or [multiple tasks][abort_matching] in the `JoinMap` based on
/// their keys, or [test whether a task corresponding to a given key exists][contains] in the `JoinMap`.
///
/// In addition, when tasks in the `JoinMap` complete, they will return the
/// associated key along with the value returned by the task, if any.
///
/// A `JoinMap` can be used to await the completion of some or all of the tasks
/// in the map. The map is not ordered, and the tasks will be returned in the
/// order they complete.
///
/// All of the tasks must have the same return type `V`.
///
/// When the `JoinMap` is dropped, all tasks in the `JoinMap` are immediately aborted.
///
/// **Note**: This is an [unstable API][unstable]. The public API of this type
/// may break in 1.x releases. See [the documentation on unstable
/// features][unstable] for details.
///
/// [`JoinSet`]: crate::task::JoinSet
/// [unstable]: crate#unstable-features
/// [abort]: fn@Self::abort
/// [abort_matching]: fn@Self::abort_matching
/// [contains]: fn@Self::contains_task
#[cfg_attr(docsrs, doc(cfg(all(feature = "rt", tokio_unstable))))]
pub struct JoinMap<K, V, S = RandomState> {
    aborts: HashMap<MapEntry<K, V>, (), S>,
    joins: IdleNotifiedSet<(u64, JoinHandle<V>)>,
}

impl<K, V> JoinMap<K, V> {
    /// Creates a new empty `JoinMap`.
    ///
    /// The `JoinMap` is initially created with a capacity of 0, so it will not
    /// allocate until a task is first spawned on it.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio_util::task::JoinMap;
    /// let mut map: JoinMap<&str, i32> = JoinMap::new();
    /// ```
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self::with_hasher(RandomState::new())
    }

    /// Creates an empty `JoinMap` with the specified capacity.
    ///
    /// The `JoinMap` will be able to hold at least `capacity` tasks without
    /// reallocating.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio_util::task::JoinMap;
    /// let mut map: JoinMap<&str, i32> = JoinMap::with_capacity(10);
    /// ```
    #[inline]
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        JoinMap::with_capacity_and_hasher(capacity, Default::default())
    }
}

impl<K, V, S> JoinMap<K, V, S> {
    /// Creates an empty `JoinMap` which will use the given hash builder to hash
    /// keys.
    ///
    /// The created map has the default initial capacity.
    ///
    /// Warning: `hash_builder` is normally randomly generated, and
    /// is designed to allow `JoinMap` to be resistant to attacks that
    /// cause many collisions and very poor performance. Setting it
    /// manually using this function can expose a DoS attack vector.
    ///
    /// The `hash_builder` passed should implement the [`BuildHasher`] trait for
    /// the `JoinMap` to be useful, see its documentation for details.
    #[inline]
    #[must_use]
    pub fn with_hasher(hash_builder: S) -> Self {
        Self::with_capacity_and_hasher(0, hash_builder)
    }

    /// Creates an empty `JoinMap` with the specified capacity, using `hash_builder`
    /// to hash the keys.
    ///
    /// The `JoinMap` will be able to hold at least `capacity` elements without
    /// reallocating. If `capacity` is 0, the `JoinMap` will not allocate.
    ///
    /// Warning: `hash_builder` is normally randomly generated, and
    /// is designed to allow HashMaps to be resistant to attacks that
    /// cause many collisions and very poor performance. Setting it
    /// manually using this function can expose a DoS attack vector.
    ///
    /// The `hash_builder` passed should implement the [`BuildHasher`] trait for
    /// the `JoinMap`to be useful, see its documentation for details.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio_util::task::JoinMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let s = RandomState::new();
    /// let mut map = JoinMap::with_capacity_and_hasher(10, s);
    /// map.spawn(1, async move { "hello world!" });
    /// ```
    #[inline]
    #[must_use]
    pub fn with_capacity_and_hasher(capacity: usize, hash_builder: S) -> Self {
        Self {
            aborts: HashMap::with_capacity_and_hasher(capacity, hash_builder),
            joins: IdleNotifiedSet::new(),
        }
    }

    /// Returns the number of tasks currently in the `JoinMap`.
    pub fn len(&self) -> usize {
        self.joins.len()
    }

    /// Returns whether the `JoinMap` is empty.
    pub fn is_empty(&self) -> bool {
        self.joins.is_empty()
    }

    /// Returns the number of tasks the map can hold without reallocating.
    ///
    /// This number is a lower bound; the `JoinMap` might be able to hold
    /// more, but is guaranteed to be able to hold at least this many.
    ///
    /// # Examples
    ///
    /// ```
    /// use crate::task::JoinMap;
    ///
    /// let map: JoinMap<i32, i32> = JoinMap::with_capacity(100);
    /// assert!(map.capacity() >= 100);
    /// ```
    #[inline]
    pub fn capacity(&self) -> usize {
        self.aborts.capacity()
    }
}

impl<K, V, S> JoinMap<K, V, S>
where
    K: Hash + Eq + 'static,
    V: 'static,
    S: BuildHasher,
{
    /// Spawn the provided task and store it in this `JoinMap` with the provided
    /// key.
    ///
    /// # Panics
    ///
    /// This method panics if called outside of a Tokio runtime.
    ///
    /// [`AbortHandle`]: crate::task::AbortHandle
    pub fn spawn<F>(&mut self, key: K, task: F)
    where
        F: Future<Output = V>,
        F: Send + 'static,
        V: Send,
    {
        self.insert(key, crate::spawn(task))
    }

    /// Spawn the provided task on the provided runtime and store it in this
    /// `JoinMap` with the provided key.
    ///
    /// If a task previously existed in the `JoinMap` for this key, that task
    /// will be cancelled and replaced with the new one.
    pub fn spawn_on<F>(&mut self, key: K, task: F, handle: &Handle)
    where
        F: Future<Output = V>,
        F: Send + 'static,
        V: Send,
    {
        self.insert(key, handle.spawn(task));
    }

    /// Spawn the provided task on the current [`LocalSet`] and store it in this
    /// `JoinMap` with the provided key.
    ///
    /// If a task previously existed in the `JoinMap` for this key, that task
    /// will be cancelled and replaced with the new one.
    ///
    /// # Panics
    ///
    /// This method panics if it is called outside of a `LocalSet`.
    ///
    /// [`LocalSet`]: crate::task::LocalSet
    pub fn spawn_local<F>(&mut self, key: K, task: F)
    where
        F: Future<Output = V>,
        F: 'static,
    {
        self.insert(key, crate::task::spawn_local(task));
    }

    /// Spawn the provided task on the provided [`LocalSet`] and store it in
    /// this `JoinMap` with the provided key.
    ///
    /// If a task previously existed in the `JoinMap` for this key, that task
    /// will be cancelled and replaced with the new one.
    ///
    /// [`LocalSet`]: crate::task::LocalSet
    pub fn spawn_local_on<F>(&mut self, key: K, task: F, local_set: &LocalSet)
    where
        F: Future<Output = V>,
        F: 'static,
    {
        self.insert(key, local_set.spawn_local(task))
    }

    fn insert(&mut self, key: K, jh: JoinHandle<V>) {
        let hash = Self::hash(&self.aborts, &key);

        let mut entry = self.joins.insert_idle((hash, jh));

        // Set the waker that is notified when the task completes.
        entry.with_value_and_context(|(_, jh), ctx| jh.set_join_waker(ctx.waker()));

        let map_entry = self
            .aborts
            .raw_entry_mut()
            .from_hash(hash, |entry| entry.key == key);

        let entry = MapEntry {
            entry: entry.entry(),
            key,
        };

        match map_entry {
            hash_map::RawEntryMut::Occupied(mut occ) => {
                let prev = occ.insert_key(entry);
                // Abort the previous entry.
                if let Some(mut prev_entry) = self.joins.entry_mut(prev.entry) {
                    prev_entry.with_value_and_context(|(_prev_hash, prev_task), _| {
                        debug_assert_eq!(*_prev_hash, hash);
                        prev_task.abort();
                    });
                    // XXX(eliza): we could also remove it from the
                    // `IdleNotifiedSet` here...?
                }
            }
            hash_map::RawEntryMut::Vacant(vac) => {
                vac.insert(entry, ());
            }
        }
    }

    /// Waits until one of the tasks in the map completes and returns its
    /// output, along with the key corresponding to that task.
    ///
    /// Returns `None` if the map is empty.
    ///
    /// # Cancel Safety
    ///
    /// This method is cancel safe. If `join_one` is used as the event in a `tokio::select!`
    /// statement and some other branch completes first, it is guaranteed that no tasks were
    /// removed from this `JoinMap`.
    ///
    /// # Returns
    ///
    /// This function returns:
    ///
    ///  * `Ok(Some((key, value)))` if one of the tasks in this `JoinMap` has
    ///    completed. The `value` is the return value of that ask, and `key` is
    ///    the key associated with the task.
    ///  * `Err(err)` if one of the tasks in this JoinMap` has panicked or been aborted.
    ///  * `Ok(None)` if the `JoinMap` is empty.
    pub async fn join_one(&mut self) -> Result<Option<(K, V)>, JoinError> {
        crate::future::poll_fn(|cx| self.poll_join_one(cx)).await
    }

    /// Aborts all tasks and waits for them to finish shutting down.
    ///
    /// Calling this method is equivalent to calling [`abort_all`] and then calling [`join_one`] in
    /// a loop until it returns `None`.
    ///
    /// This method ignores any panics in the tasks shutting down. When this call returns, the
    /// `JoinMap` will be empty.
    ///
    /// [`abort_all`]: fn@Self::abort_all
    /// [`join_one`]: fn@Self::join_one
    pub async fn shutdown(&mut self) {
        self.abort_all();
        while self.join_one().await.transpose().is_some() {}
    }

    /// Abort the task corresponding to the provided `key`.
    ///
    /// If this `JoinMap` contains a task corresponding to `key`, this method
    /// will abort that task and return `true`. Otherwise, if no task exists for
    /// `key`, this method returns `false`.
    pub fn abort<Q: ?Sized>(&mut self, key: &Q) -> bool
    where
        Q: Hash + Eq,
        K: Borrow<Q>,
    {
        let hash = Self::hash(&self.aborts, key);
        let entry = self
            .aborts
            .raw_entry_mut()
            .from_hash(hash, |entry| key == entry.key.borrow());
        let entry = match entry {
            hash_map::RawEntryMut::Occupied(entry) => entry.remove_entry().0,
            _ => return false,
        };

        debug_assert!(key == entry.key.borrow());
        if let Some(mut entry) = self.joins.entry_mut(entry.entry.clone()) {
            entry.with_value_and_context(|(_actual_hash, jh), _| {
                debug_assert_eq!(hash, *_actual_hash);
                jh.abort();
            });
            return true;
        }

        false
    }

    /// Aborts all tasks with keys matching `predicate`.
    ///
    /// `predicate` is a function called with a reference to each key in the
    /// map. If it returns `true` for a given key, the corresponding task will
    /// be cancelled.
    // XXX(eliza): do we want to consider counting the number of tasks aborted?
    pub fn abort_matching(&mut self, mut predicate: impl FnMut(&K) -> bool) {
        let aborts = &mut self.aborts;
        let joins = &mut self.joins;
        aborts.retain(|entry, _| {
            if predicate(&entry.key) {
                if let Some(mut entry) = joins.entry_mut(entry.entry.clone()) {
                    entry.with_value_and_context(|(_, jh), _| jh.abort());
                }
                false
            } else {
                true
            }
        })
    }

    /// Returns `true` if this `JoinMap` contains a task for the provided key.
    ///
    /// If the task has completed, but its output hasn't yet been consumed by a
    /// call to [`join_one`], this method will still return `true`.
    ///
    /// [`join_one`]: fn@Self::join_one
    pub fn contains_task<Q: ?Sized>(&mut self, key: &Q) -> bool
    where
        Q: Hash + Eq,
        K: Borrow<Q>,
    {
        Self::get(&self.aborts, key).is_some()
    }

    /// Reserves capacity for at least `additional` more tasks to be spawned
    /// on this `JoinMap` without reallocating for the map of task keys. The
    /// collection may reserve more space to avoid frequent reallocations.
    ///
    /// Note that spawning a task will still cause an allocation for the task
    /// itself.
    ///
    /// # Panics
    ///
    /// Panics if the new allocation size overflows [`usize`].
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio_util::task::JoinMap;
    ///
    /// let mut map: JoinMap<&str, i32> = JoinMap::new();
    /// map.reserve(10);
    /// ```
    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        self.aborts.reserve(additional)
    }

    /// Shrinks the capacity of the `JoinMap` as much as possible. It will drop
    /// down as much as possible while maintaining the internal rules
    /// and possibly leaving some space in accordance with the resize policy.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio_util::task::JoinMap;
    ///
    /// let mut map: JoinMap<i32, i32> = JoinMap::with_capacity(100);
    /// map.spawn(1, async move { 2 });
    /// map.spawn(3, async move { 4 });
    /// assert!(map.capacity() >= 100);
    /// map.shrink_to_fit();
    /// assert!(map.capacity() >= 2);
    /// ```
    #[inline]
    pub fn shrink_to_fit(&mut self) {
        self.aborts.shrink_to_fit();
    }

    /// Shrinks the capacity of the map with a lower limit. It will drop
    /// down no lower than the supplied limit while maintaining the internal rules
    /// and possibly leaving some space in accordance with the resize policy.
    ///
    /// If the current capacity is less than the lower limit, this is a no-op.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio_util::task::JoinMap;
    ///
    /// let mut map: JoinMap<i32, i32> = JoinMap::with_capacity(100);
    /// map.spawn(1, async move { 2 });
    /// map.spawn(3, async move { 4 });
    /// assert!(map.capacity() >= 100);
    /// map.shrink_to(10);
    /// assert!(map.capacity() >= 10);
    /// map.shrink_to(0);
    /// assert!(map.capacity() >= 2);
    /// ```
    #[inline]
    pub fn shrink_to(&mut self, min_capacity: usize) {
        self.aborts.shrink_to(min_capacity)
    }

    /// Polls for one of the tasks in the map to complete, returning the output
    /// and key of the completed task if one completed.
    ///
    /// If this returns `Poll::Ready(Some((key, _)))`  then the task with the
    /// key `key` completed, and has been removed from the map.
    ///
    /// When the method returns `Poll::Pending`, the `Waker` in the provided `Context` is scheduled
    /// to receive a wakeup when a task in the `JoinSet` completes. Note that on multiple calls to
    /// `poll_join_one`, only the `Waker` from the `Context` passed to the most recent call is
    /// scheduled to receive a wakeup.
    ///
    /// # Returns
    ///
    /// This function returns:
    ///
    ///  * `Poll::Pending` if the `JoinMap` is not empty but there is no task whose output is
    ///     available right now.
    ///  * `Poll::Ready(Some((key, Ok(value))))` if one of the tasks in this
    ///    `JoinMap` has completed. The `value` is the return value of that
    ///    task, and `key` is the key associated with the task.
    ///  * `Poll::Ready(Some((key, Err(err)))` if one of the tasks in this
    ///    `JoinMap` has panicked or been aborted. `key` is the key associated
    ///    with the task that panicked or was aborted.
    ///  * `Poll::Ready(None)` if the `JoinMap` is empty.
    ///
    /// Note that this method may return `Poll::Pending` even if one of the tasks has completed.
    /// This can happen if the [coop budget] is reached.
    ///
    /// [coop budget]: crate::task#cooperative-scheduling
    fn poll_join_one(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<(K, V)>, JoinError>> {
        // The call to `pop_notified` moves the entry to the `idle` list. It is moved back to
        // the `notified` list if the waker is notified in the `poll` call below.
        let mut entry = match self.joins.pop_notified(cx.waker()) {
            Some(entry) => entry,
            None => {
                if self.is_empty() {
                    return Poll::Ready(Ok(None));
                } else {
                    // The waker was set by `pop_notified`.
                    return Poll::Pending;
                }
            }
        };

        let res = entry.with_value_and_context(|(_, jh), ctx| Pin::new(jh).poll(ctx));

        if let Poll::Ready(res) = res {
            // Look up the entry in the key map by the hash stored in the
            // `IdleNotifiedSet` entry + the `Arc` pointer.
            let hash = entry.with_value_and_context(|(hash, _), _| *hash);
            let map_entry = self
                .aborts
                .raw_entry_mut()
                .from_hash(hash, |map_entry| entry.ptr_eq(&map_entry.entry));
            // Remove the entry from the `IdleNotifiedSet`.
            let _ = entry.remove();
            match (map_entry, res) {
                // Found the task in the key map! Remove it.
                (hash_map::RawEntryMut::Occupied(occ), res) => {
                    let (map_entry, _) = occ.remove_entry();
                    Poll::Ready(res.map(|val| Some((map_entry.key, val))))
                }
                // The task's key is no longer in the key map ---
                // presumably, it was already cancelled.
                (hash_map::RawEntryMut::Vacant(_), Err(e)) => {
                    debug_assert!(
                        e.is_cancelled(),
                        "if the task's key is no longer in the map, it must have been cancelled!"
                    );
                    Poll::Ready(Err(e))
                }
                (hash_map::RawEntryMut::Vacant(_), Ok(_)) => unreachable!(
                    "if the task's key is no longer in the map, it must have been cancelled!"
                ),
            }
        } else {
            // A JoinHandle generally won't emit a wakeup without being ready unless
            // the coop limit has been reached. We yield to the executor in this
            // case.
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    fn get<'a, Q: ?Sized>(
        map: &'a HashMap<MapEntry<K, V>, (), S>,
        key: &Q,
    ) -> Option<&'a MapEntry<K, V>>
    where
        Q: Hash + Eq,
        K: Borrow<Q>,
    {
        let hash = Self::hash(map, key);
        let (entry, _) = map
            .raw_entry()
            .from_hash(hash, |entry| key == entry.key.borrow())?;
        Some(entry)
    }

    fn hash<Q: ?Sized>(map: &HashMap<MapEntry<K, V>, (), S>, key: &Q) -> u64
    where
        Q: Hash,
    {
        let mut hasher = map.hasher().build_hasher();
        key.hash(&mut hasher);
        hasher.finish()
    }
}

impl<K, V, S> JoinMap<K, V, S>
where
    K: 'static,
    V: 'static,
{
    /// Aborts all tasks on this `JoinMap`.
    ///
    /// This does not remove the tasks from the `JoinMap`. To wait for the tasks to complete
    /// cancellation, you should call `join_one` in a loop until the `JoinMap` is empty.
    pub fn abort_all(&mut self) {
        self.joins.for_each(|(_, jh)| jh.abort());
    }

    /// Removes all tasks from this `JoinMap` without aborting them.
    ///
    /// The tasks removed by this call will continue to run in the background even if the `JoinMap`
    /// is dropped. They may still be aborted by key.
    pub fn detach_all(&mut self) {
        self.joins.drain(drop);
    }
}

impl<K, V, S> Drop for JoinMap<K, V, S> {
    fn drop(&mut self) {
        self.joins.drain(|(_, join_handle)| join_handle.abort());
    }
}

impl<K: fmt::Debug + 'static, V: 'static, S> fmt::Debug for JoinMap<K, V, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // debug print the keys in this `JoinMap`.
        struct KeySet<'a, K, V, S>(&'a JoinMap<K, V, S>);
        impl<K: fmt::Debug + 'static, V: 'static, S> fmt::Debug for KeySet<'_, K, V, S> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_set()
                    .entries(self.0.aborts.keys().map(|entry| &entry.key))
                    .finish()
            }
        }

        f.debug_struct("JoinMap")
            .field("keys", &KeySet(self))
            .finish()
    }
}

impl<K, V> Default for JoinMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

struct MapEntry<K, V> {
    key: K,
    entry: Arc<idle_notified_set::ListEntry<(u64, JoinHandle<V>)>>,
}

impl<K: Hash, V> Hash for MapEntry<K, V> {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        self.key.hash(hasher);
    }
}

impl<K: PartialEq, V> PartialEq for MapEntry<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl<K: Eq, V> Eq for MapEntry<K, V> {}
