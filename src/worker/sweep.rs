use std::hash::Hash;
use std::sync::Arc;

use dashmap::DashMap;

/// Off-hot-path eviction over a `DashMap<K, Arc<T>>`. Drops entries whose
/// only strong reference is the map's own and that the caller-supplied
/// `idle` predicate accepts. Atomic per shard via `DashMap::retain` — never
/// drops an entry another task just cloned out of the map.
pub(crate) fn sweep_idle<K, T, F>(map: &DashMap<K, Arc<T>>, idle: F) -> usize
where
    K: Eq + Hash,
    F: Fn(&T) -> bool,
{
    let mut removed = 0usize;
    map.retain(|_, slot| {
        if Arc::strong_count(slot) != 1 {
            return true;
        }
        if idle(slot.as_ref()) {
            removed += 1;
            false
        } else {
            true
        }
    });
    removed
}
