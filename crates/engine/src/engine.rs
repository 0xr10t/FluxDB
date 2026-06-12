use common::{EngineError, Key, Value};
use db_core::transaction_manager::TransactionManager;
use std::path::Path;
use std::sync::{Arc, Mutex};
use storage::buffer_pool::BufferPoolManager;
use storage::disk::DiskManager;
use storage::index::BTreeIndex;
use storage::page::PAGE_SIZE;
use storage::wal::Wal;
pub struct Engine<K, V>
where
    K: Key,
    V: Value,
{
    pub(crate) index: Arc<BTreeIndex<K, V>>,
    pub(crate) wal: Arc<Mutex<Wal>>,
    pub(crate) disk_manager: Arc<DiskManager>,
    pub(crate) buffer_pool: Arc<BufferPoolManager>,
    pub(crate) transaction_manager: Arc<TransactionManager>,
}

impl<K, V> Engine<K, V>
where
    K: Key,
    V: Value,
{
    // creates a new database
    pub fn create(dir_path: impl AsRef<Path>) -> Result<Engine<K, V>, EngineError> {
        let path = dir_path.as_ref();
        std::fs::create_dir_all(path)?;
        let exist = path.join("data.db").exists();
        if exist {
            return Err(EngineError::AlreadyExists);
        }
        // initialize disk manager
        let disk_manager = Arc::new(DiskManager::new(path.join("data.db"), PAGE_SIZE)?);
        // initialize wal;
        // Arc is needed on WAL as both index and buffer_pool will later have a wal instance.
        let wal = Arc::new(Mutex::new(Wal::new(path.join("wal.log"))?));
        let buffer_pool = Arc::new(BufferPoolManager::new(Arc::clone(&disk_manager)));
        let transaction_manager = Arc::new(TransactionManager::new());
        let (index, _root) = BTreeIndex::create(Arc::clone(&buffer_pool))?;
        // index needs Arc as it will be later cloned by vaccum to call index.vaccum()
        let index = Arc::new(index);
        // TODO! spawn checkpoint thread once checkpoint is there
        // TODO! spawn vacuum thread once vacuum is implemented
        Ok(Engine {
            index,
            wal,
            buffer_pool,
            disk_manager,
            transaction_manager,
        })
    }
    // opens an existing database
    pub fn open(dir_path: impl AsRef<Path>) -> Result<Engine<K, V>, EngineError> {
        let path = dir_path.as_ref();
        // the data file is the marker that a database lives here
        if !path.join("data.db").exists() {
            return Err(EngineError::NotFound);
        }
        let disk_manager = Arc::new(DiskManager::new(path.join("data.db"), PAGE_SIZE)?);
        // Wal::new opens the existing log (and creates it if a pre-WAL database
        // never had one) — the tail scan / LSN resume lands with the log manager work
        let wal = Arc::new(Mutex::new(Wal::new(path.join("wal.log"))?));
        let buffer_pool = Arc::new(BufferPoolManager::new(Arc::clone(&disk_manager)));
        let transaction_manager = Arc::new(TransactionManager::new());
        // recovery runs HERE — after the pool exists, before the index opens:
        // read checkpoint from superblock → seed CLOG from pinned_aborted[] →
        // replay WAL from redo_point → mark crash victims Aborted →
        // inject next_txn_id / next_page_id watermarks (from_recovered)
        // index reads the root page id from the page-0 superblock
        let index = Arc::new(BTreeIndex::open(Arc::clone(&buffer_pool))?);
        // TODO! spawn checkpoint + vacuum threads
        Ok(Engine {
            index,
            wal,
            buffer_pool,
            disk_manager,
            transaction_manager,
        })
    }


    // implement close() when checkpoint lands. 
}
