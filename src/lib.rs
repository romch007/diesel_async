#![cfg_attr(doc_cfg, feature(doc_cfg, doc_auto_cfg))]
//! Diesel-async provides async variants of diesel related query functionality
//!
//! diesel-async is an extension to diesel itself. It is designed to be used together
//! with the main diesel crate. It only provides async variants of core diesel traits,
//! that perform actual io-work.
//! This includes async counterparts the following traits:
//! * [`diesel::prelude::RunQueryDsl`](https://docs.diesel.rs/2.0.x/diesel/prelude/trait.RunQueryDsl.html)
//!    -> [`diesel_async::RunQueryDsl`](crate::RunQueryDsl)
//! * [`diesel::connection::Connection`](https://docs.diesel.rs/2.0.x/diesel/connection/trait.Connection.html)
//!    -> [`diesel_async::AsyncConnection`](crate::AsyncConnection)
//! * [`diesel::query_dsl::UpdateAndFetchResults`](https://docs.diesel.rs/2.0.x/diesel/query_dsl/trait.UpdateAndFetchResults.html)
//!    -> [`diesel_async::UpdateAndFetchResults`](crate::UpdateAndFetchResults)
//!
//! These traits closely mirror their diesel counter parts while providing async functionality.
//!
//! In addition to these core traits 2 fully async connection implementations are provided
//! by diesel-async:
//!
//! * [`AsyncMysqlConnection`] (enabled by the `mysql` feature)
//! * [`AsyncPgConnection`] (enabled by the `postgres` feature)
//!
//! Ordinary usage of `diesel-async` assumes that you just replace the corresponding sync trait
//! method calls and connections with their async counterparts.
//!
//! ```rust
//! # include!("./doctest_setup.rs");
//! #
//! use diesel::prelude::*;
//! use diesel_async::{RunQueryDsl, AsyncConnection};
//!
//! diesel::table! {
//!    users(id) {
//!        id -> Integer,
//!        name -> Text,
//!    }
//! }
//! #
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() {
//! #     run_test().await;
//! # }
//! #
//! # async fn run_test() -> QueryResult<()> {
//!
//! use crate::users::dsl::*;
//!
//! # let mut connection = establish_connection().await;
//! # /*
//! let mut connection = AsyncPgConnection::establish(std::env::var("DATABASE_URL")?).await?;
//! # */
//! let data = users
//!     // use ordinary diesel query dsl here
//!     .filter(id.gt(0))
//!     // execute the query via the provided
//!     // async variant of `diesel_async::RunQueryDsl`
//!     .load::<(i32, String)>(&mut connection)
//!     .await?;
//! let expected_data = vec![
//!     (1, String::from("Sean")),
//!     (2, String::from("Tess")),
//! ];
//! assert_eq!(expected_data, data);
//! #     Ok(())
//! # }
//! ```

#![warn(missing_docs)]
use diesel::backend::Backend;
use diesel::query_builder::{AsQuery, QueryFragment, QueryId};
use diesel::row::Row;
use diesel::{ConnectionResult, QueryResult};
use futures::{Future, Stream};

pub use scoped_futures;
use scoped_futures::ScopedBoxFuture;

#[cfg(feature = "mysql")]
mod mysql;
#[cfg(feature = "postgres")]
pub mod pg;
#[cfg(any(feature = "deadpool", feature = "bb8", feature = "mobc"))]
pub mod pooled_connection;
mod run_query_dsl;
mod stmt_cache;
mod transaction_manager;

#[cfg(feature = "mysql")]
#[doc(inline)]
pub use self::mysql::AsyncMysqlConnection;
#[cfg(feature = "postgres")]
#[doc(inline)]
pub use self::pg::AsyncPgConnection;
#[doc(inline)]
pub use self::run_query_dsl::*;

#[doc(inline)]
pub use self::transaction_manager::{AnsiTransactionManager, TransactionManager};

/// Perform simple operations on a backend.
///
/// You should likely use [`AsyncConnection`] instead.
#[async_trait::async_trait]
pub trait SimpleAsyncConnection {
    /// Execute multiple SQL statements within the same string.
    ///
    /// This function is used to execute migrations,
    /// which may contain more than one SQL statement.
    async fn batch_execute(&mut self, query: &str) -> QueryResult<()>;
}

/// This trait is a workaround to emulate GAT on stable rust
///
/// It is used to specify the return type of `AsyncConnection::load`
/// and `AsyncConnection::execute` which may contain lifetimes
pub trait AsyncConnectionGatWorkaround<'conn, 'query, DB: Backend> {
    /// The future returned by `AsyncConnection::execute`
    type ExecuteFuture: Future<Output = QueryResult<usize>> + Send;
    /// The future returned by `AsyncConnection::load`
    type LoadFuture: Future<Output = QueryResult<Self::Stream>> + Send;
    /// The inner stream returned by `AsyncConnection::load`
    type Stream: Stream<Item = QueryResult<Self::Row>> + Send;
    /// The row type used by the stream returned by `AsyncConnection::load`
    type Row: Row<'conn, DB>;
}

/// An async connection to a database
///
/// This trait represents a n async database connection. It can be used to query the database through
/// the query dsl provided by diesel, custom extensions or raw sql queries. It essentially mirrors
/// the sync diesel [`Connection`](diesel::connection::Connection) implementation
#[async_trait::async_trait]
pub trait AsyncConnection: SimpleAsyncConnection + Sized + Send
where
    for<'a, 'b> Self: AsyncConnectionGatWorkaround<'a, 'b, Self::Backend>,
{
    /// The backend this type connects to
    type Backend: Backend;

    #[doc(hidden)]
    type TransactionManager: TransactionManager<Self>;

    /// Establishes a new connection to the database
    ///
    /// The argument to this method and the method's behavior varies by backend.
    /// See the documentation for that backend's connection class
    /// for details about what it accepts and how it behaves.
    async fn establish(database_url: &str) -> ConnectionResult<Self>;

    /// Executes the given function inside of a database transaction
    ///
    /// This function executes the provided closure `f` inside a database
    /// transaction. If there is already an open transaction for the current
    /// connection savepoints will be used instead. The connection is committed if
    /// the closure returns `Ok(_)`, it will be rolled back if it returns `Err(_)`.
    /// For both cases the original result value will be returned from this function.
    ///
    /// If the transaction fails to commit due to a `SerializationFailure` or a
    /// `ReadOnlyTransaction` a rollback will be attempted.
    /// If the rollback fails, the error will be returned in a
    /// [`Error::RollbackErrorOnCommit`](diesel::result::Error::RollbackErrorOnCommit),
    /// from which you will be able to extract both the original commit error and
    /// the rollback error.
    /// In addition, the connection will be considered broken
    /// as it contains a uncommitted unabortable open transaction. Any further
    /// interaction with the transaction system will result in an returned error
    /// in this case.
    ///
    /// If the closure returns an `Err(_)` and the rollback fails the function
    /// will return that rollback error directly, and the transaction manager will
    /// be marked as broken as it contains a uncommitted unabortable open transaction.
    ///
    /// If a nested transaction fails to release the corresponding savepoint
    /// the error will be returned directly.
    ///
    /// **WARNING:** Cancling the returned future does currently **not**
    /// close an already open transaction. You may end up with a connection
    /// containing a dangling transaction.
    ///
    /// # Example
    ///
    /// ```rust
    /// # include!("doctest_setup.rs");
    /// use diesel::result::Error;
    /// use scoped_futures::ScopedFutureExt;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// #     run_test().await.unwrap();
    /// # }
    /// #
    /// # async fn run_test() -> QueryResult<()> {
    /// #     use schema::users::dsl::*;
    /// #     let conn = &mut establish_connection().await;
    /// conn.transaction::<_, Error, _>(|conn| async move {
    ///     diesel::insert_into(users)
    ///         .values(name.eq("Ruby"))
    ///         .execute(conn)
    ///         .await?;
    ///
    ///     let all_names = users.select(name).load::<String>(conn).await?;
    ///     assert_eq!(vec!["Sean", "Tess", "Ruby"], all_names);
    ///
    ///     Ok(())
    /// }.scope_boxed()).await?;
    ///
    /// conn.transaction::<(), _, _>(|conn| async move {
    ///     diesel::insert_into(users)
    ///         .values(name.eq("Pascal"))
    ///         .execute(conn)
    ///         .await?;
    ///
    ///     let all_names = users.select(name).load::<String>(conn).await?;
    ///     assert_eq!(vec!["Sean", "Tess", "Ruby", "Pascal"], all_names);
    ///
    ///     // If we want to roll back the transaction, but don't have an
    ///     // actual error to return, we can return `RollbackTransaction`.
    ///     Err(Error::RollbackTransaction)
    /// }.scope_boxed()).await;
    ///
    /// let all_names = users.select(name).load::<String>(conn).await?;
    /// assert_eq!(vec!["Sean", "Tess", "Ruby"], all_names);
    /// #     Ok(())
    /// # }
    /// ```
    async fn transaction<'a, R, E, F>(&mut self, callback: F) -> Result<R, E>
    where
        F: for<'r> FnOnce(&'r mut Self) -> ScopedBoxFuture<'a, 'r, Result<R, E>> + Send + 'a,
        E: From<diesel::result::Error> + Send + 'a,
        R: Send + 'a,
    {
        Self::TransactionManager::transaction(self, callback).await
    }

    /// Creates a transaction that will never be committed. This is useful for
    /// tests. Panics if called while inside of a transaction or
    /// if called with a connection containing a broken transaction
    async fn begin_test_transaction(&mut self) -> QueryResult<()> {
        use crate::transaction_manager::TransactionManagerStatus;

        match Self::TransactionManager::transaction_manager_status_mut(self) {
            TransactionManagerStatus::Valid(valid_status) => {
                assert_eq!(None, valid_status.transaction_depth())
            }
            TransactionManagerStatus::InError => panic!("Transaction manager in error"),
        };
        Self::TransactionManager::begin_transaction(self).await?;
        // set the test transaction flag
        // to pervent that this connection gets droped in connection pools
        // Tests commonly set the poolsize to 1 and use `begin_test_transaction`
        // to prevent modifications to the schema
        Self::TransactionManager::transaction_manager_status_mut(self).set_test_transaction_flag();
        Ok(())
    }

    #[doc(hidden)]
    fn load<'conn, 'query, T>(
        &'conn mut self,
        source: T,
    ) -> <Self as AsyncConnectionGatWorkaround<'conn, 'query, Self::Backend>>::LoadFuture
    where
        T: AsQuery + Send + 'query,
        T::Query: QueryFragment<Self::Backend> + QueryId + Send + 'query;

    #[doc(hidden)]
    fn execute_returning_count<'conn, 'query, T>(
        &'conn mut self,
        source: T,
    ) -> <Self as AsyncConnectionGatWorkaround<'conn, 'query, Self::Backend>>::ExecuteFuture
    where
        T: QueryFragment<Self::Backend> + QueryId + Send + 'query;

    #[doc(hidden)]
    fn transaction_state(
        &mut self,
    ) -> &mut <Self::TransactionManager as TransactionManager<Self>>::TransactionStateData;
}
