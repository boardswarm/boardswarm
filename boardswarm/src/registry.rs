use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use tokio::sync::broadcast;
use tokio::sync::broadcast::Receiver;

const NAME: &str = "name";

#[derive(Clone, Debug)]
pub struct Properties {
    properties: HashMap<String, String>,
}

impl Properties {
    pub fn new<N: Into<String>>(name: N) -> Self {
        let mut properties = HashMap::new();
        properties.insert(NAME.to_string(), name.into());

        Self { properties }
    }

    pub fn name(&self) -> &str {
        self.get(NAME).unwrap_or_default()
    }

    pub fn get(&self, prop: &str) -> Option<&str> {
        self.properties.get(prop).map(String::as_ref)
    }

    pub fn matches<K, V, I>(&self, matches: I) -> bool
    where
        K: AsRef<str>,
        V: AsRef<str>,
        I: IntoIterator<Item = (K, V)>,
    {
        matches.into_iter().all(|(k, v)| {
            if let Some(prop) = self.get(k.as_ref()) {
                prop == v.as_ref()
            } else {
                false
            }
        })
    }

    pub fn insert<K, V>(&mut self, key: K, value: V)
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.properties.insert(key.into(), value.into());
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.properties.iter()
    }
}

#[derive(Clone, Debug)]
pub struct Item<T> {
    properties: Arc<Properties>,
    item: T,
}

impl<T> Item<T> {
    pub fn new(properties: Properties, item: T) -> Self {
        Item {
            properties: Arc::new(properties),
            item,
        }
    }
    pub fn name(&self) -> &str {
        self.properties.name()
    }

    pub fn properties(&self) -> Arc<Properties> {
        self.properties.clone()
    }

    pub fn inner(&self) -> &T {
        &self.item
    }

    pub fn into_inner(self) -> T {
        self.item
    }
}

#[derive(Clone)]
pub enum RegistryChange<T> {
    Added { id: u64, item: Item<T> },
    Removed(u64),
}

#[derive(Debug)]
struct RegistryInner<T> {
    next: u64,
    contents: BTreeMap<u64, Item<T>>,
}

#[derive(Debug)]
pub struct Registry<T> {
    monitor: broadcast::Sender<RegistryChange<T>>,
    inner: RwLock<RegistryInner<T>>,
}

impl<T> Registry<T>
where
    T: Clone,
{
    pub fn new() -> Self {
        Self {
            monitor: broadcast::channel(16).0,
            inner: RwLock::new(RegistryInner {
                next: 0,
                contents: BTreeMap::new(),
            }),
        }
    }

    pub fn add(&self, properties: Properties, item: T) -> u64 {
        let item = Item::new(properties, item);
        let mut inner = self.inner.write().unwrap();
        inner.next += 1;
        let id = inner.next;
        inner.contents.insert(id, item.clone());
        let _ = self.monitor.send(RegistryChange::Added { id, item });
        id
    }

    pub fn remove(&self, id: u64) {
        let mut inner = self.inner.write().unwrap();
        if let Some(_item) = inner.contents.remove(&id) {
            let _ = self.monitor.send(RegistryChange::Removed(id));
        }
    }

    pub fn lookup(&self, id: u64) -> Option<Item<T>> {
        let inner = self.inner.read().unwrap();
        inner.contents.get(&id).cloned()
    }

    #[allow(dead_code)]
    pub fn ids(&self) -> Vec<u64> {
        let inner = self.inner.read().unwrap();
        inner.contents.keys().copied().collect()
    }

    pub fn contents(&self) -> Vec<(u64, Item<T>)> {
        let inner = self.inner.read().unwrap();
        inner
            .contents
            .iter()
            .map(|(&id, item)| (id, item.clone()))
            .collect()
    }

    pub fn find<'a, K, V, I>(&self, matches: &'a I) -> Option<(u64, Item<T>)>
    where
        K: AsRef<str>,
        V: AsRef<str>,
        &'a I: IntoIterator<Item = (K, V)>,
    {
        let inner = self.inner.read().unwrap();
        inner
            .contents
            .iter()
            .find(|(&_id, item)| item.properties.matches(matches))
            .map(|(&id, item)| (id, item.clone()))
    }

    pub fn find_by_name(&self, name: &str) -> Option<(u64, Item<T>)> {
        let inner = self.inner.read().unwrap();
        inner
            .contents
            .iter()
            .find(|(&_id, item)| item.properties.name() == name)
            .map(|(&id, item)| (id, item.clone()))
    }

    pub fn monitor(&self) -> Receiver<RegistryChange<T>> {
        self.monitor.subscribe()
    }
}

impl<T> Default for Registry<T>
where
    T: Clone,
{
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn properties() {
        let mut props = Properties::new("test");
        props.insert("udev.BADGER", "5");

        assert_eq!(props.get(NAME), Some("test"));
        assert_eq!(props.name(), "test");

        let mut t = HashMap::new();
        t.insert("name".to_string(), "test".to_string());
        assert_eq!(props.matches(&t), true);

        assert_eq!(props.matches([("name", "test")]), true);
        assert_eq!(props.matches([("udev.BADGER", "5")]), true);

        assert_eq!(props.matches([("name", "no")]), false);
        assert_eq!(props.matches([("udev.BADGER", "7")]), false);
        assert_eq!(props.matches([("udev.SNAKE", "7")]), false);

        assert_eq!(
            props.matches([("name", "test"), ("udev.BADGER", "5")]),
            true
        );
        assert_eq!(
            props.matches([("name", "test"), ("udev.BADGER", "7")]),
            false
        );
        assert_eq!(
            props.matches([("name", "test"), ("udev.SNAKE", "5")]),
            false
        );
    }
}
