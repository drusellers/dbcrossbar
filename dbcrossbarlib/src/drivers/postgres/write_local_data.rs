//! Support for writing local data to Postgres.

use std::{io::prelude::*, str};

use super::{connect, csv_to_binary::copy_csv_to_pg_binary, Connection};
use crate::common::*;
use crate::drivers::postgres_shared::{Ident, PgCreateTable, PgDataType};
use crate::tokio_glue::{run_sync_fn_in_background, SyncStreamReader};
use crate::transform::spawn_sync_transform;

/// If `table_name` exists, `DROP` it.
fn drop_table_if_exists(
    ctx: &Context,
    conn: &Connection,
    table_name: &str,
) -> Result<()> {
    debug!(ctx.log(), "deleting table {} if exists", table_name);
    let drop_sql = format!("DROP TABLE IF EXISTS {}", Ident(table_name));
    conn.execute(&drop_sql, &[])
        .with_context(|_| format!("error deleting existing {}", table_name))?;
    Ok(())
}

/// Run the specified `CREATE TABLE` SQL.
fn create_table(
    ctx: &Context,
    conn: &Connection,
    pg_create_table: &PgCreateTable,
) -> Result<()> {
    debug!(ctx.log(), "create table {}", pg_create_table.name);
    let create_sql = format!("{}", pg_create_table);
    trace!(ctx.log(), "CREATE TABLE SQL: {}", create_sql);
    conn.execute(&create_sql, &[])
        .with_context(|_| format!("error creating {}", pg_create_table.name))?;
    Ok(())
}

/// Run `DROP TABLE` and/or `CREATE TABLE` as needed to prepare `table` for
/// copying in data.
///
/// We take ownership of `pg_create_table` because we want to edit it before
/// running it.
fn prepare_table(
    ctx: &Context,
    conn: &Connection,
    mut pg_create_table: PgCreateTable,
    if_exists: IfExists,
) -> Result<()> {
    match if_exists {
        IfExists::Overwrite => {
            drop_table_if_exists(ctx, conn, &pg_create_table.name)?;
            pg_create_table.if_not_exists = false;
        }
        IfExists::Append => {
            // We create the table if it doesn't exist, but we're happy to use
            // whatever is already there. I hope the schema matches! (But we'll
            // provide a schema to `COPY dest (cols) FROM ...`, so that should
            // at least make sure we agree on column names and order.)
            pg_create_table.if_not_exists = true;
        }
        IfExists::Error => {
            // We always want to create the table, so omit `IF NOT EXISTS`. If
            // the table already exists, we will fail with an error.
            pg_create_table.if_not_exists = false;
        }
    }
    create_table(ctx, conn, &pg_create_table)
}

/// Generate the `COPY ... FROM ...` SQL we'll pass to `copy_in`. `data_format`
/// should be something like `"CSV HRADER"` or `"BINARY"`.
///
/// We have a separate function for generating this because we'll use it for
/// multiple `COPY` statements.
fn copy_from_sql(
    pg_create_table: &PgCreateTable,
    data_format: &str,
) -> Result<String> {
    let mut copy_sql_buff = vec![];
    writeln!(&mut copy_sql_buff, "COPY {:?} (", pg_create_table.name)?;
    for (idx, col) in pg_create_table.columns.iter().enumerate() {
        if let PgDataType::Array { .. } = col.data_type {
            return Err(format_err!("cannot yet import array column {:?}", col.name));
        }
        if idx + 1 == pg_create_table.columns.len() {
            writeln!(&mut copy_sql_buff, "    {:?}", col.name)?;
        } else {
            writeln!(&mut copy_sql_buff, "    {:?},", col.name)?;
        }
    }
    writeln!(&mut copy_sql_buff, ") FROM STDIN WITH {}", data_format)?;
    let copy_sql = str::from_utf8(&copy_sql_buff)
        .expect("generated SQL should always be UTF-8")
        .to_owned();
    Ok(copy_sql)
}

/// Copy data from `rdr` and insert it into the specified table. The
/// `copy_from_sql` SQL should have been generated by the [`copy_from_sql`]
/// function.
fn copy_from(
    ctx: &Context,
    conn: &Connection,
    table_name: &str,
    copy_from_sql: &str,
    mut rdr: Box<dyn Read>,
) -> Result<()> {
    debug!(ctx.log(), "copying data into table");
    let stmt = conn.prepare(copy_from_sql)?;
    stmt.copy_in(&[], &mut rdr)
        .with_context(|_| format!("error copying data into {}", table_name))?;
    Ok(())
}

/// Like `copy_from`, but safely callable from `async` code.
async fn copy_from_async(
    ctx: Context,
    url: Url,
    table_name: String,
    copy_from_sql: String,
    stream: Box<dyn Stream<Item = BytesMut, Error = Error> + Send + 'static>,
) -> Result<()> {
    await!(run_sync_fn_in_background(move || -> Result<()> {
        let conn = connect(&url)?;
        let rdr = SyncStreamReader::new(ctx.clone(), stream);
        copy_from(&ctx, &conn, &table_name, &copy_from_sql, Box::new(rdr))?;
        Ok(())
    }))
}

// The actual implementation of `write_local_data`, in a separate function so we
// can use `async`.
pub(crate) async fn write_local_data_helper(
    ctx: Context,
    url: Url,
    table_name: String,
    schema: Table,
    mut data: BoxStream<CsvStream>,
    if_exists: IfExists,
) -> Result<BoxStream<BoxFuture<()>>> {
    let ctx = ctx.child(o!("table" => schema.name.clone()));
    debug!(
        ctx.log(),
        "writing data streams to {} table {}", url, table_name,
    );

    // Convert our `schema` to a `PgCreateTable`.
    let pg_create_table =
        PgCreateTable::from_name_and_columns(table_name.clone(), &schema.columns)?;

    // Connect to PostgreSQL and prepare our table. We `drop(conn)` afterwards
    // because it can't be kept alive over an `await!`. This is because `conn`
    // isn't safe to send between threads (specifically, it doesn't implement
    // `Send`), and because `await!` may result in us getting scheduled onto
    // a different thread.
    let conn = connect(&url)?;
    prepare_table(&ctx, &conn, pg_create_table.clone(), if_exists)?;
    drop(conn);

    // Generate our `COPY ... FROM` SQL.
    let copy_sql = copy_from_sql(&pg_create_table, "BINARY")?;

    // Insert data streams one at a time, because parallel insertion _probably_
    // won't gain much with Postgres (but we haven't measured).
    let fut = async move {
        loop {
            match await!(data.into_future()) {
                Err((err, _rest_of_stream)) => {
                    debug!(ctx.log(), "error reading stream of streams: {}", err);
                    return Err(err);
                }
                Ok((None, _rest_of_stream)) => {
                    return Ok(());
                }
                Ok((Some(csv_stream), rest_of_stream)) => {
                    data = rest_of_stream;

                    let ctx = ctx.child(o!("stream" => csv_stream.name.clone()));

                    // Convert our CSV stream into a PostgreSQL `BINARY` stream.
                    let transform_ctx = ctx.child(o!("transform" => "csv_to_binary"));
                    let transform_table = pg_create_table.clone();
                    let binary_stream = spawn_sync_transform(
                        transform_ctx,
                        csv_stream.data,
                        move |_ctx, rdr, wtr| {
                            copy_csv_to_pg_binary(&transform_table, rdr, wtr)
                        },
                    )?;

                    // Run our copy code in a background thread.
                    await!(copy_from_async(
                        ctx,
                        url.clone(),
                        table_name.clone(),
                        copy_sql.clone(),
                        binary_stream,
                    ))?;
                }
            }
        }
    };
    Ok(box_stream_once(Ok(fut.into_boxed())))
}
