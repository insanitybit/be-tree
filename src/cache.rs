//! The tree's small cache contract, independent of the native or Miri backend.

use crate::BlockId;

#[cfg(not(miri))]
pub(crate) struct ObjectCache<V>(moka::future::Cache<BlockId, V>);

#[cfg(not(miri))]
impl<V> Clone for ObjectCache<V> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

#[cfg(not(miri))]
impl<V: Clone + Send + Sync + 'static> ObjectCache<V> {
    pub(crate) fn new(
        max_bytes: u64,
        weigh: impl Fn(&BlockId, &V) -> u32 + Send + Sync + 'static,
    ) -> Self {
        Self(
            moka::future::Cache::builder()
                .max_capacity(max_bytes)
                .weigher(weigh)
                .build(),
        )
    }

    pub(crate) async fn get(&self, id: &BlockId) -> Option<V> {
        self.0.get(id).await
    }

    pub(crate) fn contains_key(&self, id: &BlockId) -> bool {
        self.0.contains_key(id)
    }

    pub(crate) async fn insert(&self, id: BlockId, value: V) {
        self.0.insert(id, value).await;
    }
}

/// Moka uses crossbeam-epoch operations that current Miri rejects under its experimental alias
/// model. This semantic substitute keeps the same tiny contract for interpreter runs.
#[cfg(miri)]
pub(crate) struct ObjectCache<V> {
    enabled: bool,
    values: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<BlockId, V>>>,
}

#[cfg(miri)]
impl<V> Clone for ObjectCache<V> {
    fn clone(&self) -> Self {
        Self {
            enabled: self.enabled,
            values: self.values.clone(),
        }
    }
}

#[cfg(miri)]
impl<V: Clone> ObjectCache<V> {
    pub(crate) fn new(
        max_bytes: u64,
        _weigh: impl Fn(&BlockId, &V) -> u32 + Send + Sync + 'static,
    ) -> Self {
        Self {
            enabled: max_bytes > 0,
            values: Default::default(),
        }
    }

    pub(crate) async fn get(&self, id: &BlockId) -> Option<V> {
        self.values.lock().expect("miri cache").get(id).cloned()
    }

    pub(crate) fn contains_key(&self, id: &BlockId) -> bool {
        self.values.lock().expect("miri cache").contains_key(id)
    }

    pub(crate) async fn insert(&self, id: BlockId, value: V) {
        if self.enabled {
            self.values.lock().expect("miri cache").insert(id, value);
        }
    }
}
