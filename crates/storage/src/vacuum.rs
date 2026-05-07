use crate::page::{LeafPageAccessor, LeafPageBuilder, PageId};
use common::{Key, Value};
use db_core::transaction_manager::TransactionManager;

pub fn is_dead(xmin: u64, xmax: u64, tm: &TransactionManager) -> bool {
    if xmax == 0 || !tm.is_committed(xmax) {
        return false;
    }

    if !tm.is_aborted(xmin) {
        return true;
    }

    xmin < tm.global_xmin()
}

pub fn compact_leaf_page<K: Key, V: Value>(
    page_data: &mut [u8],
    page_id: PageId,
    tm: &TransactionManager,
) -> usize {
    let acc = LeafPageAccessor::<K, V>::new(page_data);
    let n = acc.num_pairs() as usize;

    let mut live: Vec<(Vec<u8>, Vec<u8>, u64, u64)> = Vec::new();
    let mut dead_count = 0;

    for i in 0..n {
        let xmin = acc.get_xmin();
        let xmax = acc.get_xmax();

        if is_dead(xmin, xmax, tm) {
            dead_count += 1;
            continue;
        }

        let k = K::as_bytes(&acc.get_key(i)).as_ref().to_vec();
        let v = V::as_bytes(&acc.get_value(i)).as_ref().to_vec();
        live.push((k, v, xmin, xmax));
    }

    if dead_count == 0 {
        return 0;
    }

    let high_key: Option<Vec<u8>> = acc.high_key_bytes().map(|b| b.to_vec());
    let rightlink = acc.rightlink();
    let prev_page = acc.prev_page();
    let lsn = acc.lsn();

    let mut builder = LeafPageBuilder::<K, V>::new(page_id, page_data);
    if let Some(ref hk) = high_key {
        builder.set_high_key(hk);
    }

    builder.set_rightlink(rightlink);
    builder.set_prev_page(prev_page);
    for (k, v, xmin, xmax) in &live {
        builder.push_with_mvcc(&K::from_bytes(k), &v::from_bytes(v), *xmin, *xmax);
    }
    let mut m = builder.finish();
    m.set_lsn(lsn);

    dead_count
}
