use std::{fmt::Debug, hash::Hash, sync::Arc};

use tokio::sync::OwnedMutexGuard as Omg;

use crate::{Result, SyncMutex, err};

/// Map of Mutexes
pub struct MutexMap<Key, Val> {
	map: Map<Key, Val>,
}

pub struct Guard<Key, Val>
where
	Key: Clone + Eq + Hash + Send,
	Val: Default + Send,
{
	map: Map<Key, Val>,
	key: Key,
	val: Omg<Val>,
}

type Map<Key, Val> = Arc<MapMutex<Key, Val>>;
type MapMutex<Key, Val> = SyncMutex<HashMap<Key, Val>>;
type HashMap<Key, Val> = std::collections::HashMap<Key, Value<Val>>;
type Value<Val> = Arc<tokio::sync::Mutex<Val>>;

impl<Key, Val> MutexMap<Key, Val>
where
	Key: Clone + Eq + Hash + Send,
	Val: Default + Send,
{
	#[must_use]
	pub fn new() -> Self {
		Self {
			map: Map::new(MapMutex::new(HashMap::new())),
		}
	}

	#[tracing::instrument(level = "trace", skip(self))]
	pub async fn lock<'a, K>(&'a self, k: &'a K) -> Guard<Key, Val>
	where
		K: Debug + Send + ?Sized + Sync,
		Key: TryFrom<&'a K>,
		<Key as TryFrom<&'a K>>::Error: Debug,
	{
		let key: Key = k.try_into().expect("failed to construct key");
		let val = self.map.lock().entry(key.clone()).or_default().clone();

		Guard::<Key, Val> {
			key,
			map: Arc::clone(&self.map),
			val: val.lock_owned().await,
		}
	}

	#[tracing::instrument(level = "trace", skip(self))]
	pub fn try_lock<'a, K>(&self, k: &'a K) -> Result<Guard<Key, Val>>
	where
		K: Debug + Send + ?Sized + Sync,
		Key: TryFrom<&'a K>,
		<Key as TryFrom<&'a K>>::Error: Debug,
	{
		let key: Key = k.try_into().expect("failed to construct key");
		let val = self.map.lock().entry(key.clone()).or_default().clone();

		Ok(Guard::<Key, Val> {
			key,
			map: Arc::clone(&self.map),
			val: val.try_lock_owned().map_err(|_| err!("would yield"))?,
		})
	}

	#[tracing::instrument(level = "trace", skip(self))]
	pub fn try_try_lock<'a, K>(&self, k: &'a K) -> Result<Guard<Key, Val>>
	where
		K: Debug + Send + ?Sized + Sync,
		Key: TryFrom<&'a K>,
		<Key as TryFrom<&'a K>>::Error: Debug,
	{
		let key: Key = k.try_into().expect("failed to construct key");
		let val = self
			.map
			.try_lock()
			.ok_or_else(|| err!("would block"))?
			.entry(key.clone())
			.or_default()
			.clone();

		Ok(Guard::<Key, Val> {
			key,
			map: Arc::clone(&self.map),
			val: val.try_lock_owned().map_err(|_| err!("would yield"))?,
		})
	}

	#[must_use]
	pub fn contains(&self, k: &Key) -> bool { self.map.lock().contains_key(k) }

	#[must_use]
	pub fn is_empty(&self) -> bool { self.map.lock().is_empty() }

	#[must_use]
	pub fn len(&self) -> usize { self.map.lock().len() }
}

impl<Key, Val> Default for MutexMap<Key, Val>
where
	Key: Clone + Eq + Hash + Send,
	Val: Default + Send,
{
	fn default() -> Self { Self::new() }
}

impl<Key, Val> Drop for Guard<Key, Val>
where
	Key: Clone + Eq + Hash + Send,
	Val: Default + Send,
{
	#[tracing::instrument(name = "unlock", level = "trace", skip_all)]
	fn drop(&mut self) {
		if Arc::strong_count(Omg::mutex(&self.val)) <= 2 {
			let mut map = self.map.lock();
			if let std::collections::hash_map::Entry::Occupied(e) = map.entry(self.key.clone()) {
				if Arc::ptr_eq(e.get(), Omg::mutex(&self.val)) && Arc::strong_count(e.get()) <= 2
				{
					e.remove();
				}
			}
		}
	}
}
