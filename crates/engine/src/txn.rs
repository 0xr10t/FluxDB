use crate::engine::Engine;
use common::{EngineError, Key, Value};
use db_core::transaction::Transaction;

// Used a reference instead of Arc as reference enforces that transaction does not outlive the engine.
pub struct TxnHandle<'e, K: Key, V: Value> {
    pub(crate) engine: &'e Engine<K, V>,
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

impl<K, V> TxnHandle<'_, K, V>
where
    K: Key,
    V: Value,
{
    pub fn commit(mut self) -> Result<(), EngineError> {
        if self.poisoned {
            return Err(EngineError::TransactionConflict); // no need to call abort here as self will get dropped and abort is called inside drop impl itself. 
        };
        if let Some(txn) = self.txn.take() {
            self.engine.commit(txn);
        }
        Ok(())
    }

    pub fn insert(
        &mut self,
        key: &K::SelfType<'_>,
        value: &V::SelfType<'_>,
    ) -> Result<(), EngineError> {
        if self.poisoned {
            return Err(EngineError::TransactionConflict);
        }
        let txn = self
            .txn
            .as_mut()
            .expect("this shouldn't be none in any possible case"); // Txn is none only on either committed or dropped 
        let res = self.engine.insert_in(txn, key, value);
        if matches!(res, Err(EngineError::TransactionConflict)) {
            self.poisoned = true; //the whole txn is dead
        }
        res
    }

    pub fn delete(&mut self, key: &K::SelfType<'_>) -> Result<(), EngineError> {
        if self.poisoned {
            return Err(EngineError::TransactionConflict);
        }
        let txn = self
            .txn
            .as_mut()
            .expect("this shouldn't be none in any possible case"); // Txn is none only on either committed or dropped 
        let res = self.engine.delete_in(txn, key);
        if matches!(res, Err(EngineError::TransactionConflict)) {
            self.poisoned = true; //the whole txn is dead
        }
        res
    }

    pub fn update(
        &mut self,
        key: &K::SelfType<'_>,
        value: &V::SelfType<'_>,
    ) -> Result<(), EngineError> {
        if self.poisoned {
            return Err(EngineError::TransactionConflict);
        }
        let txn = self
            .txn
            .as_mut()
            .expect("this shouldn't be none in any possible case"); // Txn is none only on either committed or dropped 
        let res = self.engine.update_in(txn, key, value);
        if matches!(res, Err(EngineError::TransactionConflict)) {
            self.poisoned = true; //the whole txn is dead
        }
        res
    }

    pub fn get(&mut self, key: &K::SelfType<'_>) -> Result<Option<Vec<u8>>, EngineError> {
        if self.poisoned {
            return Err(EngineError::TransactionConflict);
        }
        let txn = self
            .txn
            .as_ref()
            .expect("this shouldn't be none in any possible case"); // Txn is none only on either committed or dropped 
        let res = self.engine.get_in(txn, key);
        res
    }

    pub fn abort(self) {} // drop does the work here as well.
}

impl<K: Key, V: Value> Drop for TxnHandle<'_, K, V> {
    fn drop(&mut self) {
        if let Some(txn) = self.txn.take() {
            // only way to reach this path is if no one commits so txn still has a value.
            self.engine.abort(txn); // abort the transaction is no one commits 
        }
    }
}
