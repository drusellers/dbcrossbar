//! Driver for working with BigQuery schemas.

use std::{fmt, str::FromStr};

use crate::common::*;
use crate::drivers::gs::GsLocator;

mod table_name;
mod write_remote_data;

use self::table_name::TableName;
use self::write_remote_data::write_remote_data_helper;

/// URL scheme for `BigQueryLocator`.
pub(crate) const BIGQUERY_SCHEME: &str = "bigquery:";

/// A locator for a BigQuery table.
#[derive(Debug, Clone)]
pub struct BigQueryLocator {
    /// The table pointed to by this locator.
    table_name: TableName,
}

impl fmt::Display for BigQueryLocator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "bigquery:{}",
            self.table_name
        )
    }
}

impl FromStr for BigQueryLocator {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        if !s.starts_with(BIGQUERY_SCHEME) {
            return Err(format_err!("expected a bigquery: locator, found {}", s));
        }
        let table_name = s[BIGQUERY_SCHEME.len()..].parse()?;
        Ok(BigQueryLocator { table_name })
    }
}

impl Locator for BigQueryLocator {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn supports_write_remote_data(&self, source: &dyn Locator) -> bool {
        // We can only do `write_remote_data` if `source` is a `GsLocator`.
        // Otherwise, we need to do `write_local_data` like normal.
        source.as_any().is::<GsLocator>()
    }

    fn write_remote_data(
        &self,
        ctx: Context,
        schema: Table,
        source: BoxLocator,
        if_exists: IfExists,
    ) -> BoxFuture<()> {
        write_remote_data_helper(ctx, schema, source, self.to_owned(), if_exists)
            .into_boxed()
    }
}
