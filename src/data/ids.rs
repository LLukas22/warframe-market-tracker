//! Typed ids for the store's tables. Each table can only be indexed by its own id, so an
//! auction's id can't look up a riven by mistake.

use std::marker::PhantomData;
use std::ops::{Index, IndexMut};

/// A dense index into one table.
pub(crate) trait Id: Copy {
    fn index(self) -> usize;
    /// Panics rather than wrap around when a table outgrows its id type: ids are kept small
    /// to keep the indexes compact, so running out is a bug to fix.
    fn from_index(index: usize) -> Self;
}

macro_rules! ids {
    ($($(#[$doc:meta])* $name:ident($repr:ty),)*) => {$(
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub(crate) struct $name($repr);

        impl Id for $name {
            fn index(self) -> usize {
                self.0 as usize
            }

            fn from_index(index: usize) -> Self {
                Self(<$repr>::try_from(index).unwrap_or_else(|_| {
                    panic!(concat!("too many items for ", stringify!($name)))
                }))
            }
        }
    )*};
}

ids! {
    /// A weapon, in `Inner::weapons`.
    WeaponId(u16),
    /// An attribute slug, in `Inner::attrs`.
    AttrId(u16),
    /// A riven: every listing of the same mod, in `Inner::rivens`.
    RivenId(u32),
    /// A listing, in `Inner::auctions`.
    AuctionId(u32),
}

impl RivenId {
    /// The riven of a scanned riven, which isn't in the data.
    pub(crate) const UNLISTED: Self = Self(u32::MAX);
}

/// A `Vec` indexed by its own id type.
pub(crate) struct Table<I, T> {
    rows: Vec<T>,
    id: PhantomData<fn() -> I>,
}

impl<I: Id, T> Table<I, T> {
    /// Adds a row, returning its id.
    pub(crate) fn push(&mut self, row: T) -> I {
        let id = I::from_index(self.rows.len());
        self.rows.push(row);
        id
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub(crate) fn iter(&self) -> std::slice::Iter<'_, T> {
        self.rows.iter()
    }

    pub(crate) fn iter_mut(&mut self) -> std::slice::IterMut<'_, T> {
        self.rows.iter_mut()
    }

    /// Every id, in order.
    #[cfg(test)]
    pub(crate) fn ids(&self) -> impl DoubleEndedIterator<Item = I> + use<I, T> {
        (0..self.rows.len()).map(I::from_index)
    }

    /// Every row with its id.
    pub(crate) fn enumerate(&self) -> impl DoubleEndedIterator<Item = (I, &T)> {
        self.rows
            .iter()
            .enumerate()
            .map(|(i, row)| (I::from_index(i), row))
    }

    /// A table with a `value` for every id of this one.
    pub(crate) fn same_ids<U: Clone>(&self, value: U) -> Table<I, U> {
        Table::from(vec![value; self.rows.len()])
    }
}

impl<I, T> From<Vec<T>> for Table<I, T> {
    fn from(rows: Vec<T>) -> Self {
        Self {
            rows,
            id: PhantomData,
        }
    }
}

impl<I, T> FromIterator<T> for Table<I, T> {
    fn from_iter<It: IntoIterator<Item = T>>(iter: It) -> Self {
        Self::from(iter.into_iter().collect::<Vec<_>>())
    }
}

impl<I, T> IntoIterator for Table<I, T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.into_iter()
    }
}

impl<'a, I, T> IntoIterator for &'a Table<I, T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.iter()
    }
}

impl<'a, I, T> IntoIterator for &'a mut Table<I, T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.iter_mut()
    }
}

impl<I, T> Default for Table<I, T> {
    fn default() -> Self {
        Self::from(Vec::new())
    }
}

impl<I, T: Clone> Clone for Table<I, T> {
    fn clone(&self) -> Self {
        Self::from(self.rows.clone())
    }
}

impl<I: Id, T> Index<I> for Table<I, T> {
    type Output = T;

    fn index(&self, id: I) -> &T {
        &self.rows[id.index()]
    }
}

impl<I: Id, T> IndexMut<I> for Table<I, T> {
    fn index_mut(&mut self, id: I) -> &mut T {
        &mut self.rows[id.index()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_index_their_own_table() {
        let mut weapons: Table<WeaponId, &str> = Table::default();
        let a = weapons.push("a");
        let b = weapons.push("b");
        assert_eq!((weapons[a], weapons[b]), ("a", "b"));
        assert_eq!(weapons.ids().collect::<Vec<_>>(), [a, b]);
        let counts = weapons.same_ids(0);
        assert_eq!(counts[b], 0);
    }

    #[test]
    #[should_panic(expected = "too many items for WeaponId")]
    fn running_out_of_ids_panics() {
        WeaponId::from_index(usize::from(u16::MAX) + 1);
    }
}
