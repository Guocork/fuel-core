use super::database_description::{
    IndexationKind,
    indexation_availability,
};
use crate::database::{
    Database,
    Error as DatabaseError,
    database_description::{
        DatabaseDescription,
        DatabaseMetadata,
    },
};
use fuel_core_storage::{
    Error as StorageError,
    Mappable,
    Result as StorageResult,
    StorageAsRef,
    StorageInspect,
    blueprint::plain::Plain,
    codec::postcard::Postcard,
    structured_storage::TableWithBlueprint,
};

/// The table that stores all metadata about the database.
pub struct MetadataTable<Description>(core::marker::PhantomData<Description>);

impl<Description> Mappable for MetadataTable<Description>
where
    Description: DatabaseDescription,
{
    type Key = ();
    type OwnedKey = ();
    type Value = DatabaseMetadata<Description::Height>;
    type OwnedValue = Self::Value;
}

impl<Description> TableWithBlueprint for MetadataTable<Description>
where
    Description: DatabaseDescription,
{
    type Blueprint = Plain<Postcard, Postcard>;
    type Column = Description::Column;

    fn column() -> Self::Column {
        Description::metadata_column()
    }
}

impl<Description, Stage> Database<Description, Stage>
where
    Description: DatabaseDescription,
    Self: StorageInspect<MetadataTable<Description>, Error = StorageError>,
{
    /// Ensures the version is correct.
    pub fn check_version(&self) -> StorageResult<()> {
        let Some(metadata) = self.storage::<MetadataTable<Description>>().get(&())?
        else {
            return Ok(());
        };

        if metadata.version() != Description::version() {
            return Err(DatabaseError::InvalidDatabaseVersion {
                found: metadata.version(),
                expected: Description::version(),
            }
            .into())
        }

        Ok(())
    }

    pub fn latest_height_from_metadata(
        &self,
    ) -> StorageResult<Option<Description::Height>> {
        let metadata = self.storage::<MetadataTable<Description>>().get(&())?;

        let metadata = metadata.map(|metadata| *metadata.height());

        Ok(metadata)
    }

    pub fn indexation_available(&self, kind: IndexationKind) -> StorageResult<bool> {
        let metadata = self
            .storage::<MetadataTable<Description>>()
            .get(&())?
            .map(|metadata| metadata.into_owned());

        let indexation_availability = indexation_availability::<Description>(metadata);
        Ok(indexation_availability.contains(&kind))
    }
}
