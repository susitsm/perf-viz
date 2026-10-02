//! Typed indices and a vector indexed by them, so that a trie node id cannot index the frame table.
//!
//! [`define_index!`](crate::define_index) declares an id type; [`IndexVec`] is a `Vec` that is indexed by one id type
//! only. Ids store `index + 1` in a `NonZeroU32`, so an `Option<Id>` is as small as the id itself
//! and can stand for "no such id" without a sentinel value.

use std::{
    fmt,
    hash::Hash,
    marker::PhantomData,
    ops::{Index, IndexMut},
};

/// An id type that stands for a position in an [`IndexVec`].
pub trait Idx: Copy + Eq + Ord + Hash + fmt::Debug {
    /// The id for position `index`. Panics if it does not fit.
    fn new(index: usize) -> Self;
    fn index(self) -> usize;
}

/// Declares an id type: `define_index!(pub NodeId);`.
#[macro_export]
macro_rules! define_index {
    ($(#[$meta:meta])* $vis:vis $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        $vis struct $name(::std::num::NonZeroU32);

        impl $crate::index::Idx for $name {
            fn new(index: usize) -> Self {
                let stored = u32::try_from(index)
                    .ok()
                    .and_then(|index| index.checked_add(1))
                    .and_then(::std::num::NonZeroU32::new)
                    .expect(concat!(stringify!($name), " out of range"));
                Self(stored)
            }

            fn index(self) -> usize {
                self.0.get() as usize - 1
            }
        }

        impl ::std::fmt::Debug for $name {
            fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(formatter, "{}({})", stringify!($name), self.0.get() - 1)
            }
        }
    };
}

/// A vector indexed by `I` instead of `usize`.
pub struct IndexVec<I: Idx, T> {
    raw: Vec<T>,
    index_type: PhantomData<fn(&I)>,
}

impl<I: Idx, T> IndexVec<I, T> {
    pub fn new() -> Self {
        Self::from_raw(Vec::new())
    }

    pub fn from_raw(raw: Vec<T>) -> Self {
        Self {
            raw,
            index_type: PhantomData,
        }
    }

    /// `len` copies of `element`.
    pub fn from_elem(element: T, len: usize) -> Self
    where
        T: Clone,
    {
        Self::from_raw(vec![element; len])
    }

    /// Appends `value` and returns its id.
    pub fn push(&mut self, value: T) -> I {
        let id = self.next_index();
        self.raw.push(value);
        id
    }

    /// The id the next [`push`](Self::push) will return.
    pub fn next_index(&self) -> I {
        I::new(self.raw.len())
    }

    pub fn len(&self) -> usize {
        self.raw.len()
    }

    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    pub fn get(&self, id: I) -> Option<&T> {
        self.raw.get(id.index())
    }

    pub fn get_mut(&mut self, id: I) -> Option<&mut T> {
        self.raw.get_mut(id.index())
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.raw.iter()
    }

    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, T> {
        self.raw.iter_mut()
    }

    /// Each element with its id.
    pub fn iter_enumerated(&self) -> impl DoubleEndedIterator<Item = (I, &T)> + ExactSizeIterator {
        self.raw
            .iter()
            .enumerate()
            .map(|(index, value)| (I::new(index), value))
    }

    /// Every id, in order.
    pub fn indices(&self) -> impl DoubleEndedIterator<Item = I> + ExactSizeIterator + use<I, T> {
        (0..self.raw.len()).map(I::new)
    }

    pub fn as_raw_slice(&self) -> &[T] {
        &self.raw
    }

    pub fn into_raw(self) -> Vec<T> {
        self.raw
    }
}

impl<I: Idx, T> Index<I> for IndexVec<I, T> {
    type Output = T;

    fn index(&self, id: I) -> &T {
        &self.raw[id.index()]
    }
}

impl<I: Idx, T> IndexMut<I> for IndexVec<I, T> {
    fn index_mut(&mut self, id: I) -> &mut T {
        &mut self.raw[id.index()]
    }
}

impl<I: Idx, T> Default for IndexVec<I, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<I: Idx, T: Clone> Clone for IndexVec<I, T> {
    fn clone(&self) -> Self {
        Self::from_raw(self.raw.clone())
    }
}

impl<I: Idx, T: fmt::Debug> fmt::Debug for IndexVec<I, T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.raw.fmt(formatter)
    }
}

impl<I: Idx, T: PartialEq> PartialEq for IndexVec<I, T> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl<I: Idx, T> FromIterator<T> for IndexVec<I, T> {
    fn from_iter<Iter: IntoIterator<Item = T>>(iter: Iter) -> Self {
        Self::from_raw(iter.into_iter().collect())
    }
}

impl<I: Idx, T> IntoIterator for IndexVec<I, T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.raw.into_iter()
    }
}

impl<'a, I: Idx, T> IntoIterator for &'a IndexVec<I, T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.raw.iter()
    }
}

impl<'a, I: Idx, T> IntoIterator for &'a mut IndexVec<I, T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.raw.iter_mut()
    }
}

impl<I: Idx, T> Extend<T> for IndexVec<I, T> {
    fn extend<Iter: IntoIterator<Item = T>>(&mut self, iter: Iter) {
        self.raw.extend(iter);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    define_index!(TestId);

    #[test]
    fn push_returns_consecutive_ids_that_index_back() {
        let mut values: IndexVec<TestId, &str> = IndexVec::new();
        let first = values.push("a");
        let second = values.push("b");
        assert_eq!((first.index(), second.index()), (0, 1));
        assert_eq!((values[first], values[second]), ("a", "b"));
        assert_eq!(values.next_index().index(), 2);
        values[first] = "c";
        assert_eq!(values.as_raw_slice(), ["c", "b"]);
    }

    #[test]
    fn iter_enumerated_and_indices_pair_ids_with_values() {
        let values: IndexVec<TestId, u32> = [10, 20, 30].into_iter().collect();
        let pairs: Vec<_> = values
            .iter_enumerated()
            .map(|(id, value)| (id.index(), *value))
            .collect();
        assert_eq!(pairs, [(0, 10), (1, 20), (2, 30)]);
        assert_eq!(values.indices().count(), 3);
        assert_eq!(
            IndexVec::<TestId, u8>::from_elem(7, 2).as_raw_slice(),
            [7, 7]
        );
    }

    #[test]
    fn an_optional_id_is_as_small_as_the_id() {
        assert_eq!(size_of::<TestId>(), 4);
        assert_eq!(size_of::<Option<TestId>>(), 4);
    }

    #[test]
    fn ids_print_their_name_and_position() {
        assert_eq!(format!("{:?}", TestId::new(3)), "TestId(3)");
    }

    #[test]
    #[should_panic(expected = "out of range")]
    fn an_index_that_does_not_fit_panics() {
        TestId::new(u32::MAX as usize + 1);
    }

    #[test]
    fn get_is_none_past_the_end() {
        let values: IndexVec<TestId, u8> = IndexVec::from_elem(0, 1);
        assert!(values.get(TestId::new(0)).is_some());
        assert!(values.get(TestId::new(1)).is_none());
    }
}
