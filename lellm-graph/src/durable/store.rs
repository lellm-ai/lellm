use super::{DurableError, RunRecord};
use parking_lot::Mutex;
use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

/// 每次执行、决策或取消都必须持有独占租用；释放后可被其他进程恢复。
pub trait DurableStore: Send + Sync {
    fn acquire(&self, run_id: &str) -> Result<Box<dyn RunLease>, DurableError>;
    fn inspect(&self, run_id: &str) -> Result<RunRecord, DurableError>;
}

/// 保存完整信封；实现必须原子发布，且失败后不得用未确认记录继续执行。
pub trait RunLease: Send + Sync {
    fn load(&self) -> Result<Option<RunRecord>, DurableError>;
    fn save(&self, record: &RunRecord) -> Result<(), DurableError>;
}

#[derive(Default)]
struct MemoryData {
    records: HashMap<String, Vec<u8>>,
    held: HashSet<String>,
}

#[derive(Clone, Default)]
pub struct MemoryRunStore {
    inner: Arc<Mutex<MemoryData>>,
}
struct MemoryLease {
    inner: Arc<Mutex<MemoryData>>,
    id: String,
}

fn validate_id(id: &str) -> Result<(), DurableError> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(DurableError::Conflict(
            "run_id must contain 1..128 ASCII letters, digits, '-' or '_'".into(),
        ));
    }
    Ok(())
}

impl DurableStore for MemoryRunStore {
    fn acquire(&self, id: &str) -> Result<Box<dyn RunLease>, DurableError> {
        validate_id(id)?;
        if !self.inner.lock().held.insert(id.into()) {
            return Err(DurableError::Busy);
        }
        Ok(Box::new(MemoryLease {
            inner: self.inner.clone(),
            id: id.into(),
        }))
    }
    fn inspect(&self, id: &str) -> Result<RunRecord, DurableError> {
        validate_id(id)?;
        let data = self.inner.lock();
        RunRecord::decode(data.records.get(id).ok_or(DurableError::NotFound)?)
    }
}
impl RunLease for MemoryLease {
    fn load(&self) -> Result<Option<RunRecord>, DurableError> {
        self.inner
            .lock()
            .records
            .get(&self.id)
            .map(|v| RunRecord::decode(v))
            .transpose()
    }
    fn save(&self, record: &RunRecord) -> Result<(), DurableError> {
        if record.run_id != self.id {
            return Err(DurableError::Conflict("run identity".into()));
        }
        self.inner
            .lock()
            .records
            .insert(self.id.clone(), serde_json::to_vec(record)?);
        Ok(())
    }
}
impl Drop for MemoryLease {
    fn drop(&mut self) {
        self.inner.lock().held.remove(&self.id);
    }
}

/// 本机文件后端；锁文件永不替换/删除，数据通过同目录 rename 原子发布。
/// 同步 IO；保证进程崩溃恢复，不承诺断电安全或网络文件系统锁语义。
pub struct FileRunStore {
    root: PathBuf,
}
impl FileRunStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    fn directory(&self, id: &str) -> Result<PathBuf, DurableError> {
        validate_id(id)?;
        Ok(self.root.join(id))
    }
}
struct FileLease {
    directory: PathBuf,
    id: String,
    _lock: File,
}
fn read_record(path: &Path, id: &str) -> Result<Option<RunRecord>, DurableError> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let record = RunRecord::decode(&bytes)?;
            if record.run_id != id {
                return Err(DurableError::Incompatible("run identity".into()));
            }
            Ok(Some(record))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
impl DurableStore for FileRunStore {
    fn acquire(&self, id: &str) -> Result<Box<dyn RunLease>, DurableError> {
        let directory = self.directory(id)?;
        std::fs::create_dir_all(&directory)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("run.lock"))?;
        lock.try_lock().map_err(|e| match e {
            std::fs::TryLockError::WouldBlock => DurableError::Busy,
            std::fs::TryLockError::Error(e) => DurableError::Io(e),
        })?;
        Ok(Box::new(FileLease {
            directory,
            id: id.into(),
            _lock: lock,
        }))
    }
    fn inspect(&self, id: &str) -> Result<RunRecord, DurableError> {
        read_record(&self.directory(id)?.join("run.json"), id)?.ok_or(DurableError::NotFound)
    }
}
impl RunLease for FileLease {
    fn load(&self) -> Result<Option<RunRecord>, DurableError> {
        read_record(&self.directory.join("run.json"), &self.id)
    }
    fn save(&self, record: &RunRecord) -> Result<(), DurableError> {
        if record.run_id != self.id {
            return Err(DurableError::Conflict("run identity".into()));
        }
        let bytes = serde_json::to_vec(record)?;
        let temp = self.directory.join("run.pending");
        let mut file = File::create(&temp)?;
        file.write_all(&bytes)?;
        file.flush()?;
        drop(file);
        std::fs::rename(temp, self.directory.join("run.json"))?;
        Ok(())
    }
}
