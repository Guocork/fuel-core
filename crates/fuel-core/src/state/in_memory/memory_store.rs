use crate::{
    database::{
        Error as DatabaseError,
        Result as DatabaseResult,
        database_description::{
            DatabaseDescription,
            on_chain::OnChain,
        },
    },
    state::{
        IterDirection,
        IterableKeyValueView,
        KeyValueView,
        TransactableStorage,
        in_memory::memory_view::MemoryView,
        iterable_key_value_view::IterableKeyValueViewWrapper,
    },
};
use fuel_core_storage::{
    Result as StorageResult,
    iter::{
        BoxedIter,
        IntoBoxedIter,
        IterableStore,
        iterator,
        keys_iterator,
    },
    kv_store::{
        KVItem,
        KeyItem,
        KeyValueInspect,
        StorageColumn,
        Value,
        WriteOperation,
    },
    transactional::{
        Changes,
        ReferenceBytesKey,
        StorageChanges,
    },
};
use std::{
    collections::{
        BTreeMap,
        HashSet,
    },
    fmt::Debug,
    ops::Deref,
    sync::Mutex,
};

#[derive(Debug)]
pub struct MemoryStore<Description = OnChain>
where
    Description: DatabaseDescription,
{
    inner: Vec<Mutex<BTreeMap<ReferenceBytesKey, Value>>>,
    _marker: core::marker::PhantomData<Description>,
}

impl<Description> Default for MemoryStore<Description>
where
    Description: DatabaseDescription,
{
    fn default() -> Self {
        use enum_iterator::all;

        let largest_column_idx = all::<Description::Column>()
            .map(|column| column.as_usize())
            .max()
            .expect("there should be atleast 1 column in the storage");

        Self {
            inner: (0..=largest_column_idx)
                .map(|_| Mutex::new(BTreeMap::new()))
                .collect(),
            _marker: Default::default(),
        }
    }
}

impl<Description> MemoryStore<Description>
where
    Description: DatabaseDescription,
{
    fn create_view(&self) -> MemoryView<Description> {
        // Lock all tables at the same time to have consistent view.
        let locks = self
            .inner
            .iter()
            .map(|lock| lock.lock().expect("Poisoned lock"))
            .collect::<Vec<_>>();
        let inner = locks
            .iter()
            .map(|btree| btree.deref().clone())
            .collect::<Vec<_>>();
        MemoryView {
            inner,
            _marker: Default::default(),
        }
    }

    pub fn iter_all(
        &self,
        column: Description::Column,
        prefix: Option<&[u8]>,
        start: Option<&[u8]>,
        direction: IterDirection,
    ) -> impl Iterator<Item = KVItem> + use<Description> {
        let lock = self.inner[column.as_usize()].lock().expect("poisoned");

        let collection: Vec<_> = iterator(&lock, prefix, start, direction)
            .map(|(key, value)| (key.clone().into(), value.clone()))
            .collect();

        collection.into_iter().map(Ok)
    }

    pub fn iter_all_keys(
        &self,
        column: Description::Column,
        prefix: Option<&[u8]>,
        start: Option<&[u8]>,
        direction: IterDirection,
    ) -> impl Iterator<Item = KeyItem> + use<Description> {
        let lock = self.inner[column.as_usize()].lock().expect("poisoned");

        let collection: Vec<_> = keys_iterator(&lock, prefix, start, direction)
            .map(|key| key.to_vec())
            .collect();

        collection.into_iter().map(Ok)
    }

    fn _insert_changes(
        &self,
        conflicts_finder: &mut HashSet<(u32, ReferenceBytesKey)>,
        changes: Changes,
    ) -> DatabaseResult<()> {
        for (column, btree) in changes.into_iter() {
            let mut lock = self.inner[column as usize]
                .lock()
                .map_err(|e| anyhow::anyhow!("The lock is poisoned: {}", e))?;

            for (key, operation) in btree.into_iter() {
                if !conflicts_finder.insert((column, key.clone())) {
                    return Err(DatabaseError::ConflictingChanges {
                        column,
                        key: key.clone(),
                    })
                }

                match operation {
                    WriteOperation::Insert(value) => {
                        lock.insert(key, value);
                    }
                    WriteOperation::Remove => {
                        lock.remove(&key);
                    }
                }
            }
        }
        Ok(())
    }
}

impl<Description> KeyValueInspect for MemoryStore<Description>
where
    Description: DatabaseDescription,
{
    type Column = Description::Column;

    fn get(&self, key: &[u8], column: Self::Column) -> StorageResult<Option<Value>> {
        Ok(self.inner[column.as_usize()]
            .lock()
            .map_err(|e| anyhow::anyhow!("The lock is poisoned: {}", e))?
            .get(key)
            .cloned())
    }
}

impl<Description> IterableStore for MemoryStore<Description>
where
    Description: DatabaseDescription,
{
    fn iter_store(
        &self,
        column: Self::Column,
        prefix: Option<&[u8]>,
        start: Option<&[u8]>,
        direction: IterDirection,
    ) -> BoxedIter<KVItem> {
        self.iter_all(column, prefix, start, direction).into_boxed()
    }

    fn iter_store_keys(
        &self,
        column: Self::Column,
        prefix: Option<&[u8]>,
        start: Option<&[u8]>,
        direction: IterDirection,
    ) -> BoxedIter<fuel_core_storage::kv_store::KeyItem> {
        self.iter_all_keys(column, prefix, start, direction)
            .into_boxed()
    }
}

impl<Description> TransactableStorage<Description::Height> for MemoryStore<Description>
where
    Description: DatabaseDescription,
{
    fn commit_changes(
        &self,
        _: Option<Description::Height>,
        changes: StorageChanges,
    ) -> StorageResult<()> {
        let mut conflicts_finder = HashSet::<(u32, ReferenceBytesKey)>::new();

        match changes {
            StorageChanges::ChangesList(changes) => {
                for changes in changes.into_iter() {
                    self._insert_changes(&mut conflicts_finder, changes)?;
                }
            }
            StorageChanges::Changes(changes) => {
                self._insert_changes(&mut conflicts_finder, changes)?;
            }
        };
        Ok(())
    }

    fn view_at_height(
        &self,
        _: &Description::Height,
    ) -> StorageResult<KeyValueView<Self::Column, Description::Height>> {
        // TODO: https://github.com/FuelLabs/fuel-core/issues/1995
        Err(
            anyhow::anyhow!("The historical view is not implemented for `MemoryStore`")
                .into(),
        )
    }

    fn latest_view(
        &self,
    ) -> StorageResult<IterableKeyValueView<Self::Column, Description::Height>> {
        let view = self.create_view();
        Ok(IterableKeyValueView::from_storage_and_metadata(
            IterableKeyValueViewWrapper::new(view),
            None,
        ))
    }

    fn rollback_block_to(&self, _: &Description::Height) -> StorageResult<()> {
        // TODO: https://github.com/FuelLabs/fuel-core/issues/1995
        Err(
            anyhow::anyhow!("The historical view is not implemented for `MemoryStore`")
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuel_core_storage::{
        column::Column,
        kv_store::KeyValueMutate,
        transactional::ReadTransaction,
    };

    impl<Description> KeyValueMutate for MemoryStore<Description>
    where
        Description: DatabaseDescription,
    {
        fn write(
            &mut self,
            key: &[u8],
            column: Self::Column,
            buf: &[u8],
        ) -> StorageResult<usize> {
            let mut transaction = self.read_transaction();
            let len = transaction.write(key, column, buf)?;
            let changes = transaction.into_changes();
            self.commit_changes(None, changes.into())?;
            Ok(len)
        }

        fn delete(&mut self, key: &[u8], column: Self::Column) -> StorageResult<()> {
            let mut transaction = self.read_transaction();
            transaction.delete(key, column)?;
            let changes = transaction.into_changes();
            self.commit_changes(None, changes.into())?;
            Ok(())
        }
    }

    #[test]
    fn can_use_unit_value() {
        let key = vec![0x00];

        let mut db = MemoryStore::<OnChain>::default();
        let expected = Value::from([]);
        db.put(&key.to_vec(), Column::Metadata, expected.clone())
            .unwrap();

        assert_eq!(db.get(&key, Column::Metadata).unwrap().unwrap(), expected);

        assert!(db.exists(&key, Column::Metadata).unwrap());

        assert_eq!(
            db.iter_all(Column::Metadata, None, None, IterDirection::Forward)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            vec![(key.clone(), expected.clone())]
        );

        assert_eq!(db.take(&key, Column::Metadata).unwrap().unwrap(), expected);

        assert!(!db.exists(&key, Column::Metadata).unwrap());
    }

    #[test]
    fn can_use_unit_key() {
        let key: Vec<u8> = Vec::with_capacity(0);

        let mut db = MemoryStore::<OnChain>::default();
        let expected = Value::from([1, 2, 3]);
        db.put(&key, Column::Metadata, expected.clone()).unwrap();

        assert_eq!(db.get(&key, Column::Metadata).unwrap().unwrap(), expected);

        assert!(db.exists(&key, Column::Metadata).unwrap());

        assert_eq!(
            db.iter_all(Column::Metadata, None, None, IterDirection::Forward)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            vec![(key.clone(), expected.clone())]
        );

        assert_eq!(db.take(&key, Column::Metadata).unwrap().unwrap(), expected);

        assert!(!db.exists(&key, Column::Metadata).unwrap());
    }

    #[test]
    fn can_use_unit_key_and_value() {
        let key: Vec<u8> = Vec::with_capacity(0);

        let mut db = MemoryStore::<OnChain>::default();
        let expected = Value::from([]);
        db.put(&key, Column::Metadata, expected.clone()).unwrap();

        assert_eq!(db.get(&key, Column::Metadata).unwrap().unwrap(), expected);

        assert!(db.exists(&key, Column::Metadata).unwrap());

        assert_eq!(
            db.iter_all(Column::Metadata, None, None, IterDirection::Forward)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            vec![(key.clone(), expected.clone())]
        );

        assert_eq!(db.take(&key, Column::Metadata).unwrap().unwrap(), expected);

        assert!(!db.exists(&key, Column::Metadata).unwrap());
    }
}
