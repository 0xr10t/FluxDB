use crate::engine::Engine;
use common::{EngineError, Key, Value};
use db_core::transaction::Transaction;

// Used a reference instead of Arc as reference enforces that transaction does not outlive the engine. 
pub struct TxnHandle<'e, K:Key, V:Value> {
    pub(crate) engine:&'e Engine<K,V>,
    pub(crate) txn: Option<Transaction>,
    pub(crate) poisoned: bool, 
}

impl<K, V> Engine<K, V>
where
    K: Key,
    V: Value,
{
    // this function is called once inserts/get/update/delete finishes and returns from index.rs
    pub(crate) fn commit(&self, txn: Transaction) {
        if !txn.wrote_anything() {
            self.transaction_manager.mark_committed(txn.txn_id);
            return;
        }
        // take the shared lock to prevent checkpoint from running
        // append commit log to wal
        // fsync upto this commit_lsn
        // call tm.mark_committed()
        self.transaction_manager.mark_committed(txn.txn_id);
        // release the lock
    }

    // this function is called once inserts/get/update/delete finishes and returns from index.rs
    pub(crate) fn abort(&self, txn: Transaction) {
        if !txn.wrote_anything() {
            self.transaction_manager.mark_aborted(txn.txn_id);
            return;
        }
        // take the shared lock to prevent checkpoint from running
        // append abort log to wal
        // no need to fsync aborts
        // call tm.mark_aborted()
        self.transaction_manager.mark_aborted(txn.txn_id);
        // release the lock
    }
}

impl<K,V> TxnHandle<'_, K,V> 
where 
    K: Key,
    V: Value
{
    pub fn commit(mut self) -> Result<(), EngineError> {
        if self.poisoned {};
        if let Some(txn) = self.txn.take() {
            self.engine.commit(txn); 
        }
        Ok(())
    }
}

impl<K: Key, V: Value> Drop for TxnHandle<'_, K, V> {
    fn drop(&mut self) {
        if let Some(txn) = self.txn.take() {  // only way to reach this path is if no one commits so txn still has a value. 
            self.engine.abort(txn); // abort the transaction is no one commits 
        }
    }
  }