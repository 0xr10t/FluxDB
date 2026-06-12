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
    index: Arc<BTreeIndex<K, V>>,
    wal: Arc<Mutex<Wal>>,
    disk_manager: Arc<DiskManager>,
    buffer_pool: Arc<BufferPoolManager>,
    transaction_manager: Arc<TransactionManager>,
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
    // opens a existing database
    pub fn open(dir_path: impl AsRef<Path>) -> Result<Engine<K, V>, EngineError> {
        todo!()
    }
}
