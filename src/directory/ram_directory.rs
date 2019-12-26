use crate::common::CountingWriter;
use crate::core::META_FILEPATH;
use crate::directory::error::{DeleteError, OpenReadError, OpenWriteError};
use crate::directory::AntiCallToken;
use crate::directory::WatchCallbackList;
use crate::directory::{Directory, ReadOnlySource, WatchCallback, WatchHandle};
use crate::directory::{TerminatingWrite, WritePtr};
use fail::fail_point;
use std::collections::HashMap;
use std::fmt;
use std::io::{self, BufWriter, Cursor, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::result;
use std::sync::{Arc, RwLock};

/// Writer associated with the `RAMDirectory`
///
/// The Writer just writes a buffer.
///
/// # Panics
///
/// On drop, if the writer was left in a *dirty* state.
/// That is, if flush was not called after the last call
/// to write.
///
struct VecWriter {
    path: PathBuf,
    shared_directory: RAMDirectory,
    data: Cursor<Vec<u8>>,
    is_flushed: bool,
}

impl VecWriter {
    fn new(path_buf: PathBuf, shared_directory: RAMDirectory) -> VecWriter {
        VecWriter {
            path: path_buf,
            data: Cursor::new(Vec::new()),
            shared_directory,
            is_flushed: true,
        }
    }
}

impl Drop for VecWriter {
    fn drop(&mut self) {
        if !self.is_flushed {
            panic!(
                "You forgot to flush {:?} before its writter got Drop. Do not rely on drop.",
                self.path
            )
        }
    }
}

impl Seek for VecWriter {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.data.seek(pos)
    }
}

impl Write for VecWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.is_flushed = false;
        self.data.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.is_flushed = true;
        let mut fs = self.shared_directory.fs.write().unwrap();
        fs.write(self.path.clone(), self.data.get_ref());
        Ok(())
    }
}

impl TerminatingWrite for VecWriter {
    fn terminate_ref(&mut self, _: AntiCallToken) -> io::Result<()> {
        self.flush()
    }
}

#[derive(Default)]
struct InnerDirectory {
    fs: HashMap<PathBuf, ReadOnlySource>,
    watch_router: WatchCallbackList,
}

impl InnerDirectory {
    fn write(&mut self, path: PathBuf, data: &[u8]) -> bool {
        let data = ReadOnlySource::new(Vec::from(data));
        self.fs.insert(path, data).is_some()
    }

    fn open_read(&self, path: &Path) -> Result<ReadOnlySource, OpenReadError> {
        self.fs
            .get(path)
            .ok_or_else(|| OpenReadError::FileDoesNotExist(PathBuf::from(path)))
            .map(Clone::clone)
    }

    fn delete(&mut self, path: &Path) -> result::Result<(), DeleteError> {
        match self.fs.remove(path) {
            Some(_) => Ok(()),
            None => Err(DeleteError::FileDoesNotExist(PathBuf::from(path))),
        }
    }

    fn exists(&self, path: &Path) -> bool {
        self.fs.contains_key(path)
    }

    fn watch(&mut self, watch_handle: WatchCallback) -> WatchHandle {
        self.watch_router.subscribe(watch_handle)
    }

    fn total_mem_usage(&self) -> usize {
        self.fs.values().map(|f| f.len()).sum()
    }

    fn serialize_bundle(self, wrt: &mut WritePtr) -> io::Result<()> {
        let mut counting_writer = CountingWriter::wrap(wrt);
        let mut file_index: HashMap<PathBuf, (u64, u64)> = HashMap::default();
        for (path, source) in &self.fs {
            let start = counting_writer.written_bytes();
            counting_writer.write_all(source.as_slice())?;
            let stop = counting_writer.written_bytes();
            file_index.insert(path.to_path_buf(), (start, stop));
        }
        serde_json::to_writer(&mut counting_writer, &file_index)?;
        let index_offset = counting_writer.written_bytes();
        let index_offset_buffer = index_offset.to_le_bytes();
        counting_writer.write_all(&index_offset_buffer[..])?;
        Ok(())
    }
}

impl fmt::Debug for RAMDirectory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RAMDirectory")
    }
}

/// A Directory storing everything in anonymous memory.
///
/// It is mainly meant for unit testing.
/// Writes are only made visible upon flushing.
///
#[derive(Clone, Default)]
pub struct RAMDirectory {
    fs: Arc<RwLock<InnerDirectory>>,
}

impl RAMDirectory {
    /// Constructor
    pub fn create() -> RAMDirectory {
        Self::default()
    }

    /// Returns the sum of the size of the different files
    /// in the RAMDirectory.
    pub fn total_mem_usage(&self) -> usize {
        self.fs.read().unwrap().total_mem_usage()
    }

    /// Serialize the RAMDirectory into a bundle.
    ///
    /// This method will fail, write nothing, and return an error if a
    /// clone of this repository exists.
    pub fn serialize_bundle(self, wrt: &mut WritePtr) -> io::Result<()> {
        let inner_directory = self.try_unwrap().map_err(|_| {
            io::Error::new(
                io::ErrorKind::Other,
                "Serialize bundle requires that \
            there are no other existing copy of the directory."
                    .to_string(),
            )
        })?;
        inner_directory.serialize_bundle(wrt)
    }

    fn try_unwrap(self) -> Result<InnerDirectory, ()> {
        let inner_directory_lock = Arc::try_unwrap(self.fs).map_err(|_| ())?;
        let inner_directory = inner_directory_lock.into_inner().map_err(|_| ())?;
        Ok(inner_directory)
    }
}

impl Directory for RAMDirectory {
    fn open_read(&self, path: &Path) -> result::Result<ReadOnlySource, OpenReadError> {
        self.fs.read().unwrap().open_read(path)
    }

    fn delete(&self, path: &Path) -> result::Result<(), DeleteError> {
        fail_point!("RAMDirectory::delete", |_| {
            use crate::directory::error::IOError;
            let io_error = IOError::from(io::Error::from(io::ErrorKind::Other));
            Err(DeleteError::from(io_error))
        });
        self.fs.write().unwrap().delete(path)
    }

    fn exists(&self, path: &Path) -> bool {
        self.fs.read().unwrap().exists(path)
    }

    fn open_write(&mut self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        let mut fs = self.fs.write().unwrap();
        let path_buf = PathBuf::from(path);
        let vec_writer = VecWriter::new(path_buf.clone(), self.clone());
        let exists = fs.write(path_buf.clone(), &[]);
        // force the creation of the file to mimic the MMap directory.
        if exists {
            Err(OpenWriteError::FileAlreadyExists(path_buf))
        } else {
            Ok(BufWriter::new(Box::new(vec_writer)))
        }
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        Ok(self.open_read(path)?.as_slice().to_owned())
    }

    fn atomic_write(&mut self, path: &Path, data: &[u8]) -> io::Result<()> {
        fail_point!("RAMDirectory::atomic_write", |msg| Err(io::Error::new(
            io::ErrorKind::Other,
            msg.unwrap_or_else(|| "Undefined".to_string())
        )));
        let path_buf = PathBuf::from(path);

        // Reserve the path to prevent calls to .write() to succeed.
        self.fs.write().unwrap().write(path_buf.clone(), &[]);

        let mut vec_writer = VecWriter::new(path_buf, self.clone());
        vec_writer.write_all(data)?;
        vec_writer.flush()?;
        if path == Path::new(&*META_FILEPATH) {
            let _ = self.fs.write().unwrap().watch_router.broadcast();
        }
        Ok(())
    }

    fn watch(&self, watch_callback: WatchCallback) -> crate::Result<WatchHandle> {
        Ok(self.fs.write().unwrap().watch(watch_callback))
    }
}
