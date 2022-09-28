/*!
 * Defines the [Transaction] type that performs multiple [Operation]s with ACID properties.
*/

use futures::StreamExt;
use object_store::path::Path;
use uuid::Uuid;

use crate::{model::schema::SchemaV2, table::Table};
use anyhow::{anyhow, Result};

use self::operation::Operation;

mod operation;

/// Transactions let you perform a sequence of [Operation]s that can be committed to be performed with ACID guarantees.
pub struct Transaction<'table> {
    table: &'table mut Table,
    operations: Vec<Operation>,
}

impl<'table> Transaction<'table> {
    /// Create a transaction for the given table.
    pub fn new(table: &'table mut Table) -> Self {
        Transaction {
            table,
            operations: vec![],
        }
    }
    /// Update the schmema of the table
    pub fn update_schema(mut self, schema: SchemaV2) -> Self {
        self.operations.push(Operation::UpdateSchema(schema));
        self
    }
    /// Update the spec of the table
    pub fn update_spec(mut self, spec_id: i32) -> Self {
        self.operations.push(Operation::UpdateSpec(spec_id));
        self
    }
    /// Quickly append files to the table
    pub fn fast_append(mut self, files: Vec<String>) -> Self {
        self.operations.push(Operation::NewFastAppend(files));
        self
    }
    /// Commit the transaction to perform the [Operation]s with ACID guarantees.
    pub async fn commit(self) -> Result<()> {
        self.table.increment_sequence_number();
        self.table.new_snapshot();
        let table = futures::stream::iter(self.operations)
            .fold(
                Ok::<&mut Table, anyhow::Error>(self.table),
                |table, op| async move {
                    let table = table?;
                    op.execute(table).await?;
                    Ok(table)
                },
            )
            .await?;
        match (table.catalog(), table.identifier()) {
            (Some(catalog), Some(identifier)) => {
                let object_store = catalog.object_store();
                let location = &table.metadata().location;
                let transaction_uuid = Uuid::new_v4();
                let version = &table.metadata().last_sequence_number;
                let metadata_json = serde_json::to_string(&table.metadata())
                    .map_err(|err| anyhow!(err.to_string()))?;
                let metadata_file_location: Path = (location.to_string()
                    + "/metadata/"
                    + &version.to_string()
                    + "-"
                    + &transaction_uuid.to_string()
                    + ".metadata.json")
                    .into();
                object_store
                    .put(&metadata_file_location, metadata_json.into())
                    .await
                    .map_err(|err| anyhow!(err.to_string()))?;
                let previous_metadata_file_location = table.metadata_location();
                let new_table = catalog
                    .clone()
                    .update_table(
                        identifier.clone(),
                        metadata_file_location.as_ref(),
                        previous_metadata_file_location,
                    )
                    .await?;
                *table = new_table;
                Ok(())
            }
            (_, _) => {
                let object_store = table.object_store();
                let location = &table.metadata().location;
                let uuid = Uuid::new_v4();
                let version = &table.metadata().last_sequence_number;
                let metadata_json = serde_json::to_string(&table.metadata())
                    .map_err(|err| anyhow!(err.to_string()))?;
                let temp_path: Path =
                    (location.to_string() + "/metadata/" + &uuid.to_string() + ".metadata.json")
                        .into();
                let final_path: Path = (location.to_string()
                    + "/metadata/v"
                    + &version.to_string()
                    + ".metadata.json")
                    .into();
                object_store
                    .put(&temp_path, metadata_json.into())
                    .await
                    .map_err(|err| anyhow!(err.to_string()))?;
                object_store
                    .copy_if_not_exists(&temp_path, &final_path)
                    .await
                    .map_err(|err| anyhow!(err.to_string()))?;
                object_store
                    .delete(&temp_path)
                    .await
                    .map_err(|err| anyhow!(err.to_string()))?;
                let new_table = Table::load_file_system_table(location, &object_store).await?;
                *table = new_table;
                Ok(())
            }
        }
    }
}