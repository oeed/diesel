pub(super) mod copy;
pub(crate) mod cursor;
mod raw;
mod result;
mod row;
mod stmt;

use self::copy::{CopyFromSink, CopyToBuffer};
use self::cursor::*;
use self::private::ConnectionAndTransactionManager;
use self::raw::{PgTransactionStatus, RawConnection};
use self::stmt::Statement;
use crate::connection::instrumentation::{
    DebugQuery, DynInstrumentation, Instrumentation, StrQueryHelper,
};
use crate::connection::statement_cache::{MaybeCached, StatementCache};
use crate::connection::*;
use crate::expression::QueryMetadata;
use crate::pg::backend::PgNotification;
use crate::pg::metadata_lookup::{GetPgMetadataCache, PgMetadataCache};
use crate::pg::query_builder::copy::InternalCopyFromQuery;
use crate::pg::{Pg, TransactionBuilder};
use crate::query_builder::bind_collector::RawBytesBindCollector;
use crate::query_builder::*;
use crate::result::ConnectionError::CouldntSetupConfiguration;
use crate::result::*;
use crate::RunQueryDsl;
use std::ffi::CString;
use std::fmt::Debug;
use std::os::raw as libc;

use super::query_builder::copy::{CopyFromExpression, CopyTarget, CopyToCommand};

pub(super) use self::result::PgResult;

/// The connection string expected by `PgConnection::establish`
/// should be a PostgreSQL connection string, as documented at
/// <https://www.postgresql.org/docs/9.4/static/libpq-connect.html#LIBPQ-CONNSTRING>
///
/// # Supported loading model implementations
///
/// * [`DefaultLoadingMode`]
/// * [`PgRowByRowLoadingMode`]
///
/// If you are unsure which loading mode is the correct one for your application,
/// you likely want to use the `DefaultLoadingMode` as that one offers
/// generally better performance.
///
/// Due to the fact that `PgConnection` supports multiple loading modes
/// it is **required** to always specify the used loading mode
/// when calling [`RunQueryDsl::load_iter`]
///
/// ## `DefaultLoadingMode`
///
/// By using this mode `PgConnection` defaults to loading all response values at **once**
/// and only performs deserialization afterward for the `DefaultLoadingMode`.
/// Generally this mode will be more performant as it.
///
/// This loading mode allows users to perform hold more than one iterator at once using
/// the same connection:
/// ```rust
/// # include!("../../doctest_setup.rs");
/// #
/// # fn main() {
/// #     run_test().unwrap();
/// # }
/// #
/// # fn run_test() -> QueryResult<()> {
/// #     use schema::users;
/// #     let connection = &mut establish_connection();
/// use diesel::connection::DefaultLoadingMode;
///
/// let iter1 = users::table.load_iter::<(i32, String), DefaultLoadingMode>(connection)?;
/// let iter2 = users::table.load_iter::<(i32, String), DefaultLoadingMode>(connection)?;
///
/// for r in iter1 {
///     let (id, name) = r?;
///     println!("Id: {} Name: {}", id, name);
/// }
///
/// for r in iter2 {
///     let (id, name) = r?;
///     println!("Id: {} Name: {}", id, name);
/// }
/// #   Ok(())
/// # }
/// ```
///
/// ## `PgRowByRowLoadingMode`
///
/// By using this mode `PgConnection` defaults to loading each row of the result set
/// separately. This might be desired for huge result sets.
///
/// This loading mode **prevents** creating more than one iterator at once using
/// the same connection. The following code is **not** allowed:
///
/// ```compile_fail
/// # include!("../../doctest_setup.rs");
/// #
/// # fn main() {
/// #     run_test().unwrap();
/// # }
/// #
/// # fn run_test() -> QueryResult<()> {
/// #     use schema::users;
/// #     let connection = &mut establish_connection();
/// use diesel::pg::PgRowByRowLoadingMode;
///
/// let iter1 = users::table.load_iter::<(i32, String), PgRowByRowLoadingMode>(connection)?;
/// // creating a second iterator generates an compiler error
/// let iter2 = users::table.load_iter::<(i32, String), PgRowByRowLoadingMode>(connection)?;
///
/// for r in iter1 {
///     let (id, name) = r?;
///     println!("Id: {} Name: {}", id, name);
/// }
///
/// for r in iter2 {
///     let (id, name) = r?;
///     println!("Id: {} Name: {}", id, name);
/// }
/// #   Ok(())
/// # }
/// ```
#[allow(missing_debug_implementations)]
#[cfg(feature = "postgres")]
pub struct PgConnection {
    statement_cache: StatementCache<Pg, Statement>,
    metadata_cache: PgMetadataCache,
    connection_and_transaction_manager: ConnectionAndTransactionManager,
}

// according to libpq documentation a connection can be transferred to other threads
#[allow(unsafe_code)]
unsafe impl Send for PgConnection {}

impl SimpleConnection for PgConnection {
    #[allow(unsafe_code)] // use of unsafe function
    fn batch_execute(&mut self, query: &str) -> QueryResult<()> {
        self.connection_and_transaction_manager
            .instrumentation
            .on_connection_event(InstrumentationEvent::StartQuery {
                query: &StrQueryHelper::new(query),
            });
        let c_query = CString::new(query)?;
        let inner_result = unsafe {
            self.connection_and_transaction_manager
                .raw_connection
                .exec(c_query.as_ptr())
        };
        update_transaction_manager_status(
            inner_result.and_then(|raw_result| {
                PgResult::new(
                    raw_result,
                    &self.connection_and_transaction_manager.raw_connection,
                )
            }),
            &mut self.connection_and_transaction_manager,
            &StrQueryHelper::new(query),
            true,
        )?;
        Ok(())
    }
}

/// A [`PgConnection`] specific loading mode to load rows one by one
///
/// See the documentation of [`PgConnection`] for details
#[derive(Debug, Copy, Clone)]
pub struct PgRowByRowLoadingMode;

impl ConnectionSealed for PgConnection {}

impl Connection for PgConnection {
    type Backend = Pg;
    type TransactionManager = AnsiTransactionManager;

    fn establish(database_url: &str) -> ConnectionResult<PgConnection> {
        let mut instrumentation = DynInstrumentation::default_instrumentation();
        instrumentation.on_connection_event(InstrumentationEvent::StartEstablishConnection {
            url: database_url,
        });
        let r = RawConnection::establish(database_url).and_then(|raw_conn| {
            let mut conn = PgConnection {
                connection_and_transaction_manager: ConnectionAndTransactionManager {
                    raw_connection: raw_conn,
                    transaction_state: AnsiTransactionManager::default(),
                    instrumentation: DynInstrumentation::none(),
                },
                statement_cache: StatementCache::new(),
                metadata_cache: PgMetadataCache::new(),
            };
            conn.set_config_options()
                .map_err(CouldntSetupConfiguration)?;
            Ok(conn)
        });
        instrumentation.on_connection_event(InstrumentationEvent::FinishEstablishConnection {
            url: database_url,
            error: r.as_ref().err(),
        });
        let mut conn = r?;
        conn.connection_and_transaction_manager.instrumentation = instrumentation;
        Ok(conn)
    }

    fn execute_returning_count<T>(&mut self, source: &T) -> QueryResult<usize>
    where
        T: QueryFragment<Pg> + QueryId,
    {
        update_transaction_manager_status(
            self.with_prepared_query(source, true, |query, params, conn, _source| {
                let res = query
                    .execute(&mut conn.raw_connection, &params, false)
                    .map(|r| r.rows_affected());
                // according to https://www.postgresql.org/docs/current/libpq-async.html
                // `PQgetResult` needs to be called till a null pointer is returned
                while conn.raw_connection.get_next_result()?.is_some() {}
                res
            }),
            &mut self.connection_and_transaction_manager,
            &crate::debug_query(source),
            true,
        )
    }

    fn transaction_state(&mut self) -> &mut AnsiTransactionManager
    where
        Self: Sized,
    {
        &mut self.connection_and_transaction_manager.transaction_state
    }

    fn instrumentation(&mut self) -> &mut dyn Instrumentation {
        &mut *self.connection_and_transaction_manager.instrumentation
    }

    fn set_instrumentation(&mut self, instrumentation: impl Instrumentation) {
        self.connection_and_transaction_manager.instrumentation = instrumentation.into();
    }

    fn set_prepared_statement_cache_size(&mut self, size: CacheSize) {
        self.statement_cache.set_cache_size(size);
    }
}

impl<B> LoadConnection<B> for PgConnection
where
    Self: self::private::PgLoadingMode<B>,
{
    type Cursor<'conn, 'query> = <Self as self::private::PgLoadingMode<B>>::Cursor<'conn, 'query>;
    type Row<'conn, 'query> = <Self as self::private::PgLoadingMode<B>>::Row<'conn, 'query>;

    fn load<'conn, 'query, T>(
        &'conn mut self,
        source: T,
    ) -> QueryResult<Self::Cursor<'conn, 'query>>
    where
        T: Query + QueryFragment<Self::Backend> + QueryId + 'query,
        Self::Backend: QueryMetadata<T::SqlType>,
    {
        self.with_prepared_query(source, false, |stmt, params, conn, source| {
            use self::private::PgLoadingMode;
            let result = stmt.execute(&mut conn.raw_connection, &params, Self::USE_ROW_BY_ROW_MODE);
            let result = update_transaction_manager_status(
                result,
                conn,
                &crate::debug_query(&source),
                false,
            )?;
            Self::get_cursor(conn, result, source)
        })
    }
}

impl GetPgMetadataCache for PgConnection {
    fn get_metadata_cache(&mut self) -> &mut PgMetadataCache {
        &mut self.metadata_cache
    }
}

#[inline(always)]
fn update_transaction_manager_status<T>(
    query_result: QueryResult<T>,
    conn: &mut ConnectionAndTransactionManager,
    source: &dyn DebugQuery,
    final_call: bool,
) -> QueryResult<T> {
    /// avoid monomorphizing for every result type - this part will not be inlined
    fn non_generic_inner(conn: &mut ConnectionAndTransactionManager, is_err: bool) {
        let raw_conn: &mut RawConnection = &mut conn.raw_connection;
        let tm: &mut AnsiTransactionManager = &mut conn.transaction_state;
        // libpq keeps track of the transaction status internally, and that is accessible
        // via `transaction_status`. We can use that to update the AnsiTransactionManager
        // status
        match raw_conn.transaction_status() {
            PgTransactionStatus::InError => {
                tm.status.set_requires_rollback_maybe_up_to_top_level(true)
            }
            PgTransactionStatus::Unknown => tm.status.set_in_error(),
            PgTransactionStatus::Idle => {
                // This is useful in particular for commit attempts (even
                // if `COMMIT` errors it still exits transaction)

                // This may repair the transaction manager
                tm.status = TransactionManagerStatus::Valid(Default::default())
            }
            PgTransactionStatus::InTransaction => {
                let transaction_status = &mut tm.status;
                // If we weren't an error, it is possible that we were a transaction start
                // -> we should tolerate any state
                if is_err {
                    // An error may not have us enter a transaction, so if we weren't in one
                    // we may not be in one now
                    if !matches!(transaction_status, TransactionManagerStatus::Valid(valid_tm) if valid_tm.transaction_depth().is_some())
                    {
                        // -> transaction manager is broken
                        transaction_status.set_in_error()
                    }
                } else {
                    // If transaction was InError before, but now it's not (because we attempted
                    // a rollback), we may pretend it's fixed because
                    // if it isn't Postgres *will* tell us again.

                    // Fun fact: if we have not received an `InTransaction` status however,
                    // postgres will *not* warn us that transaction is broken when attempting to
                    // commit, so we may think that commit has succeeded but in fact it hasn't.
                    tm.status.set_requires_rollback_maybe_up_to_top_level(false)
                }
            }
            PgTransactionStatus::Active => {
                // This is a transient state for libpq - nothing we can deduce here.
            }
        }
    }

    fn non_generic_instrumentation(
        query_result: Result<(), &Error>,
        conn: &mut ConnectionAndTransactionManager,
        source: &dyn DebugQuery,
        final_call: bool,
    ) {
        if let Err(e) = query_result {
            conn.instrumentation
                .on_connection_event(InstrumentationEvent::FinishQuery {
                    query: source,
                    error: Some(e),
                });
        } else if final_call {
            conn.instrumentation
                .on_connection_event(InstrumentationEvent::FinishQuery {
                    query: source,
                    error: None,
                });
        }
    }

    non_generic_inner(conn, query_result.is_err());
    non_generic_instrumentation(query_result.as_ref().map(|_| ()), conn, source, final_call);
    query_result
}

#[cfg(feature = "r2d2")]
impl crate::r2d2::R2D2Connection for PgConnection {
    fn ping(&mut self) -> QueryResult<()> {
        crate::r2d2::CheckConnectionQuery.execute(self).map(|_| ())
    }

    fn is_broken(&mut self) -> bool {
        AnsiTransactionManager::is_broken_transaction_manager(self)
    }
}

impl MultiConnectionHelper for PgConnection {
    fn to_any<'a>(
        lookup: &mut <Self::Backend as crate::sql_types::TypeMetadata>::MetadataLookup,
    ) -> &mut (dyn std::any::Any + 'a) {
        lookup.as_any()
    }

    fn from_any(
        lookup: &mut dyn std::any::Any,
    ) -> Option<&mut <Self::Backend as crate::sql_types::TypeMetadata>::MetadataLookup> {
        lookup
            .downcast_mut::<Self>()
            .map(|conn| conn as &mut dyn super::PgMetadataLookup)
    }
}

impl PgConnection {
    /// Build a transaction, specifying additional details such as isolation level
    ///
    /// See [`TransactionBuilder`] for more examples.
    ///
    /// [`TransactionBuilder`]: crate::pg::TransactionBuilder
    ///
    /// ```rust
    /// # include!("../../doctest_setup.rs");
    /// #
    /// # fn main() {
    /// #     run_test().unwrap();
    /// # }
    /// #
    /// # fn run_test() -> QueryResult<()> {
    /// #     use schema::users::dsl::*;
    /// #     let conn = &mut connection_no_transaction();
    /// conn.build_transaction()
    ///     .read_only()
    ///     .serializable()
    ///     .deferrable()
    ///     .run(|conn| Ok(()))
    /// # }
    /// ```
    pub fn build_transaction(&mut self) -> TransactionBuilder<'_, Self> {
        TransactionBuilder::new(self)
    }

    pub(crate) fn copy_from<S, T>(&mut self, target: S) -> Result<usize, S::Error>
    where
        S: CopyFromExpression<T>,
    {
        let query = InternalCopyFromQuery::new(target);
        let res = self.with_prepared_query(query, false, |stmt, binds, conn, mut source| {
            fn inner_copy_in<S, T>(
                stmt: MaybeCached<'_, Statement>,
                conn: &mut ConnectionAndTransactionManager,
                binds: Vec<Option<Vec<u8>>>,
                source: &mut InternalCopyFromQuery<S, T>,
            ) -> Result<usize, S::Error>
            where
                S: CopyFromExpression<T>,
            {
                let _res = stmt.execute(&mut conn.raw_connection, &binds, false)?;
                let mut copy_in = CopyFromSink::new(&mut conn.raw_connection);
                let r = source.target.callback(&mut copy_in);
                copy_in.finish(r.as_ref().err().map(|e| e.to_string()))?;
                let next_res = conn.raw_connection.get_next_result()?.ok_or_else(|| {
                    crate::result::Error::DeserializationError(
                        "Failed to receive result from the database".into(),
                    )
                })?;
                let rows = next_res.rows_affected();
                while let Some(_r) = conn.raw_connection.get_next_result()? {}
                r?;
                Ok(rows)
            }

            let rows = inner_copy_in(stmt, conn, binds, &mut source);
            if let Err(ref e) = rows {
                let database_error = crate::result::Error::DatabaseError(
                    crate::result::DatabaseErrorKind::Unknown,
                    Box::new(e.to_string()),
                );
                conn.instrumentation
                    .on_connection_event(InstrumentationEvent::FinishQuery {
                        query: &crate::debug_query(&source),
                        error: Some(&database_error),
                    });
            } else {
                conn.instrumentation
                    .on_connection_event(InstrumentationEvent::FinishQuery {
                        query: &crate::debug_query(&source),
                        error: None,
                    });
            }

            rows
        })?;

        Ok(res)
    }

    pub(crate) fn copy_to<T>(&mut self, command: CopyToCommand<T>) -> QueryResult<CopyToBuffer<'_>>
    where
        T: CopyTarget,
    {
        let res = self.with_prepared_query::<_, _, Error>(
            command,
            false,
            |stmt, binds, conn, source| {
                let res = stmt.execute(&mut conn.raw_connection, &binds, false);
                conn.instrumentation
                    .on_connection_event(InstrumentationEvent::FinishQuery {
                        query: &crate::debug_query(&source),
                        error: res.as_ref().err(),
                    });
                Ok(CopyToBuffer::new(&mut conn.raw_connection, res?))
            },
        )?;
        Ok(res)
    }

    fn with_prepared_query<'conn, T, R, E>(
        &'conn mut self,
        source: T,
        execute_returning_count: bool,
        f: impl FnOnce(
            MaybeCached<'_, Statement>,
            Vec<Option<Vec<u8>>>,
            &'conn mut ConnectionAndTransactionManager,
            T,
        ) -> Result<R, E>,
    ) -> Result<R, E>
    where
        T: QueryFragment<Pg> + QueryId,
        E: From<crate::result::Error>,
    {
        self.connection_and_transaction_manager
            .instrumentation
            .on_connection_event(InstrumentationEvent::StartQuery {
                query: &crate::debug_query(&source),
            });
        let mut bind_collector = RawBytesBindCollector::<Pg>::new();
        source.collect_binds(&mut bind_collector, self, &Pg)?;
        let binds = bind_collector.binds;
        let metadata = bind_collector.metadata;

        let cache = &mut self.statement_cache;
        let conn = &mut self.connection_and_transaction_manager.raw_connection;
        let query = cache.cached_statement(
            &source,
            &Pg,
            &metadata,
            conn,
            Statement::prepare,
            &mut *self.connection_and_transaction_manager.instrumentation,
        );
        if !execute_returning_count {
            if let Err(ref e) = query {
                self.connection_and_transaction_manager
                    .instrumentation
                    .on_connection_event(InstrumentationEvent::FinishQuery {
                        query: &crate::debug_query(&source),
                        error: Some(e),
                    });
            }
        }

        f(
            query?,
            binds,
            &mut self.connection_and_transaction_manager,
            source,
        )
    }

    fn set_config_options(&mut self) -> QueryResult<()> {
        crate::sql_query("SET TIME ZONE 'UTC'").execute(self)?;
        crate::sql_query("SET CLIENT_ENCODING TO 'UTF8'").execute(self)?;
        self.connection_and_transaction_manager
            .raw_connection
            .set_notice_processor(noop_notice_processor);
        Ok(())
    }

    /// See Postgres documentation for SQL commands [NOTIFY][] and [LISTEN][]
    ///
    /// The returned iterator can yield items even after a None value when new notifications have been received.
    /// The iterator can be polled again after a `None` value was received as new notifications might have
    /// been send in the mean time.
    ///
    /// [NOTIFY]: https://www.postgresql.org/docs/current/sql-notify.html
    /// [LISTEN]: https://www.postgresql.org/docs/current/sql-listen.html
    ///
    /// ## Example
    ///
    /// ```
    /// # include!("../../doctest_setup.rs");
    /// #
    /// # fn main() {
    /// #     run_test().unwrap();
    /// # }
    /// #
    /// # fn run_test() -> QueryResult<()> {
    /// #     let connection = &mut establish_connection();
    ///
    /// // register the notifications channel we want to receive notifications for
    /// diesel::sql_query("LISTEN example_channel").execute(connection)?;
    /// // send some notification
    /// // this is usually done from a different connection/thread/application
    /// diesel::sql_query("NOTIFY example_channel, 'additional data'").execute(connection)?;
    ///
    /// for result in connection.notifications_iter() {
    ///     let notification = result.unwrap();
    ///     assert_eq!(notification.channel, "example_channel");
    ///     assert_eq!(notification.payload, "additional data");
    ///
    ///     println!(
    ///         "Notification received from server process with id {}.",
    ///         notification.process_id
    ///     );
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn notifications_iter(&mut self) -> impl Iterator<Item = QueryResult<PgNotification>> + '_ {
        let conn = &self.connection_and_transaction_manager.raw_connection;
        std::iter::from_fn(move || conn.pq_notifies().transpose())
    }
}

extern "C" fn noop_notice_processor(_: *mut libc::c_void, _message: *const libc::c_char) {}

mod private {
    use super::*;

    #[allow(missing_debug_implementations)]
    pub struct ConnectionAndTransactionManager {
        pub(super) raw_connection: RawConnection,
        pub(super) transaction_state: AnsiTransactionManager,
        pub(super) instrumentation: DynInstrumentation,
    }

    pub trait PgLoadingMode<B> {
        const USE_ROW_BY_ROW_MODE: bool;
        type Cursor<'conn, 'query>: Iterator<Item = QueryResult<Self::Row<'conn, 'query>>>;
        type Row<'conn, 'query>: crate::row::Row<'conn, Pg>;

        fn get_cursor<'conn, 'query>(
            raw_connection: &'conn mut ConnectionAndTransactionManager,
            result: PgResult,
            source: impl QueryFragment<Pg> + 'query,
        ) -> QueryResult<Self::Cursor<'conn, 'query>>;
    }

    impl PgLoadingMode<DefaultLoadingMode> for PgConnection {
        const USE_ROW_BY_ROW_MODE: bool = false;
        type Cursor<'conn, 'query> = Cursor;
        type Row<'conn, 'query> = self::row::PgRow;

        fn get_cursor<'conn, 'query>(
            conn: &'conn mut ConnectionAndTransactionManager,
            result: PgResult,
            source: impl QueryFragment<Pg> + 'query,
        ) -> QueryResult<Self::Cursor<'conn, 'query>> {
            update_transaction_manager_status(
                Cursor::new(result, &mut conn.raw_connection),
                conn,
                &crate::debug_query(&source),
                true,
            )
        }
    }

    impl PgLoadingMode<PgRowByRowLoadingMode> for PgConnection {
        const USE_ROW_BY_ROW_MODE: bool = true;
        type Cursor<'conn, 'query> = RowByRowCursor<'conn, 'query>;
        type Row<'conn, 'query> = self::row::PgRow;

        fn get_cursor<'conn, 'query>(
            raw_connection: &'conn mut ConnectionAndTransactionManager,
            result: PgResult,
            source: impl QueryFragment<Pg> + 'query,
        ) -> QueryResult<Self::Cursor<'conn, 'query>> {
            Ok(RowByRowCursor::new(
                result,
                raw_connection,
                Box::new(source),
            ))
        }
    }
}

#[cfg(test)]
// that's a false positive for `panic!`/`assert!` on rust 2018
#[allow(clippy::uninlined_format_args)]
mod tests {
    extern crate dotenvy;

    use super::*;
    use crate::prelude::*;
    use crate::result::Error::DatabaseError;
    use std::num::NonZeroU32;

    fn connection() -> PgConnection {
        crate::test_helpers::pg_connection_no_transaction()
    }

    #[diesel_test_helper::test]
    fn notifications_arrive() {
        use crate::sql_query;

        let conn = &mut connection();
        sql_query("LISTEN test_notifications")
            .execute(conn)
            .unwrap();
        sql_query("NOTIFY test_notifications, 'first'")
            .execute(conn)
            .unwrap();
        sql_query("NOTIFY test_notifications, 'second'")
            .execute(conn)
            .unwrap();

        let notifications = conn
            .notifications_iter()
            .map(Result::unwrap)
            .collect::<Vec<_>>();

        assert_eq!(2, notifications.len());
        assert_eq!(notifications[0].channel, "test_notifications");
        assert_eq!(notifications[1].channel, "test_notifications");
        assert_eq!(notifications[0].payload, "first");
        assert_eq!(notifications[1].payload, "second");

        let next_notification = conn.notifications_iter().next();
        assert!(
            next_notification.is_none(),
            "Got a next notification, while not expecting one: {next_notification:?}"
        );

        sql_query("NOTIFY test_notifications")
            .execute(conn)
            .unwrap();
        assert_eq!(
            conn.notifications_iter().next().unwrap().unwrap().payload,
            ""
        );
    }

    #[diesel_test_helper::test]
    fn malformed_sql_query() {
        let connection = &mut connection();
        let query =
            crate::sql_query("SELECT not_existent FROM also_not_there;").execute(connection);

        if let Err(DatabaseError(_, string)) = query {
            assert_eq!(Some(26), string.statement_position());
        } else {
            unreachable!();
        }
    }

    table! {
        users {
            id -> Integer,
            name -> Text,
        }
    }

    #[diesel_test_helper::test]
    fn transaction_manager_returns_an_error_when_attempting_to_commit_outside_of_a_transaction() {
        use crate::connection::{AnsiTransactionManager, TransactionManager};
        use crate::result::Error;
        use crate::PgConnection;

        let conn = &mut crate::test_helpers::pg_connection_no_transaction();
        assert_eq!(
            None,
            <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                conn
            ).transaction_depth().expect("Transaction depth")
        );
        let result = AnsiTransactionManager::commit_transaction(conn);
        assert!(matches!(result, Err(Error::NotInTransaction)))
    }

    #[diesel_test_helper::test]
    fn transaction_manager_returns_an_error_when_attempting_to_rollback_outside_of_a_transaction() {
        use crate::connection::{AnsiTransactionManager, TransactionManager};
        use crate::result::Error;
        use crate::PgConnection;

        let conn = &mut crate::test_helpers::pg_connection_no_transaction();
        assert_eq!(
            None,
            <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                conn
            ).transaction_depth().expect("Transaction depth")
        );
        let result = AnsiTransactionManager::rollback_transaction(conn);
        assert!(matches!(result, Err(Error::NotInTransaction)))
    }

    #[diesel_test_helper::test]
    fn postgres_transaction_is_rolled_back_upon_syntax_error() {
        use std::num::NonZeroU32;

        use crate::connection::{AnsiTransactionManager, TransactionManager};
        use crate::pg::connection::raw::PgTransactionStatus;
        use crate::*;
        let conn = &mut crate::test_helpers::pg_connection_no_transaction();
        assert_eq!(
            None,
            <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                conn
            ).transaction_depth().expect("Transaction depth")
        );
        let _result = conn.build_transaction().run(|conn| {
            assert_eq!(
                NonZeroU32::new(1),
                <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                    conn
                ).transaction_depth().expect("Transaction depth")
            );
            // In Postgres, a syntax error breaks the transaction block
            let query_result = sql_query("SELECT_SYNTAX_ERROR 1").execute(conn);
            assert!(query_result.is_err());
            assert_eq!(
                PgTransactionStatus::InError,
                conn.connection_and_transaction_manager.raw_connection.transaction_status()
            );
            query_result
        });
        assert_eq!(
            None,
            <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                conn
            ).transaction_depth().expect("Transaction depth")
        );
        assert_eq!(
            PgTransactionStatus::Idle,
            conn.connection_and_transaction_manager
                .raw_connection
                .transaction_status()
        );
    }

    #[diesel_test_helper::test]
    fn nested_postgres_transaction_is_rolled_back_upon_syntax_error() {
        use std::num::NonZeroU32;

        use crate::connection::{AnsiTransactionManager, TransactionManager};
        use crate::pg::connection::raw::PgTransactionStatus;
        use crate::*;
        let conn = &mut crate::test_helpers::pg_connection_no_transaction();
        assert_eq!(
            None,
            <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                conn
            ).transaction_depth().expect("Transaction depth")
        );
        let result = conn.build_transaction().run(|conn| {
            assert_eq!(
                NonZeroU32::new(1),
                <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                    conn
            ).transaction_depth().expect("Transaction depth")
            );
            let result = conn.build_transaction().run(|conn| {
                assert_eq!(
                    NonZeroU32::new(2),
                    <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                        conn
            ).transaction_depth().expect("Transaction depth")
                );
                sql_query("SELECT_SYNTAX_ERROR 1").execute(conn)
            });
            assert!(result.is_err());
            assert_eq!(
                NonZeroU32::new(1),
                <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                    conn
            ).transaction_depth().expect("Transaction depth")
            );
            let query_result = sql_query("SELECT 1").execute(conn);
            assert!(query_result.is_ok());
            assert_eq!(
                PgTransactionStatus::InTransaction,
                conn.connection_and_transaction_manager.raw_connection.transaction_status()
            );
            query_result
        });
        assert!(result.is_ok());
        assert_eq!(
            PgTransactionStatus::Idle,
            conn.connection_and_transaction_manager
                .raw_connection
                .transaction_status()
        );
        assert_eq!(
            None,
            <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                conn
            ).transaction_depth().expect("Transaction depth")
        );
    }

    #[diesel_test_helper::test]
    // This function uses collect with an side effect (spawning threads)
    // so this is a false positive from clippy
    #[allow(clippy::needless_collect)]
    fn postgres_transaction_depth_is_tracked_properly_on_serialization_failure() {
        use crate::pg::connection::raw::PgTransactionStatus;
        use crate::result::DatabaseErrorKind::SerializationFailure;
        use crate::result::Error::DatabaseError;
        use crate::*;
        use std::sync::{Arc, Barrier};
        use std::thread;

        table! {
            #[sql_name = "pg_transaction_depth_is_tracked_properly_on_commit_failure"]
            serialization_example {
                id -> Serial,
                class -> Integer,
            }
        }

        let conn = &mut crate::test_helpers::pg_connection_no_transaction();

        sql_query(
            "DROP TABLE IF EXISTS pg_transaction_depth_is_tracked_properly_on_commit_failure;",
        )
        .execute(conn)
        .unwrap();
        sql_query(
            r#"
            CREATE TABLE pg_transaction_depth_is_tracked_properly_on_commit_failure (
                id SERIAL PRIMARY KEY,
                class INTEGER NOT NULL
            )
        "#,
        )
        .execute(conn)
        .unwrap();

        insert_into(serialization_example::table)
            .values(&vec![
                serialization_example::class.eq(1),
                serialization_example::class.eq(2),
            ])
            .execute(conn)
            .unwrap();

        let before_barrier = Arc::new(Barrier::new(2));
        let after_barrier = Arc::new(Barrier::new(2));
        let threads = (1..3)
            .map(|i| {
                let before_barrier = before_barrier.clone();
                let after_barrier = after_barrier.clone();
                thread::spawn(move || {
                    use crate::connection::AnsiTransactionManager;
                    use crate::connection::TransactionManager;
                    let conn = &mut crate::test_helpers::pg_connection_no_transaction();
                    assert_eq!(None, <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(conn).transaction_depth().expect("Transaction depth"));

                    let result = conn.build_transaction().serializable().run(|conn| {
                        assert_eq!(NonZeroU32::new(1), <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(conn).transaction_depth().expect("Transaction depth"));

                        let _ = serialization_example::table
                            .filter(serialization_example::class.eq(i))
                            .count()
                            .execute(conn)?;

                        let other_i = if i == 1 { 2 } else { 1 };
                        let q = insert_into(serialization_example::table)
                            .values(serialization_example::class.eq(other_i));
                        before_barrier.wait();

                        let r = q.execute(conn);
                        after_barrier.wait();
                        r
                    });
                    assert_eq!(
                        PgTransactionStatus::Idle,
                        conn.connection_and_transaction_manager.raw_connection.transaction_status()
                    );

                    assert_eq!(None, <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(conn).transaction_depth().expect("Transaction depth"));
                    result
                })
            })
            .collect::<Vec<_>>();

        let mut results = threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect::<Vec<_>>();

        results.sort_by_key(|r| r.is_err());

        assert!(results[0].is_ok(), "Got {:?} instead", results);
        assert!(
            matches!(&results[1], Err(DatabaseError(SerializationFailure, _))),
            "Got {:?} instead",
            results
        );
        assert_eq!(
            PgTransactionStatus::Idle,
            conn.connection_and_transaction_manager
                .raw_connection
                .transaction_status()
        );
    }

    #[diesel_test_helper::test]
    // This function uses collect with an side effect (spawning threads)
    // so this is a false positive from clippy
    #[allow(clippy::needless_collect)]
    fn postgres_transaction_depth_is_tracked_properly_on_nested_serialization_failure() {
        use crate::pg::connection::raw::PgTransactionStatus;
        use crate::result::DatabaseErrorKind::SerializationFailure;
        use crate::result::Error::DatabaseError;
        use crate::*;
        use std::sync::{Arc, Barrier};
        use std::thread;

        table! {
            #[sql_name = "pg_nested_transaction_depth_is_tracked_properly_on_commit_failure"]
            serialization_example {
                id -> Serial,
                class -> Integer,
            }
        }

        let conn = &mut crate::test_helpers::pg_connection_no_transaction();

        sql_query(
            "DROP TABLE IF EXISTS pg_nested_transaction_depth_is_tracked_properly_on_commit_failure;",
        )
        .execute(conn)
        .unwrap();
        sql_query(
            r#"
            CREATE TABLE pg_nested_transaction_depth_is_tracked_properly_on_commit_failure (
                id SERIAL PRIMARY KEY,
                class INTEGER NOT NULL
            )
        "#,
        )
        .execute(conn)
        .unwrap();

        insert_into(serialization_example::table)
            .values(&vec![
                serialization_example::class.eq(1),
                serialization_example::class.eq(2),
            ])
            .execute(conn)
            .unwrap();

        let before_barrier = Arc::new(Barrier::new(2));
        let after_barrier = Arc::new(Barrier::new(2));
        let threads = (1..3)
            .map(|i| {
                let before_barrier = before_barrier.clone();
                let after_barrier = after_barrier.clone();
                thread::spawn(move || {
                    use crate::connection::AnsiTransactionManager;
                    use crate::connection::TransactionManager;
                    let conn = &mut crate::test_helpers::pg_connection_no_transaction();
                    assert_eq!(None, <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(conn).transaction_depth().expect("Transaction depth"));

                    let result = conn.build_transaction().serializable().run(|conn| {
                        assert_eq!(NonZeroU32::new(1), <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(conn).transaction_depth().expect("Transaction depth"));
                        let r = conn.transaction(|conn| {
                            assert_eq!(NonZeroU32::new(2), <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(conn).transaction_depth().expect("Transaction depth"));

                            let _ = serialization_example::table
                                .filter(serialization_example::class.eq(i))
                                .count()
                                .execute(conn)?;

                            let other_i = if i == 1 { 2 } else { 1 };
                            let q = insert_into(serialization_example::table)
                                .values(serialization_example::class.eq(other_i));
                            before_barrier.wait();

                            let r = q.execute(conn);
                            after_barrier.wait();
                            r
                        });
                        assert_eq!(NonZeroU32::new(1), <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(conn).transaction_depth().expect("Transaction depth"));
                        assert_eq!(
                            PgTransactionStatus::InTransaction,
                            conn.connection_and_transaction_manager.raw_connection.transaction_status()
                        );
                        r
                    });
                    assert_eq!(
                        PgTransactionStatus::Idle,
                        conn.connection_and_transaction_manager.raw_connection.transaction_status()
                    );

                    assert_eq!(None, <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(conn).transaction_depth().expect("Transaction depth"));
                    result
                })
            })
            .collect::<Vec<_>>();

        let mut results = threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect::<Vec<_>>();

        results.sort_by_key(|r| r.is_err());

        assert!(results[0].is_ok(), "Got {:?} instead", results);
        assert!(
            matches!(&results[1], Err(DatabaseError(SerializationFailure, _))),
            "Got {:?} instead",
            results
        );
        assert_eq!(
            PgTransactionStatus::Idle,
            conn.connection_and_transaction_manager
                .raw_connection
                .transaction_status()
        );
    }

    #[diesel_test_helper::test]
    fn postgres_transaction_is_rolled_back_upon_deferred_constraint_failure() {
        use crate::connection::{AnsiTransactionManager, TransactionManager};
        use crate::pg::connection::raw::PgTransactionStatus;
        use crate::result::Error;
        use crate::*;

        let conn = &mut crate::test_helpers::pg_connection_no_transaction();
        assert_eq!(
            None,
            <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                conn
            ).transaction_depth().expect("Transaction depth")
        );
        let result: Result<_, Error> = conn.build_transaction().run(|conn| {
            assert_eq!(
                NonZeroU32::new(1),
                <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                    conn
            ).transaction_depth().expect("Transaction depth")
            );
            sql_query("DROP TABLE IF EXISTS deferred_constraint_commit").execute(conn)?;
            sql_query("CREATE TABLE deferred_constraint_commit(id INT UNIQUE INITIALLY DEFERRED)")
                .execute(conn)?;
            sql_query("INSERT INTO deferred_constraint_commit VALUES(1)").execute(conn)?;
            let result =
                sql_query("INSERT INTO deferred_constraint_commit VALUES(1)").execute(conn);
            assert!(result.is_ok());
            assert_eq!(
                PgTransactionStatus::InTransaction,
                conn.connection_and_transaction_manager.raw_connection.transaction_status()
            );
            Ok(())
        });
        assert_eq!(
            None,
            <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                conn
            ).transaction_depth().expect("Transaction depth")
        );
        assert_eq!(
            PgTransactionStatus::Idle,
            conn.connection_and_transaction_manager
                .raw_connection
                .transaction_status()
        );
        assert!(result.is_err());
    }

    #[diesel_test_helper::test]
    fn postgres_transaction_is_rolled_back_upon_deferred_trigger_failure() {
        use crate::connection::{AnsiTransactionManager, TransactionManager};
        use crate::pg::connection::raw::PgTransactionStatus;
        use crate::result::Error;
        use crate::*;

        let conn = &mut crate::test_helpers::pg_connection_no_transaction();
        assert_eq!(
            None,
            <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                conn
            ).transaction_depth().expect("Transaction depth")
        );
        let result: Result<_, Error> = conn.build_transaction().run(|conn| {
            assert_eq!(
                NonZeroU32::new(1),
                <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                    conn
            ).transaction_depth().expect("Transaction depth")
            );
            sql_query("DROP TABLE IF EXISTS deferred_trigger_commit").execute(conn)?;
            sql_query("CREATE TABLE deferred_trigger_commit(id INT UNIQUE INITIALLY DEFERRED)")
                .execute(conn)?;
            sql_query(
                r#"
                    CREATE OR REPLACE FUNCTION transaction_depth_blow_up()
                        RETURNS trigger
                        LANGUAGE plpgsql
                        AS $$
                    DECLARE
                    BEGIN
                        IF NEW.value = 42 THEN
                            RAISE EXCEPTION 'Transaction kaboom';
                        END IF;
                    RETURN NEW;

                    END;$$;
                "#,
            )
            .execute(conn)?;

            sql_query(
                r#"
                    CREATE CONSTRAINT TRIGGER transaction_depth_trigger
                        AFTER INSERT ON "deferred_trigger_commit"
                        DEFERRABLE INITIALLY DEFERRED
                        FOR EACH ROW
                        EXECUTE PROCEDURE transaction_depth_blow_up()
            "#,
            )
            .execute(conn)?;
            let result = sql_query("INSERT INTO deferred_trigger_commit VALUES(42)").execute(conn);
            assert!(result.is_ok());
            assert_eq!(
                PgTransactionStatus::InTransaction,
                conn.connection_and_transaction_manager.raw_connection.transaction_status()
            );
            Ok(())
        });
        assert_eq!(
            None,
            <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                conn
            ).transaction_depth().expect("Transaction depth")
        );
        assert_eq!(
            PgTransactionStatus::Idle,
            conn.connection_and_transaction_manager
                .raw_connection
                .transaction_status()
        );
        assert!(result.is_err());
    }

    #[diesel_test_helper::test]
    fn nested_postgres_transaction_is_rolled_back_upon_deferred_trigger_failure() {
        use crate::connection::{AnsiTransactionManager, TransactionManager};
        use crate::pg::connection::raw::PgTransactionStatus;
        use crate::result::Error;
        use crate::*;

        let conn = &mut crate::test_helpers::pg_connection_no_transaction();
        assert_eq!(
            None,
            <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                conn
            ).transaction_depth().expect("Transaction depth")
        );
        let result: Result<_, Error> = conn.build_transaction().run(|conn| {
            assert_eq!(
                NonZeroU32::new(1),
                <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                    conn
            ).transaction_depth().expect("Transaction depth")
            );
            sql_query("DROP TABLE IF EXISTS deferred_trigger_nested_commit").execute(conn)?;
            sql_query(
                "CREATE TABLE deferred_trigger_nested_commit(id INT UNIQUE INITIALLY DEFERRED)",
            )
            .execute(conn)?;
            sql_query(
                r#"
                    CREATE OR REPLACE FUNCTION transaction_depth_blow_up()
                        RETURNS trigger
                        LANGUAGE plpgsql
                        AS $$
                    DECLARE
                    BEGIN
                        IF NEW.value = 42 THEN
                            RAISE EXCEPTION 'Transaction kaboom';
                        END IF;
                    RETURN NEW;

                    END;$$;
                "#,
            )
            .execute(conn)?;

            sql_query(
                r#"
                    CREATE CONSTRAINT TRIGGER transaction_depth_trigger
                        AFTER INSERT ON "deferred_trigger_nested_commit"
                        DEFERRABLE INITIALLY DEFERRED
                        FOR EACH ROW
                        EXECUTE PROCEDURE transaction_depth_blow_up()
            "#,
            )
            .execute(conn)?;
            let inner_result: Result<_, Error> = conn.build_transaction().run(|conn| {
                let result = sql_query("INSERT INTO deferred_trigger_nested_commit VALUES(42)")
                    .execute(conn);
                assert!(result.is_ok());
                Ok(())
            });
            assert!(inner_result.is_err());
            assert_eq!(
                PgTransactionStatus::InTransaction,
                conn.connection_and_transaction_manager.raw_connection.transaction_status()
            );
            Ok(())
        });
        assert_eq!(
            None,
            <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                conn
            ).transaction_depth().expect("Transaction depth")
        );
        assert_eq!(
            PgTransactionStatus::Idle,
            conn.connection_and_transaction_manager
                .raw_connection
                .transaction_status()
        );
        assert!(result.is_ok(), "Expected success, got {:?}", result);
    }

    #[diesel_test_helper::test]
    fn nested_postgres_transaction_is_rolled_back_upon_deferred_constraint_failure() {
        use crate::connection::{AnsiTransactionManager, TransactionManager};
        use crate::pg::connection::raw::PgTransactionStatus;
        use crate::result::Error;
        use crate::*;

        let conn = &mut crate::test_helpers::pg_connection_no_transaction();
        assert_eq!(
            None,
            <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                conn
            ).transaction_depth().expect("Transaction depth")
        );
        let result: Result<_, Error> = conn.build_transaction().run(|conn| {
            assert_eq!(
                NonZeroU32::new(1),
                <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                    conn
            ).transaction_depth().expect("Transaction depth")
            );
            sql_query("DROP TABLE IF EXISTS deferred_constraint_nested_commit").execute(conn)?;
            sql_query("CREATE TABLE deferred_constraint_nested_commit(id INT UNIQUE INITIALLY DEFERRED)").execute(conn)?;
            let inner_result: Result<_, Error> = conn.build_transaction().run(|conn| {
                assert_eq!(
                    NonZeroU32::new(2),
                    <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                        conn
                    ).transaction_depth().expect("Transaction depth")
                );
                sql_query("INSERT INTO deferred_constraint_nested_commit VALUES(1)").execute(conn)?;
                let result = sql_query("INSERT INTO deferred_constraint_nested_commit VALUES(1)").execute(conn);
                assert!(result.is_ok());
                Ok(())
            });
            assert!(inner_result.is_err());
            assert_eq!(
                PgTransactionStatus::InTransaction,
                conn.connection_and_transaction_manager.raw_connection.transaction_status()
            );
            assert_eq!(
                NonZeroU32::new(1),
                <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                    conn
            ).transaction_depth().expect("Transaction depth")
            );
            sql_query("INSERT INTO deferred_constraint_nested_commit VALUES(1)").execute(conn)
        });
        assert_eq!(
            None,
            <AnsiTransactionManager as TransactionManager<PgConnection>>::transaction_manager_status_mut(
                conn
            ).transaction_depth().expect("Transaction depth")
        );
        assert_eq!(
            PgTransactionStatus::Idle,
            conn.connection_and_transaction_manager
                .raw_connection
                .transaction_status()
        );
        assert!(result.is_ok());
    }
}
