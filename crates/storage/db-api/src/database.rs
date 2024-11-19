use crate::{
    table::TableImporter,
    transaction::{DbTx, DbTxMut},
    DatabaseError,
};
use std::{fmt::Debug, sync::Arc};
use tracing::info;
#[track_caller]
fn log_caller_location() -> String {
    let caller = std::panic::Location::caller();
    format!("{}:{}", caller.file(), caller.line())
}
/// Main Database trait that can open read-only and read-write transactions.
///
/// Sealed trait which cannot be implemented by 3rd parties, exposed only for consumption.
pub trait Database: Send + Sync + Debug {
    /// Read-Only database transaction
    type TX: DbTx + Send + Sync + Debug + 'static;
    /// Read-Write database transaction
    type TXMut: DbTxMut + DbTx + TableImporter + Send + Sync + Debug + 'static;

    /// Create read only transaction.
    #[track_caller]
    fn tx(&self) -> Result<Self::TX, DatabaseError>;

    /// Create read write transaction only possible if database is open with write access.
    #[track_caller]
    fn tx_mut(&self) -> Result<Self::TXMut, DatabaseError>;

    /// Takes a function and passes a read-only transaction into it, making sure it's closed in the
    /// end of the execution.
    fn view<T, F>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&Self::TX) -> T,
        T: std::fmt::Debug,
    {
        let caller_location = log_caller_location();
        info!("VIEW METHOD called from {}", caller_location);
        let tx = self.tx()?;
        
        info!("transaction in view method: {:?}", tx);

        let res = f(&tx);
        info!("transaction response in view method: {:?}", res);

        tx.commit()?;

        Ok(res)
    }

    /// Takes a function and passes a write-read transaction into it, making sure it's committed in
    /// the end of the execution.
    fn update<T, F>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&Self::TXMut) -> T,
        T: std::fmt::Debug,
    {
        let caller_location = log_caller_location();
        info!("UPDATE METHOD called from {}", caller_location);
        let tx: <Self as Database>::TXMut = self.tx_mut()?;
        info!("transaction in view method: {:?}", tx);

        let res = f(&tx);
        info!("transaction response in update method: {:?}", res);

        tx.commit()?;

        Ok(res)
    }
}

impl<DB: Database> Database for Arc<DB> {
    type TX = <DB as Database>::TX;
    type TXMut = <DB as Database>::TXMut;

    fn tx(&self) -> Result<Self::TX, DatabaseError> {
        info!("tx method in db-api/src/database.rs Arc<DB>");
        <DB as Database>::tx(self)
    }

    fn tx_mut(&self) -> Result<Self::TXMut, DatabaseError> {
        info!("tx_mut method in db-api/src/database.rs Arc<DB>");
        <DB as Database>::tx_mut(self)
    }
}

impl<DB: Database> Database for &DB {
    type TX = <DB as Database>::TX;
    type TXMut = <DB as Database>::TXMut;

    fn tx(&self) -> Result<Self::TX, DatabaseError> {
        info!("tx method in db-api/src/database.rs &DB");
        <DB as Database>::tx(self)
    }

    fn tx_mut(&self) -> Result<Self::TXMut, DatabaseError> {
        info!("tx_mut method in db-api/src/database.rs &DB");
        <DB as Database>::tx_mut(self)
    }
}
