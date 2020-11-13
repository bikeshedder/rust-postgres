use crate::codec::FrontendMessage;
use crate::connection::RequestMessages;
use crate::query::RowStream;
use crate::types::{BorrowToSql, ToSql};
use crate::{bind, query, slice_iter, Client, Error, Portal, Row, ToStatement};
use futures::TryStreamExt;
use postgres_protocol::message::frontend;

/// A representation of a PostgreSQL database transaction.
///
/// Transactions will implicitly roll back when dropped. Use the `commit` method to commit the changes made in the
/// transaction. Transactions can be nested, with inner transactions implemented via safepoints.
pub struct Transaction<'a> {
    client: &'a mut Client,
    savepoint: Option<Savepoint>,
    done: bool,
}

/// A representation of a PostgreSQL database savepoint.
pub(crate) struct Savepoint {
    pub name: String,
    pub depth: u32,
}

impl Savepoint {
    pub(crate) fn new(depth: u32, name: Option<String>) -> Self {
        Self {
            depth,
            name: name.unwrap_or_else(|| format!("sp_{}", depth)),
        }
    }
}

impl<'a> Drop for Transaction<'a> {
    fn drop(&mut self) {
        self.client.transaction_depth -= 1;

        if self.done {
            return;
        }

        let query = if let Some(sp) = self.savepoint.as_ref() {
            format!("ROLLBACK TO {}", sp.name)
        } else {
            "ROLLBACK".to_string()
        };
        let buf = self.client.inner().with_buf(|buf| {
            frontend::query(&query, buf).unwrap();
            buf.split().freeze()
        });
        let _ = self
            .client
            .inner()
            .send(RequestMessages::Single(FrontendMessage::Raw(buf)));
    }
}

impl<'a> Transaction<'a> {
    pub(crate) fn new(client: &'a mut Client) -> Transaction<'a> {
        Self::new_savepoint(client, None)
    }

    pub(crate) fn new_savepoint(
        client: &'a mut Client,
        savepoint: Option<Savepoint>,
    ) -> Transaction<'a> {
        client.transaction_depth += 1;
        Transaction {
            client,
            savepoint,
            done: false,
        }
    }

    /// Consumes the transaction, committing all changes made within it.
    pub async fn commit(mut self) -> Result<(), Error> {
        self.done = true;
        let query = if let Some(sp) = self.savepoint.as_ref() {
            format!("RELEASE {}", sp.name)
        } else {
            "COMMIT".to_string()
        };
        self.client.batch_execute(&query).await
    }

    /// Rolls the transaction back, discarding all changes made within it.
    ///
    /// This is equivalent to `Transaction`'s `Drop` implementation, but provides any error encountered to the caller.
    pub async fn rollback(mut self) -> Result<(), Error> {
        self.done = true;
        let query = if let Some(sp) = self.savepoint.as_ref() {
            format!("ROLLBACK TO {}", sp.name)
        } else {
            "ROLLBACK".to_string()
        };
        self.client.batch_execute(&query).await
    }

    /// Binds a statement to a set of parameters, creating a `Portal` which can be incrementally queried.
    ///
    /// Portals only last for the duration of the transaction in which they are created, and can only be used on the
    /// connection that created them.
    ///
    /// # Panics
    ///
    /// Panics if the number of parameters provided does not match the number expected.
    pub async fn bind<T>(
        &self,
        statement: &T,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Portal, Error>
    where
        T: ?Sized + ToStatement,
    {
        self.bind_raw(statement, slice_iter(params)).await
    }

    /// A maximally flexible version of [`bind`].
    ///
    /// [`bind`]: #method.bind
    pub async fn bind_raw<P, T, I>(&self, statement: &T, params: I) -> Result<Portal, Error>
    where
        T: ?Sized + ToStatement,
        P: BorrowToSql,
        I: IntoIterator<Item = P>,
        I::IntoIter: ExactSizeIterator,
    {
        let statement = statement.__convert().into_statement(&self.client).await?;
        bind::bind(self.client.inner(), statement, params).await
    }

    /// Continues execution of a portal, returning a stream of the resulting rows.
    ///
    /// Unlike `query`, portals can be incrementally evaluated by limiting the number of rows returned in each call to
    /// `query_portal`. If the requested number is negative or 0, all rows will be returned.
    pub async fn query_portal(&self, portal: &Portal, max_rows: i32) -> Result<Vec<Row>, Error> {
        self.query_portal_raw(portal, max_rows)
            .await?
            .try_collect()
            .await
    }

    /// The maximally flexible version of [`query_portal`].
    ///
    /// [`query_portal`]: #method.query_portal
    pub async fn query_portal_raw(
        &self,
        portal: &Portal,
        max_rows: i32,
    ) -> Result<RowStream, Error> {
        query::query_portal(self.client.inner(), portal, max_rows).await
    }

    /// Like `Client::transaction`, but creates a nested transaction via a savepoint with the specified name.
    pub async fn savepoint<I>(&mut self, name: I) -> Result<Transaction<'_>, Error>
    where
        I: Into<String>,
    {
        self.client._savepoint(Some(name.into())).await
    }
}

impl std::ops::Deref for Transaction<'_> {
    type Target = Client;
    fn deref(&self) -> &Self::Target {
        self.client
    }
}

impl std::ops::DerefMut for Transaction<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.client
    }
}
