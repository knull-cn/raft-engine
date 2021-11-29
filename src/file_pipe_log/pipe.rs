// Copyright (c) 2017-present, PingCAP, Inc. Licensed under Apache-2.0.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::path::PathBuf;
use std::sync::Arc;

use crossbeam::utils::CachePadded;
use fail::fail_point;
use fs2::FileExt;
use log::warn;
use parking_lot::{Mutex, MutexGuard, RwLock};

use crate::config::Config;
use crate::event_listener::EventListener;
use crate::file_builder::FileBuilder;
use crate::metrics::*;
use crate::pipe_log::{FileBlockHandle, FileId, FileSeq, LogQueue, PipeLog};
use crate::{Error, Result};

use super::format::{lock_file_path, FileNameExt};
use super::log_file::{build_file_reader, build_file_writer, LogFd, LogFileWriter};

struct FileCollection {
    first_seq: FileSeq,
    active_seq: FileSeq,
    fds: VecDeque<Arc<LogFd>>,
}

struct ActiveFile<B: FileBuilder> {
    seq: FileSeq,
    writer: LogFileWriter<B>,
}

pub struct SinglePipe<B: FileBuilder> {
    queue: LogQueue,
    dir: String,
    target_file_size: usize,
    bytes_per_sync: usize,
    file_builder: Arc<B>,
    listeners: Vec<Arc<dyn EventListener>>,

    files: CachePadded<RwLock<FileCollection>>,
    active_file: CachePadded<Mutex<ActiveFile<B>>>,
}

impl<B: FileBuilder> SinglePipe<B> {
    pub fn open(
        cfg: &Config,
        file_builder: Arc<B>,
        listeners: Vec<Arc<dyn EventListener>>,
        queue: LogQueue,
        mut first_seq: FileSeq,
        mut fds: VecDeque<Arc<LogFd>>,
    ) -> Result<Self> {
        let create_file = first_seq == 0;
        let active_seq = if create_file {
            first_seq = 1;
            let file_id = FileId {
                queue,
                seq: first_seq,
            };
            let fd = Arc::new(LogFd::create(&file_id.build_file_path(&cfg.dir))?);
            fds.push_back(fd);
            first_seq
        } else {
            first_seq + fds.len() as u64 - 1
        };

        for seq in first_seq..=active_seq {
            for listener in &listeners {
                listener.post_new_log_file(FileId { queue, seq });
            }
        }

        let active_fd = fds.back().unwrap().clone();
        let file_id = FileId {
            queue,
            seq: active_seq,
        };
        let active_file = ActiveFile {
            seq: active_seq,
            writer: build_file_writer(
                file_builder.as_ref(),
                &file_id.build_file_path(&cfg.dir),
                active_fd,
                create_file,
            )?,
        };

        let total_files = fds.len();
        let pipe = Self {
            queue,
            dir: cfg.dir.clone(),
            target_file_size: cfg.target_file_size.0 as usize,
            bytes_per_sync: cfg.bytes_per_sync.0 as usize,
            file_builder,
            listeners,

            files: CachePadded::new(RwLock::new(FileCollection {
                first_seq,
                active_seq,
                fds,
            })),
            active_file: CachePadded::new(Mutex::new(active_file)),
        };
        pipe.flush_metrics(total_files);
        Ok(pipe)
    }

    fn sync_dir(&self) -> Result<()> {
        let path = PathBuf::from(&self.dir);
        std::fs::File::open(path).and_then(|d| d.sync_all())?;
        Ok(())
    }

    fn get_fd(&self, file_seq: FileSeq) -> Result<Arc<LogFd>> {
        let files = self.files.read();
        if file_seq < files.first_seq || file_seq > files.active_seq {
            return Err(Error::Corruption("file seqno out of range".to_owned()));
        }
        Ok(files.fds[(file_seq - files.first_seq) as usize].clone())
    }

    fn rotate_imp(&self, active_file: &mut MutexGuard<ActiveFile<B>>) -> Result<()> {
        let _t = StopWatch::new(&LOG_ROTATE_DURATION_HISTOGRAM);
        let seq = active_file.seq + 1;
        debug_assert!(seq > 1);

        let file_id = FileId {
            queue: self.queue,
            seq,
        };
        let path = file_id.build_file_path(&self.dir);
        let fd = Arc::new(LogFd::create(&path)?);
        self.sync_dir()?;
        let new_file = ActiveFile {
            seq,
            writer: build_file_writer(
                self.file_builder.as_ref(),
                &path,
                fd.clone(),
                true, /*create*/
            )?,
        };

        active_file.writer.close()?;
        **active_file = new_file;

        let len = {
            let mut files = self.files.write();
            debug_assert!(files.active_seq + 1 == seq);
            files.active_seq = seq;
            files.fds.push_back(fd);
            for listener in &self.listeners {
                listener.post_new_log_file(FileId {
                    queue: self.queue,
                    seq,
                });
            }
            files.fds.len()
        };
        self.flush_metrics(len);
        Ok(())
    }

    fn flush_metrics(&self, len: usize) {
        match self.queue {
            LogQueue::Append => LOG_FILE_COUNT.append.set(len as i64),
            LogQueue::Rewrite => LOG_FILE_COUNT.rewrite.set(len as i64),
        }
    }
}

impl<B: FileBuilder> SinglePipe<B> {
    fn read_bytes(&self, handle: FileBlockHandle) -> Result<Vec<u8>> {
        let fd = self.get_fd(handle.id.seq)?;
        let mut reader = build_file_reader(
            self.file_builder.as_ref(),
            &handle.id.build_file_path(&self.dir),
            fd,
        )?;
        reader.read(handle)
    }

    fn append(&self, bytes: &[u8]) -> Result<FileBlockHandle> {
        fail_point!("file_pipe_log::append");
        let mut active_file = self.active_file.lock();
        let seq = active_file.seq;
        let writer = &mut active_file.writer;

        let start_offset = writer.offset();
        if let Err(e) = writer.write(bytes, self.target_file_size) {
            if let Err(te) = writer.truncate() {
                panic!(
                    "error when truncate {} after error: {}, get: {}",
                    seq, e, te
                );
            }
            return Err(e);
        }
        let handle = FileBlockHandle {
            id: FileId {
                queue: self.queue,
                seq,
            },
            offset: start_offset as u64,
            len: writer.offset() - start_offset,
        };
        for listener in &self.listeners {
            listener.on_append_log_file(handle);
        }
        Ok(handle)
    }

    fn maybe_sync(&self, force: bool) -> Result<()> {
        let mut active_file = self.active_file.lock();
        let seq = active_file.seq;
        let writer = &mut active_file.writer;
        if writer.offset() >= self.target_file_size {
            if let Err(e) = self.rotate_imp(&mut active_file) {
                panic!("error when rotate [{:?}:{}]: {}", self.queue, seq, e);
            }
        } else if writer.since_last_sync() >= self.bytes_per_sync || force {
            if let Err(e) = writer.sync() {
                panic!("error when sync [{:?}:{}]: {}", self.queue, seq, e,);
            }
        }

        Ok(())
    }

    fn file_span(&self) -> (FileSeq, FileSeq) {
        let files = self.files.read();
        (files.first_seq, files.active_seq)
    }

    fn total_size(&self) -> usize {
        let files = self.files.read();
        (files.active_seq - files.first_seq + 1) as usize * self.target_file_size
    }

    fn rotate(&self) -> Result<()> {
        self.rotate_imp(&mut self.active_file.lock())
    }

    fn purge_to(&self, file_seq: FileSeq) -> Result<usize> {
        let (purged, remained) = {
            let mut files = self.files.write();
            if file_seq > files.active_seq {
                return Err(box_err!("Purge active or newer files"));
            }
            let end_offset = file_seq.saturating_sub(files.first_seq) as usize;
            files.fds.drain(..end_offset);
            files.first_seq = file_seq;
            (end_offset, files.fds.len())
        };
        self.flush_metrics(remained);
        for seq in file_seq - purged as u64..file_seq {
            let file_id = FileId {
                queue: self.queue,
                seq,
            };
            let path = file_id.build_file_path(&self.dir);
            #[cfg(feature = "failpoints")]
            {
                let remove_failure = || {
                    fail::fail_point!("file_pipe_log::remove_file_failure", |_| true);
                    false
                };
                if remove_failure() {
                    continue;
                }
            }
            if let Err(e) = fs::remove_file(&path) {
                warn!("Remove purged log file {:?} failed: {}", path, e);
            }
        }
        Ok(purged)
    }
}

pub struct DualPipes<B: FileBuilder> {
    pipes: [SinglePipe<B>; 2],

    _lock_file: File,
}

impl<B: FileBuilder> DualPipes<B> {
    pub fn open(dir: &str, appender: SinglePipe<B>, rewriter: SinglePipe<B>) -> Result<Self> {
        let lock_file = File::create(lock_file_path(dir))?;
        lock_file.try_lock_exclusive().map_err(|e| {
            Error::Other(box_err!(
                "Failed to lock file: {}, maybe another instance is using this directory.",
                e
            ))
        })?;

        // TODO: remove this dependency.
        debug_assert_eq!(LogQueue::Append as usize, 0);
        debug_assert_eq!(LogQueue::Rewrite as usize, 1);
        Ok(Self {
            pipes: [appender, rewriter],
            _lock_file: lock_file,
        })
    }

    #[cfg(test)]
    pub fn file_builder(&self) -> Arc<B> {
        self.pipes[0].file_builder.clone()
    }
}

impl<B: FileBuilder> PipeLog for DualPipes<B> {
    #[inline]
    fn read_bytes(&self, handle: FileBlockHandle) -> Result<Vec<u8>> {
        self.pipes[handle.id.queue as usize].read_bytes(handle)
    }

    #[inline]
    fn append(&self, queue: LogQueue, bytes: &[u8]) -> Result<FileBlockHandle> {
        self.pipes[queue as usize].append(bytes)
    }

    #[inline]
    fn maybe_sync(&self, queue: LogQueue, force: bool) -> Result<()> {
        self.pipes[queue as usize].maybe_sync(force)
    }

    #[inline]
    fn file_span(&self, queue: LogQueue) -> (FileSeq, FileSeq) {
        self.pipes[queue as usize].file_span()
    }

    #[inline]
    fn total_size(&self, queue: LogQueue) -> usize {
        self.pipes[queue as usize].total_size()
    }

    #[inline]
    fn rotate(&self, queue: LogQueue) -> Result<()> {
        self.pipes[queue as usize].rotate()
    }

    #[inline]
    fn purge_to(&self, file_id: FileId) -> Result<usize> {
        self.pipes[file_id.queue as usize].purge_to(file_id.seq)
    }
}

#[cfg(test)]
mod tests {
    use tempfile::Builder;

    use super::super::format::LogFileHeader;
    use super::*;
    use crate::file_builder::DefaultFileBuilder;
    use crate::util::ReadableSize;

    fn new_test_pipe(cfg: &Config, queue: LogQueue) -> Result<SinglePipe<DefaultFileBuilder>> {
        SinglePipe::open(
            cfg,
            Arc::new(DefaultFileBuilder),
            Vec::new(),
            queue,
            0,
            VecDeque::new(),
        )
    }

    fn new_test_pipes(cfg: &Config) -> Result<DualPipes<DefaultFileBuilder>> {
        DualPipes::open(
            &cfg.dir,
            new_test_pipe(cfg, LogQueue::Append)?,
            new_test_pipe(cfg, LogQueue::Rewrite)?,
        )
    }

    #[test]
    fn test_dir_lock() {
        let dir = Builder::new().prefix("test_dir_lock").tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        let cfg = Config {
            dir: path.to_owned(),
            ..Default::default()
        };

        let _r1 = new_test_pipes(&cfg).unwrap();

        // Only one thread can hold file lock
        let r2 = new_test_pipes(&cfg);

        assert!(format!("{}", r2.err().unwrap())
            .contains("maybe another instance is using this directory"));
    }

    #[test]
    fn test_pipe_log() {
        let dir = Builder::new().prefix("test_pipe_log").tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        let cfg = Config {
            dir: path.to_owned(),
            target_file_size: ReadableSize::kb(1),
            bytes_per_sync: ReadableSize::kb(32),
            ..Default::default()
        };
        let queue = LogQueue::Append;

        let pipe_log = new_test_pipes(&cfg).unwrap();
        assert_eq!(pipe_log.file_span(queue), (1, 1));

        let header_size = LogFileHeader::len() as u64;

        // generate file 1, 2, 3
        let content: Vec<u8> = vec![b'a'; 1024];
        let file_handle = pipe_log.append(queue, &content).unwrap();
        pipe_log.maybe_sync(queue, false).unwrap();
        assert_eq!(file_handle.id.seq, 1);
        assert_eq!(file_handle.offset, header_size);
        assert_eq!(pipe_log.file_span(queue).1, 2);

        let file_handle = pipe_log.append(queue, &content).unwrap();
        pipe_log.maybe_sync(queue, false).unwrap();
        assert_eq!(file_handle.id.seq, 2);
        assert_eq!(file_handle.offset, header_size);
        assert_eq!(pipe_log.file_span(queue).1, 3);

        // purge file 1
        assert_eq!(pipe_log.purge_to(FileId { queue, seq: 2 }).unwrap(), 1);
        assert_eq!(pipe_log.file_span(queue).0, 2);

        // cannot purge active file
        assert!(pipe_log.purge_to(FileId { queue, seq: 4 }).is_err());

        // append position
        let s_content = b"short content".to_vec();
        let file_handle = pipe_log.append(queue, &s_content).unwrap();
        pipe_log.maybe_sync(queue, false).unwrap();
        assert_eq!(file_handle.id.seq, 3);
        assert_eq!(file_handle.offset, header_size);

        let file_handle = pipe_log.append(queue, &s_content).unwrap();
        pipe_log.maybe_sync(queue, false).unwrap();
        assert_eq!(file_handle.id.seq, 3);
        assert_eq!(
            file_handle.offset,
            header_size as u64 + s_content.len() as u64
        );

        let content_readed = pipe_log
            .read_bytes(FileBlockHandle {
                id: FileId { queue, seq: 3 },
                offset: header_size as u64,
                len: s_content.len(),
            })
            .unwrap();
        assert_eq!(content_readed, s_content);

        // leave only 1 file to truncate
        assert!(pipe_log.purge_to(FileId { queue, seq: 3 }).is_ok());
        assert_eq!(pipe_log.file_span(queue), (3, 3));
    }
}