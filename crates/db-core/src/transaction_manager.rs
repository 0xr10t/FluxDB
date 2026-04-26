use std::sync::atomic::Ordering::Relaxed;
use std::{
    collections::{HashMap, HashSet},
    sync::RwLock,
};

use crate::transaction::{Snapshot, TXN_ID, Transaction};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionStatus {
    Active,
    Committed,
    Aborted,
}

pub struct TransactionManager {
    pub clog: RwLock<HashMap<u64, TransactionStatus>>,
    pub active_txns: RwLock<HashSet<u64>>,
}

impl TransactionManager {
    pub fn new() -> Self {
        Self {
            clog: RwLock::new(HashMap::new()),
            active_txns: RwLock::new(HashSet::new()),
        }
    }

    pub fn begin(&self) -> Transaction {
        // Write-lock active_txns FIRST to prevent race conditions with get_snapshot.
        // We must lock before fetching TXN_ID to ensure that no snapshot is
        // generated in between ID creation and active set insertion.
        let mut active = self.active_txns.write().unwrap();

        let txn_id = TXN_ID.fetch_add(1, Relaxed);
        active.insert(txn_id);

        let xmin = *active.iter().min().unwrap_or(&txn_id);
        let xmax = TXN_ID.load(Relaxed);
        let active_vec: Vec<u64> = active.iter().cloned().collect();

        drop(active);

        self.clog
            .write()
            .unwrap()
            .insert(txn_id, TransactionStatus::Active);

        Transaction {
            txn_id,
            snapshot: Snapshot {
                xmin,
                xmax,
                active: active_vec,
            },
        }
    }

    pub fn commit(&self, txn_id: u64) {
        self.clog
            .write()
            .unwrap()
            .insert(txn_id, TransactionStatus::Committed);
        self.active_txns.write().unwrap().remove(&txn_id);
    }

    pub fn abort(&self, txn_id: u64) {
        // TODO: The storage/undo layer must rollback writes before calling this.
        self.clog
            .write()
            .unwrap()
            .insert(txn_id, TransactionStatus::Aborted);
        self.active_txns.write().unwrap().remove(&txn_id);
    }

    pub fn is_committed(&self, txn_id: u64) -> bool {
        if txn_id == 0 {
            return true;
        } 
        self.clog.read().unwrap().get(&txn_id) == Some(&TransactionStatus::Committed)
    }

    pub fn is_aborted(&self, txn_id: u64) -> bool {
        self.clog.read().unwrap().get(&txn_id) == Some(&TransactionStatus::Aborted)
    }

    pub fn is_active(&self, txn_id: u64) -> bool {
        self.active_txns.read().unwrap().contains(&txn_id)
    }

    pub fn get_snapshot(&self) -> Snapshot {
        // Read lock active_txns to guarantee consistency
        let active = self.active_txns.read().unwrap();
        let xmax = TXN_ID.load(Relaxed);
        Snapshot {
            xmin: *active.iter().min().unwrap_or(&xmax),
            xmax,
            active: active.iter().cloned().collect(),
        }
    }
}
