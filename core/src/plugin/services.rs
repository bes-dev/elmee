//! Typed services shared between installed components, and what the loop
//! requires of them.

use super::*;

/// An open typed service registry shared by installed components.
///
/// The type map is an implementation detail: callers receive typed `Rc<T>`
/// handles, and a plugin can only reach services it explicitly asks for.
#[derive(Clone, Default)]
pub struct Services(Rc<RefCell<ServiceMap>>);

type ServiceMap = HashMap<TypeId, ServiceBucket>;

struct ServiceBucket {
    name: &'static str,
    entries: Vec<ServiceEntry>,
}

struct ServiceEntry {
    owner: &'static str,
    value: Rc<dyn Any>,
}

pub(super) struct ServiceValue<T: ?Sized + 'static>(Rc<T>);

impl Services {
    pub(super) fn provide<T: ?Sized + 'static>(&self, owner: &'static str, service: Rc<T>) {
        self.0
            .borrow_mut()
            .entry(TypeId::of::<ServiceValue<T>>())
            .or_insert_with(|| ServiceBucket { name: std::any::type_name::<T>(), entries: Vec::new() })
            .entries
            .push(ServiceEntry { owner, value: Rc::new(ServiceValue(service)) });
    }

    pub(super) fn merge(&self, other: Services) {
        let mut destination = self.0.borrow_mut();
        for (kind, mut bucket) in std::mem::take(&mut *other.0.borrow_mut()) {
            destination
                .entry(kind)
                .or_insert_with(|| ServiceBucket { name: bucket.name, entries: Vec::new() })
                .entries
                .append(&mut bucket.entries);
        }
        for bucket in destination.values_mut() {
            bucket.entries.sort_by_key(|entry| entry.owner);
        }
    }

    /// How many providers one service type has.
    pub(super) fn count(&self, type_id: TypeId) -> usize {
        self.0.borrow().get(&type_id).map_or(0, |bucket| bucket.entries.len())
    }

    /// Every service by type name with the plugins providing it, in a stable
    /// order.
    pub(super) fn providers(&self) -> Vec<(&'static str, Vec<&'static str>)> {
        let mut providers: Vec<_> = self
            .0
            .borrow()
            .values()
            .map(|bucket| (bucket.name, bucket.entries.iter().map(|entry| entry.owner).collect::<Vec<_>>()))
            .collect();
        providers.sort();
        providers
    }

    pub fn all<T: ?Sized + 'static>(&self) -> Vec<Rc<T>> {
        self.0
            .borrow()
            .get(&TypeId::of::<ServiceValue<T>>())
            .into_iter()
            .flat_map(|bucket| &bucket.entries)
            .filter_map(|entry| entry.value.downcast_ref::<ServiceValue<T>>())
            .map(|entry| entry.0.clone())
            .collect()
    }

    pub fn one<T: ?Sized + 'static>(&self) -> Result<Option<Rc<T>>> {
        let found = self.all::<T>();
        match found.len() {
            0 => Ok(None),
            1 => Ok(found.into_iter().next()),
            count => Err(Error::Config(format!(
                "service {} has {count} providers; exactly one is required",
                std::any::type_name::<T>()
            ))),
        }
    }
}

/// Cardinality promised by a plugin for one typed runtime capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Cardinality {
    Exactly,
    AtMost,
}

pub(super) struct Requirement {
    pub(super) owner: &'static str,
    pub(super) type_id: TypeId,
    pub(super) type_name: &'static str,
    pub(super) cardinality: Cardinality,
}
