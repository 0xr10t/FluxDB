use common::{EngineError, Key, Value};
use db_core::transaction::Transaction;
use crate::engine::Engine; 
impl<K,V> Engine<K,V> 
where 
    K: Key,
    V: Value,
{
    pub(crate) fn commit(&self, txn: Transaction) {
        // take the shared lock to prevent checkpoint from running
        // append commit log to wal
        // fsync upto this commit_lsn 
        // call tm.mark_committed()
        self.transaction_manager.mark_committed(txn.txn_id);
        // release the lock
    }

    pub(crate) fn abort(&self, txn: Transaction) {
        // take the shared lock to prevent checkpoint from running
        // append abort log to wal
        // no need to fsync aborts
        // call tm.mark_aborted()
        self.transaction_manager.mark_aborted(txn.txn_id); 
        // release the lock 
    }
}
