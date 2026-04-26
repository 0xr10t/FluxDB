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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_begin_transaction() {
        let tm = TransactionManager::new();
        let txn = tm.begin();

        assert_eq!(txn.snapshot.active.len(), 1);
        assert_eq!(txn.snapshot.active[0], txn.txn_id);
        assert!(tm.is_active(txn.txn_id));
        assert!(!tm.is_committed(txn.txn_id));
        assert!(!tm.is_aborted(txn.txn_id));
    }

    #[test]
    fn test_commit_transaction() {
        let tm = TransactionManager::new();
        let txn = tm.begin();

        tm.commit(txn.txn_id);

        assert!(!tm.is_active(txn.txn_id));
        assert!(tm.is_committed(txn.txn_id));
        assert!(!tm.is_aborted(txn.txn_id));
    }

    #[test]
    fn test_abort_transaction() {
        let tm = TransactionManager::new();
        let txn = tm.begin();

        tm.abort(txn.txn_id);

        assert!(!tm.is_active(txn.txn_id));
        assert!(!tm.is_committed(txn.txn_id));
        assert!(tm.is_aborted(txn.txn_id));
    }

    #[test]
    fn test_snapshot_empty_active() {
        let tm = TransactionManager::new();
        let snap = tm.get_snapshot();

        // When empty, xmin should equal xmax
        assert_eq!(snap.xmin, snap.xmax);
        assert!(snap.active.is_empty());
    }

    #[test]
    fn test_snapshot_with_multiple_active() {
        let tm = TransactionManager::new();
        let txn1 = tm.begin();
        let txn2 = tm.begin();

        let snap = tm.get_snapshot();

        assert_eq!(snap.xmin, txn1.txn_id); // txn1 is the oldest active
        assert!(snap.xmax > txn2.txn_id);
        assert!(snap.active.contains(&txn1.txn_id));
        assert!(snap.active.contains(&txn2.txn_id));
        assert_eq!(snap.active.len(), 2);
    }

    #[test]
    fn test_snapshot_with_commits_in_middle() {
        let tm = TransactionManager::new();
        let txn1 = tm.begin();
        let txn2 = tm.begin();
        let txn3 = tm.begin();

        // Commit txn2 in the middle
        tm.commit(txn2.txn_id);

        let snap = tm.get_snapshot();

        assert_eq!(snap.xmin, txn1.txn_id); // txn1 is still oldest
        assert!(!snap.active.contains(&txn2.txn_id)); // txn2 committed
        assert!(snap.active.contains(&txn1.txn_id));
        assert!(snap.active.contains(&txn3.txn_id));
        assert_eq!(snap.active.len(), 2);
    }
}
