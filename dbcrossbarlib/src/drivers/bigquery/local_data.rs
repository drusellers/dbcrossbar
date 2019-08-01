//! Helper for reading data from BigQuery.

use super::find_gs_temp_dir;
use crate::common::*;
use crate::drivers::bigquery::BigQueryLocator;

/// Implementation of `local_data`, but as a real `async` function.
pub(crate) async fn local_data_helper(
    ctx: Context,
    source: BigQueryLocator,
    schema: Table,
    query: Query,
    temporary_storage: TemporaryStorage,
    args: DriverArgs,
) -> Result<Option<BoxStream<CsvStream>>> {
    // Build a temporary location.
    let gs_temp = find_gs_temp_dir(&temporary_storage)?;
    let temp_args = DriverArgs::default();

    // Extract from BigQuery to gs://.
    let to_temp_ctx = ctx.child(o!("to_temp" => gs_temp.to_string()));
    gs_temp
        .write_remote_data(
            to_temp_ctx,
            schema.clone(),
            Box::new(source),
            query,
            temporary_storage.clone(),
            args,
            temp_args.clone(),
            IfExists::Overwrite,
        )
        .await?;

    // Copy from a temporary gs:// location.
    let from_temp_ctx = ctx.child(o!("from_temp" => gs_temp.to_string()));
    gs_temp
        .local_data(
            from_temp_ctx,
            schema,
            Query::default(),
            temporary_storage.clone(),
            temp_args,
        )
        .await
}
