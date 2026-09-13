//! Optional per-volume write-back cache in front of a BlockStore.
//!
//! `none` — write-through (SCSI write returns after the inner store accepts it).
//! `memory` — dirty chunks in RAM; lost on crash; flushed on SYNCHRONIZE CACHE / shutdown.
//! `disk` — same as memory plus atomic files under `path` so a restart can recover.

use crate::cache::{CachedStore, ChunkCache};
use crate::config::{WriteBufferConfig, WriteCacheConfig, WriteCacheMode};
use crate::store::{check_range, BlockStore, S3ChunkStore, StoreError};
use parking_lot::{Condvar, Mutex};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// A dirty chunk is eligible for background handoff after this quiet period.
const QUIET: Duration = Duration::from_millis(100);

const STAMP_NAME: &str = ".write-cache.json";

struct BufferedChunk {
    data: Vec<u8>,
    generation: u64,
}

struct Job {
    idx: u64,
    data: Vec<u8>,
    generation: u64,
}

struct BufState {
    chunks: HashMap<u64, BufferedChunk>,
    order: VecDeque<u64>,
    used_bytes: u64,
    inflight: Option<u64>,
}

impl BufState {
    fn new() -> Self {
        Self {
            chunks: HashMap::new(),
            order: VecDeque::new(),
            used_bytes: 0,
            inflight: None,
        }
    }

    fn push(&mut self, idx: u64, data: Vec<u8>, generation: u64) {
        let size = data.len() as u64;
        if let Some(old) = self.chunks.insert(
            idx,
            BufferedChunk {
                data,
                generation,
            },
        ) {
            self.used_bytes = self.used_bytes.saturating_sub(old.data.len() as u64);
            self.order.retain(|i| *i != idx);
        }
        self.used_bytes = self.used_bytes.saturating_add(size);
        self.order.push_back(idx);
    }

    fn remove(&mut self, idx: u64) -> Option<BufferedChunk> {
        let old = self.chunks.remove(&idx)?;
        self.used_bytes = self.used_bytes.saturating_sub(old.data.len() as u64);
        self.order.retain(|i| *i != idx);
        if self.inflight == Some(idx) {
            self.inflight = None;
        }
        Some(old)
    }

    fn clear(&mut self) {
        self.chunks.clear();
        self.order.clear();
        self.used_bytes = 0;
        self.inflight = None;
    }
}

struct BufferRt {
    max_bytes: u64,
    state: Mutex<BufState>,
}

struct Ctl {
    pause: bool,
    stop: bool,
    putting: bool,
    parked: bool,
    exclusive: bool,
    writers: u32,
}

struct Core<S> {
    inner: S,
    mode: WriteCacheMode,
    disk: Option<PathBuf>,
    dirty: Mutex<DirtyState>,
    file_lock: Mutex<()>,
    cache_max: u64,
    buffer: Option<BufferRt>,
    cv: Condvar,
    ctl: Mutex<Ctl>,
}

pub struct WriteCachedStore<S: BlockStore + 'static> {
    core: Arc<Core<S>>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

struct DirtyChunk {
    data: Vec<u8>,
    tick: u64,
    written_at: Instant,
}

struct DirtyState {
    chunks: HashMap<u64, DirtyChunk>,
    tick: u64,
    used_bytes: u64,
}

impl DirtyState {
    fn new() -> Self {
        Self {
            chunks: HashMap::new(),
            tick: 0,
            used_bytes: 0,
        }
    }

    fn from_recovered(map: HashMap<u64, Vec<u8>>) -> Self {
        let mut state = Self::new();
        for (idx, data) in map {
            state.insert(idx, data);
        }
        state
    }

    fn insert(&mut self, idx: u64, data: Vec<u8>) {
        self.tick = self.tick.wrapping_add(1);
        let size = data.len() as u64;
        if let Some(old) = self.chunks.insert(
            idx,
            DirtyChunk {
                data,
                tick: self.tick,
                written_at: Instant::now(),
            },
        ) {
            self.used_bytes = self.used_bytes.saturating_sub(old.data.len() as u64);
        }
        self.used_bytes = self.used_bytes.saturating_add(size);
    }

    fn remove(&mut self, idx: u64) -> Option<Vec<u8>> {
        let e = self.chunks.remove(&idx)?;
        self.used_bytes = self.used_bytes.saturating_sub(e.data.len() as u64);
        Some(e.data)
    }

    fn get(&self, idx: u64) -> Option<Vec<u8>> {
        self.chunks.get(&idx).map(|e| e.data.clone())
    }

    fn oldest(&self) -> Option<u64> {
        self.chunks
            .iter()
            .min_by_key(|(_, e)| e.tick)
            .map(|(k, _)| *k)
    }

    fn oldest_quiet(&self, quiet: Duration) -> Option<u64> {
        self.chunks
            .iter()
            .filter(|(_, c)| c.written_at.elapsed() >= quiet)
            .min_by_key(|(_, c)| c.written_at)
            .map(|(k, _)| *k)
    }
}

struct WriteHold<'a, S> {
    core: &'a Core<S>,
}

impl<S> Drop for WriteHold<'_, S> {
    fn drop(&mut self) {
        let mut ctl = self.core.ctl.lock();
        ctl.writers = ctl.writers.saturating_sub(1);
        self.core.cv.notify_all();
    }
}

impl<S: BlockStore + 'static> WriteCachedStore<S> {
    pub fn wrap(
        inner: S,
        cache: &WriteCacheConfig,
        buffer: &WriteBufferConfig,
        volume: &str,
    ) -> Result<Self, StoreError> {
        let (disk, dirty) = match cache.mode {
            WriteCacheMode::None => (None, DirtyState::new()),
            WriteCacheMode::Memory => {
                info!(
                    volume,
                    max_bytes = cache.max_bytes,
                    buffer_max = buffer.max_bytes,
                    "write cache: memory (volatile; flush on SYNCHRONIZE CACHE / shutdown / budget)"
                );
                (None, DirtyState::new())
            }
            WriteCacheMode::Disk => {
                let dir = cache.path.as_ref().ok_or_else(|| {
                    StoreError::Other("write_cache.mode=disk requires path".into())
                })?;
                let recovered = load_disk_cache(dir, volume, inner.chunk_size())?;
                info!(
                    volume,
                    path = %dir.display(),
                    recovered = recovered.len(),
                    max_bytes = cache.max_bytes,
                    buffer_max = buffer.max_bytes,
                    "write cache: disk (dirty chunks persist until flush)"
                );
                (Some(dir.clone()), DirtyState::from_recovered(recovered))
            }
        };
        let buffer_rt = if buffer.max_bytes > 0 && cache.mode != WriteCacheMode::None {
            Some(BufferRt {
                max_bytes: buffer.max_bytes,
                state: Mutex::new(BufState::new()),
            })
        } else {
            None
        };
        let core = Arc::new(Core {
            inner,
            mode: cache.mode,
            disk,
            dirty: Mutex::new(dirty),
            file_lock: Mutex::new(()),
            cache_max: cache.max_bytes,
            buffer: buffer_rt,
            cv: Condvar::new(),
            ctl: Mutex::new(Ctl {
                pause: false,
                stop: false,
                putting: false,
                parked: false,
                exclusive: false,
                writers: 0,
            }),
        });
        let store = Self {
            core: Arc::clone(&core),
            worker: Mutex::new(None),
        };
        if store.core.buffer.is_some() {
            let worker_core = Arc::clone(&core);
            let handle = thread::Builder::new()
                .name("iscsi-s3-wbuf".into())
                .spawn(move || worker_core.run_worker())
                .map_err(|e| StoreError::Other(format!("spawn write buffer: {e}")))?;
            *store.worker.lock() = Some(handle);
            store.evict_into_buffer()?;
        } else {
            store.evict_over_budget()?;
        }
        Ok(store)
    }

    pub fn inner(&self) -> &S {
        &self.core.inner
    }

    pub fn mode(&self) -> WriteCacheMode {
        self.core.mode
    }

    /// Drop dirty and queued snapshots without putting them (e.g. snapshot restore).
    /// An in-flight Put is allowed to finish first so it cannot land after restore.
    pub fn discard_dirty(&self) -> Result<(), StoreError> {
        if self.core.mode == WriteCacheMode::None {
            return Ok(());
        }
        self.core.with_exclusive(|| {
            self.core.pause_worker();
            let jobs = self.core.take_all_jobs();
            let _file = self.core.file_lock.lock();
            for job in jobs {
                self.core.remove_file(job.idx)?;
            }
            self.core.resume_worker();
            Ok(())
        })
    }

    pub fn dirty_count(&self) -> usize {
        if self.core.mode == WriteCacheMode::None {
            return 0;
        }
        self.core.dirty.lock().chunks.len()
    }

    pub fn max_bytes(&self) -> u64 {
        self.core.cache_max
    }

    pub fn dirty_bytes(&self) -> u64 {
        if self.core.mode == WriteCacheMode::None {
            return 0;
        }
        self.core.dirty.lock().used_bytes
    }

    pub fn buffer_bytes(&self) -> u64 {
        self.core
            .buffer
            .as_ref()
            .map(|b| b.state.lock().used_bytes)
            .unwrap_or(0)
    }

    pub fn buffer_chunks(&self) -> usize {
        self.core
            .buffer
            .as_ref()
            .map(|b| b.state.lock().chunks.len())
            .unwrap_or(0)
    }

    pub fn buffer_max_bytes(&self) -> u64 {
        self.core.buffer.as_ref().map(|b| b.max_bytes).unwrap_or(0)
    }

    fn evict_over_budget(&self) -> Result<(), StoreError> {
        self.core.evict_direct()
    }

    fn evict_into_buffer(&self) -> Result<(), StoreError> {
        self.core.evict_into_buffer()
    }

    fn stop_worker(&self) {
        {
            let mut ctl = self.core.ctl.lock();
            ctl.stop = true;
            ctl.pause = false;
            self.core.cv.notify_all();
        }
        if let Some(handle) = self.worker.lock().take() {
            let _ = handle.join();
        }
    }
}

impl WriteCachedStore<CachedStore<S3ChunkStore>> {
    pub fn s3(&self) -> &S3ChunkStore {
        self.core.inner.inner()
    }

    pub fn cache(&self) -> &Arc<ChunkCache> {
        self.core.inner.cache()
    }

    pub fn lock_io(&self) {
        if let Err(e) = self.flush() {
            warn!(error = %e, "flush write cache before I/O lock failed");
        }
        self.core.inner.lock_io();
    }

    pub fn unlock_io(&self) {
        self.core.inner.unlock_io();
    }

    pub fn is_io_locked(&self) -> bool {
        self.core.inner.is_io_locked()
    }
}

impl<S: BlockStore> BlockStore for WriteCachedStore<S> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), StoreError> {
        if self.core.mode == WriteCacheMode::None {
            return self.core.inner.read_at(offset, buf);
        }
        check_range(self.core.inner.capacity(), offset, buf.len())?;
        let chunk_size = self.core.inner.chunk_size();
        let mut done = 0usize;
        while done < buf.len() {
            let abs = offset + done as u64;
            let chunk_idx = abs / chunk_size;
            let within = (abs % chunk_size) as usize;
            let take = ((chunk_size as usize) - within).min(buf.len() - done);
            if let Some(chunk) = self.core.lookup(chunk_idx) {
                buf[done..done + take].copy_from_slice(&chunk[within..within + take]);
            } else {
                self.core
                    .inner
                    .read_at(abs, &mut buf[done..done + take])?;
            }
            done += take;
        }
        Ok(())
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), StoreError> {
        if self.core.mode == WriteCacheMode::None {
            return self.core.inner.write_at(offset, data);
        }
        let _hold = self.core.begin_write();
        check_range(self.core.inner.capacity(), offset, data.len())?;
        let chunk_size = self.core.inner.chunk_size();
        let mut done = 0usize;
        while done < data.len() {
            let abs = offset + done as u64;
            let chunk_idx = abs / chunk_size;
            let within = (abs % chunk_size) as usize;
            let take = ((chunk_size as usize) - within).min(data.len() - done);
            let mut chunk = self.core.load_for_write(chunk_idx)?;
            chunk[within..within + take].copy_from_slice(&data[done..done + take]);
            {
                let _file = self.core.file_lock.lock();
                self.core.persist(chunk_idx, &chunk)?;
                self.core.dirty.lock().insert(chunk_idx, chunk);
            }
            done += take;
        }
        if self.core.buffer.is_some() {
            self.core.evict_into_buffer()?;
        } else {
            self.core.evict_direct()?;
        }
        Ok(())
    }

    fn capacity(&self) -> u64 {
        self.core.inner.capacity()
    }

    fn set_capacity(&self, new_capacity: u64) -> Result<(), StoreError> {
        self.flush()?;
        self.core.inner.set_capacity(new_capacity)
    }

    fn flush(&self) -> Result<(), StoreError> {
        if self.core.mode == WriteCacheMode::None {
            return self.core.inner.flush();
        }
        self.core.with_exclusive(|| {
            self.core.pause_worker();
            let jobs = self.core.take_all_jobs();
            for job in jobs {
                self.core.write_snapshot(&job)?;
            }
            self.core.resume_worker();
            Ok(())
        })?;
        self.core.inner.flush()
    }

    fn block_size(&self) -> u32 {
        self.core.inner.block_size()
    }

    fn chunk_size(&self) -> u64 {
        self.core.inner.chunk_size()
    }

    fn present_chunks(&self) -> Result<Vec<u64>, StoreError> {
        let mut idxs = self.core.inner.present_chunks()?;
        if self.core.mode != WriteCacheMode::None {
            for idx in self.core.dirty.lock().chunks.keys() {
                if !idxs.contains(idx) {
                    idxs.push(*idx);
                }
            }
            if let Some(buf) = &self.core.buffer {
                for idx in buf.state.lock().chunks.keys() {
                    if !idxs.contains(idx) {
                        idxs.push(*idx);
                    }
                }
            }
            idxs.sort_unstable();
        }
        Ok(idxs)
    }

    fn delete_chunk(&self, index: u64) -> Result<(), StoreError> {
        if self.core.mode != WriteCacheMode::None {
            self.core.remove_cached(index);
            let _file = self.core.file_lock.lock();
            self.core.remove_file(index)?;
        }
        self.core.inner.delete_chunk(index)
    }
}

impl<S: BlockStore + 'static> Drop for WriteCachedStore<S> {
    fn drop(&mut self) {
        if self.core.mode == WriteCacheMode::None {
            return;
        }
        if let Err(e) = self.flush() {
            warn!(error = %e, "write cache flush on drop failed; dirty data may be lost");
        }
        self.stop_worker();
    }
}

impl<S: BlockStore> Core<S> {
    fn begin_write(&self) -> WriteHold<'_, S> {
        let mut ctl = self.ctl.lock();
        while ctl.exclusive {
            self.cv.wait(&mut ctl);
        }
        ctl.writers += 1;
        WriteHold { core: self }
    }

    fn with_exclusive(&self, f: impl FnOnce() -> Result<(), StoreError>) -> Result<(), StoreError> {
        {
            let mut ctl = self.ctl.lock();
            ctl.exclusive = true;
            self.cv.notify_all();
            while ctl.writers > 0 {
                self.cv.wait(&mut ctl);
            }
        }
        let result = f();
        {
            let mut ctl = self.ctl.lock();
            ctl.exclusive = false;
            self.cv.notify_all();
        }
        result
    }

    fn pause_worker(&self) {
        if self.buffer.is_none() {
            return;
        }
        let mut ctl = self.ctl.lock();
        ctl.pause = true;
        self.cv.notify_all();
        while !ctl.parked {
            self.cv.wait(&mut ctl);
        }
    }

    fn resume_worker(&self) {
        let mut ctl = self.ctl.lock();
        ctl.pause = false;
        self.cv.notify_all();
    }

    fn lookup(&self, idx: u64) -> Option<Vec<u8>> {
        if let Some(data) = self.dirty.lock().get(idx) {
            return Some(data);
        }
        self.buffer
            .as_ref()?
            .state
            .lock()
            .chunks
            .get(&idx)
            .map(|c| c.data.clone())
    }

    fn load_for_write(&self, idx: u64) -> Result<Vec<u8>, StoreError> {
        if let Some(data) = self.lookup(idx) {
            return Ok(data);
        }
        read_full_chunk(&self.inner, idx)
    }

    fn persist(&self, idx: u64, data: &[u8]) -> Result<(), StoreError> {
        let Some(dir) = &self.disk else {
            return Ok(());
        };
        write_chunk_file(dir, idx, data)
    }

    fn remove_file(&self, idx: u64) -> Result<(), StoreError> {
        let Some(dir) = &self.disk else {
            return Ok(());
        };
        let path = chunk_path(dir, idx);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StoreError::Other(format!(
                "remove write-cache {}: {e}",
                path.display()
            ))),
        }
    }

    fn write_inner(&self, idx: u64, data: &[u8]) -> Result<(), StoreError> {
        let chunk_size = self.inner.chunk_size();
        let capacity = self.inner.capacity();
        let offset = idx.saturating_mul(chunk_size);
        if offset < capacity {
            let valid = ((capacity - offset) as usize).min(data.len());
            self.inner.write_at(offset, &data[..valid])?;
        }
        Ok(())
    }

    fn write_snapshot(&self, job: &Job) -> Result<(), StoreError> {
        self.write_inner(job.idx, &job.data)?;
        let _file = self.file_lock.lock();
        if !self.dirty.lock().chunks.contains_key(&job.idx) {
            self.remove_file(job.idx)?;
        }
        Ok(())
    }

    fn take_all_jobs(&self) -> Vec<Job> {
        let mut dirty = self.dirty.lock();
        let dirty_jobs: Vec<Job> = dirty
            .chunks
            .drain()
            .map(|(idx, c)| Job {
                idx,
                data: c.data,
                generation: c.tick,
            })
            .collect();
        dirty.used_bytes = 0;
        let dirty_idxs: HashSet<u64> = dirty_jobs.iter().map(|j| j.idx).collect();
        let mut jobs = Vec::new();
        if let Some(buf) = &self.buffer {
            let mut state = buf.state.lock();
            for (idx, chunk) in state.chunks.drain() {
                if !dirty_idxs.contains(&idx) {
                    jobs.push(Job {
                        idx,
                        data: chunk.data,
                        generation: chunk.generation,
                    });
                }
            }
            state.clear();
        }
        jobs.extend(dirty_jobs);
        jobs
    }

    fn evict_direct(&self) -> Result<(), StoreError> {
        if self.cache_max == 0 || self.mode == WriteCacheMode::None {
            return Ok(());
        }
        loop {
            let victim = {
                let dirty = self.dirty.lock();
                if dirty.used_bytes <= self.cache_max {
                    return Ok(());
                }
                dirty.oldest()
            };
            let Some(idx) = victim else {
                return Ok(());
            };
            let Some(data) = self.dirty.lock().get(idx) else {
                continue;
            };
            self.write_inner(idx, &data)?;
            let _file = self.file_lock.lock();
            self.dirty.lock().remove(idx);
            self.remove_file(idx)?;
        }
    }

    fn evict_into_buffer(&self) -> Result<(), StoreError> {
        let Some(buf) = &self.buffer else {
            return self.evict_direct();
        };
        if self.cache_max == 0 {
            return Ok(());
        }
        loop {
            let moved = {
                let mut dirty = self.dirty.lock();
                if dirty.used_bytes <= self.cache_max {
                    return Ok(());
                }
                let Some(idx) = dirty.oldest() else {
                    return Ok(());
                };
                let size = dirty.chunks.get(&idx).map(|c| c.data.len() as u64).unwrap_or(0);
                let generation = dirty.chunks.get(&idx).map(|c| c.tick).unwrap_or(0);
                let mut state = buf.state.lock();
                if state.inflight == Some(idx) || state.chunks.contains_key(&idx) {
                    false
                } else if state.used_bytes.saturating_add(size) > buf.max_bytes {
                    false
                } else if let Some(data) = dirty.remove(idx) {
                    state.push(idx, data, generation);
                    true
                } else {
                    false
                }
            };
            if moved {
                self.cv.notify_all();
                continue;
            }
            let mut ctl = self.ctl.lock();
            if ctl.stop {
                return Ok(());
            }
            self.cv.wait(&mut ctl);
        }
    }

    fn handoff_quiet(&self) -> bool {
        let Some(buf) = &self.buffer else {
            return false;
        };
        let mut dirty = self.dirty.lock();
        let Some(idx) = dirty.oldest_quiet(QUIET) else {
            return false;
        };
        let size = dirty.chunks.get(&idx).map(|c| c.data.len() as u64).unwrap_or(0);
        let generation = dirty.chunks.get(&idx).map(|c| c.tick).unwrap_or(0);
        let mut state = buf.state.lock();
        if state.inflight == Some(idx) || state.chunks.contains_key(&idx) {
            return false;
        }
        if state.used_bytes.saturating_add(size) > buf.max_bytes {
            return false;
        }
        let Some(data) = dirty.remove(idx) else {
            return false;
        };
        state.push(idx, data, generation);
        true
    }

    fn next_job(&self) -> Option<Job> {
        let buf = self.buffer.as_ref()?;
        let mut state = buf.state.lock();
        if state.inflight.is_some() {
            return None;
        }
        let idx = *state.order.front()?;
        let chunk = state.chunks.get(&idx)?;
        let job = Job {
            idx,
            data: chunk.data.clone(),
            generation: chunk.generation,
        };
        state.inflight = Some(idx);
        Some(job)
    }

    fn clear_inflight(&self, idx: u64) {
        if let Some(buf) = &self.buffer {
            let mut state = buf.state.lock();
            if state.inflight == Some(idx) {
                state.inflight = None;
            }
        }
    }

    fn finish_put(&self, job: Job) -> Result<(), StoreError> {
        let result = self.write_inner(job.idx, &job.data);
        {
            let _file = self.file_lock.lock();
            if let Some(buf) = &self.buffer {
                let mut state = buf.state.lock();
                if state.inflight == Some(job.idx) {
                    state.inflight = None;
                }
                if result.is_ok()
                    && state.chunks.get(&job.idx).map(|c| c.generation) == Some(job.generation)
                {
                    state.remove(job.idx);
                }
            }
            if result.is_ok() && !self.dirty.lock().chunks.contains_key(&job.idx) {
                self.remove_file(job.idx)?;
            }
        }
        self.cv.notify_all();
        result
    }

    fn remove_cached(&self, index: u64) {
        loop {
            let inflight = self
                .buffer
                .as_ref()
                .and_then(|b| b.state.lock().inflight);
            if inflight == Some(index) {
                let mut ctl = self.ctl.lock();
                self.cv.wait(&mut ctl);
                continue;
            }
            self.dirty.lock().remove(index);
            if let Some(buf) = &self.buffer {
                buf.state.lock().remove(index);
            }
            break;
        }
    }

    fn run_worker(self: &Arc<Self>) {
        loop {
            {
                let mut ctl = self.ctl.lock();
                if ctl.stop {
                    return;
                }
                if ctl.pause {
                    ctl.parked = true;
                    ctl.putting = false;
                    self.cv.notify_all();
                    while ctl.pause && !ctl.stop {
                        self.cv.wait(&mut ctl);
                    }
                    ctl.parked = false;
                    continue;
                }
            }
            if let Some(job) = self.next_job() {
                let aborted = {
                    let ctl = self.ctl.lock();
                    ctl.pause || ctl.stop
                };
                if aborted {
                    self.clear_inflight(job.idx);
                    continue;
                }
                {
                    let mut ctl = self.ctl.lock();
                    ctl.putting = true;
                }
                if let Err(e) = self.finish_put(job) {
                    warn!(error = %e, "write buffer put failed; chunk stays queued");
                }
                {
                    let mut ctl = self.ctl.lock();
                    ctl.putting = false;
                    self.cv.notify_all();
                }
                continue;
            }
            if self.handoff_quiet() {
                self.cv.notify_all();
                continue;
            }
            let mut ctl = self.ctl.lock();
            if ctl.stop || ctl.pause {
                continue;
            }
            self.cv.wait_for(&mut ctl, QUIET);
        }
    }
}

fn read_full_chunk(store: &impl BlockStore, idx: u64) -> Result<Vec<u8>, StoreError> {
    let chunk_size = store.chunk_size();
    let mut buf = vec![0u8; chunk_size as usize];
    let offset = idx.saturating_mul(chunk_size);
    let capacity = store.capacity();
    if offset >= capacity {
        return Err(StoreError::OutOfRange {
            offset,
            len: chunk_size,
            capacity,
        });
    }
    let valid = ((capacity - offset) as usize).min(buf.len());
    store.read_at(offset, &mut buf[..valid])?;
    Ok(buf)
}

fn chunk_path(dir: &Path, idx: u64) -> PathBuf {
    dir.join(format!("{idx:016x}"))
}

fn write_chunk_file(dir: &Path, idx: u64, data: &[u8]) -> Result<(), StoreError> {
    fs::create_dir_all(dir).map_err(|e| {
        StoreError::Other(format!("create write-cache dir {}: {e}", dir.display()))
    })?;
    let dest = chunk_path(dir, idx);
    let tmp = dest.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp).map_err(|e| {
            StoreError::Other(format!("create {}: {e}", tmp.display()))
        })?;
        f.write_all(data)
            .map_err(|e| StoreError::Other(format!("write {}: {e}", tmp.display())))?;
        f.sync_all()
            .map_err(|e| StoreError::Other(format!("sync {}: {e}", tmp.display())))?;
    }
    fs::rename(&tmp, &dest)
        .map_err(|e| StoreError::Other(format!("rename {} -> {}: {e}", tmp.display(), dest.display())))?;
    Ok(())
}

fn load_disk_cache(
    dir: &Path,
    volume: &str,
    chunk_size: u64,
) -> Result<HashMap<u64, Vec<u8>>, StoreError> {
    fs::create_dir_all(dir).map_err(|e| {
        StoreError::Other(format!("create write-cache dir {}: {e}", dir.display()))
    })?;
    let stamp_path = dir.join(STAMP_NAME);
    if stamp_path.exists() {
        let text = fs::read_to_string(&stamp_path).map_err(|e| {
            StoreError::Other(format!("read {}: {e}", stamp_path.display()))
        })?;
        let stamp: WriteCacheStamp = serde_json::from_str(&text).map_err(|e| {
            StoreError::Other(format!("parse {}: {e}", stamp_path.display()))
        })?;
        if stamp.volume != volume {
            return Err(StoreError::Other(format!(
                "write-cache {} belongs to volume {:?}, not {volume:?}",
                dir.display(),
                stamp.volume
            )));
        }
        if stamp.chunk_size != chunk_size {
            return Err(StoreError::Other(format!(
                "write-cache {} chunk_size {} != volume {chunk_size}",
                dir.display(),
                stamp.chunk_size
            )));
        }
    } else {
        let stamp = WriteCacheStamp {
            volume: volume.to_string(),
            chunk_size,
        };
        let text = serde_json::to_string_pretty(&stamp)
            .map_err(|e| StoreError::Other(format!("stamp json: {e}")))?;
        fs::write(&stamp_path, text).map_err(|e| {
            StoreError::Other(format!("write {}: {e}", stamp_path.display()))
        })?;
    }

    let mut dirty = HashMap::new();
    for ent in fs::read_dir(dir)
        .map_err(|e| StoreError::Other(format!("read write-cache dir {}: {e}", dir.display())))?
    {
        let ent = ent.map_err(|e| StoreError::Other(format!("read_dir: {e}")))?;
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name.ends_with(".tmp") {
            continue;
        }
        if name.len() != 16 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let idx = u64::from_str_radix(&name, 16)
            .map_err(|e| StoreError::Other(format!("bad write-cache name {name}: {e}")))?;
        let bytes = fs::read(ent.path())
            .map_err(|e| StoreError::Other(format!("read {}: {e}", ent.path().display())))?;
        if bytes.len() as u64 != chunk_size {
            return Err(StoreError::Other(format!(
                "write-cache {} length {} != chunk_size {chunk_size}",
                ent.path().display(),
                bytes.len()
            )));
        }
        dirty.insert(idx, bytes);
    }
    Ok(dirty)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct WriteCacheStamp {
    volume: String,
    chunk_size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;
    use tempfile::tempdir;

    fn mem() -> MemoryStore {
        MemoryStore::new(8192, 512, 4096).unwrap()
    }

    fn wc(mode: WriteCacheMode, max_bytes: u64) -> WriteCacheConfig {
        WriteCacheConfig {
            mode,
            path: None,
            max_bytes,
        }
    }

    fn open(inner: MemoryStore, cfg: &WriteCacheConfig) -> WriteCachedStore<MemoryStore> {
        WriteCachedStore::wrap(inner, cfg, &WriteBufferConfig::default(), "disk0").unwrap()
    }

    fn open_buf<S: BlockStore + 'static>(
        inner: S,
        cache_max: u64,
        buffer_max: u64,
    ) -> WriteCachedStore<S> {
        WriteCachedStore::wrap(
            inner,
            &wc(WriteCacheMode::Memory, cache_max),
            &WriteBufferConfig {
                max_bytes: buffer_max,
            },
            "disk0",
        )
        .unwrap()
    }

    #[test]
    fn none_writes_through() {
        let store = open(mem(), &wc(WriteCacheMode::None, 0));
        store.write_at(0, &[7u8; 512]).unwrap();
        assert_eq!(store.dirty_count(), 0);
        let mut buf = [0u8; 512];
        store.inner().read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [7u8; 512]);
    }

    #[test]
    fn memory_defers_until_flush() {
        let store = open(mem(), &wc(WriteCacheMode::Memory, 0));
        store.write_at(100, &[2u8; 50]).unwrap();
        assert_eq!(store.dirty_count(), 1);
        let mut inner = [0u8; 50];
        store.inner().read_at(100, &mut inner).unwrap();
        assert_eq!(inner, [0u8; 50]);
        let mut seen = [0u8; 50];
        store.read_at(100, &mut seen).unwrap();
        assert_eq!(seen, [2u8; 50]);
        store.flush().unwrap();
        assert_eq!(store.dirty_count(), 0);
        store.inner().read_at(100, &mut inner).unwrap();
        assert_eq!(inner, [2u8; 50]);
    }

    #[test]
    fn memory_rmw_preserves_neighbors() {
        let store = open(mem(), &wc(WriteCacheMode::Memory, 0));
        store.write_at(0, &[1u8; 4096]).unwrap();
        store.write_at(100, &[2u8; 50]).unwrap();
        store.flush().unwrap();
        let mut buf = [0u8; 1];
        store.read_at(99, &mut buf).unwrap();
        assert_eq!(buf[0], 1);
        store.read_at(100, &mut buf).unwrap();
        assert_eq!(buf[0], 2);
        store.read_at(150, &mut buf).unwrap();
        assert_eq!(buf[0], 1);
    }

    #[test]
    fn disk_recovers_dirty_after_reopen() {
        let dir = tempdir().unwrap();
        let cfg = WriteCacheConfig {
            mode: WriteCacheMode::Disk,
            path: Some(dir.path().to_path_buf()),
            max_bytes: 0,
        };
        {
            let store = WriteCachedStore::wrap(mem(), &cfg, &WriteBufferConfig::default(), "disk0").unwrap();
            store.write_at(0, &[9u8; 512]).unwrap();
            assert_eq!(store.dirty_count(), 1);
            // Leak without Drop flush: forget the wrapper after persisting.
            std::mem::forget(store);
        }
        let store = WriteCachedStore::wrap(mem(), &cfg, &WriteBufferConfig::default(), "disk0").unwrap();
        assert_eq!(store.dirty_count(), 1);
        let mut buf = [0u8; 512];
        store.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [9u8; 512]);
        store.flush().unwrap();
        assert_eq!(store.dirty_count(), 0);
    }

    #[test]
    fn disk_reopen_evicts_over_budget() {
        let dir = tempdir().unwrap();
        let unlimited = WriteCacheConfig {
            mode: WriteCacheMode::Disk,
            path: Some(dir.path().to_path_buf()),
            max_bytes: 0,
        };
        {
            let store = WriteCachedStore::wrap(mem(), &unlimited, &WriteBufferConfig::default(), "disk0").unwrap();
            store.write_at(0, &[1u8; 512]).unwrap();
            store.write_at(4096, &[2u8; 512]).unwrap();
            assert_eq!(store.dirty_count(), 2);
            std::mem::forget(store);
        }
        let capped = WriteCacheConfig {
            mode: WriteCacheMode::Disk,
            path: Some(dir.path().to_path_buf()),
            max_bytes: 4096,
        };
        let store = WriteCachedStore::wrap(mem(), &capped, &WriteBufferConfig::default(), "disk0").unwrap();
        assert_eq!(store.dirty_count(), 1);
        assert_eq!(store.dirty_bytes(), 4096);
    }

    #[test]
    fn present_chunks_includes_dirty_only() {
        let store = open(mem(), &wc(WriteCacheMode::Memory, 0));
        store.write_at(4096, &[3u8; 512]).unwrap();
        assert_eq!(store.present_chunks().unwrap(), vec![1]);
        assert!(store.inner().present_chunks().unwrap().is_empty());
    }

    #[test]
    fn memory_evicts_oldest_when_over_budget() {
        let store = open(mem(), &wc(WriteCacheMode::Memory, 4096));
        store.write_at(0, &[1u8; 512]).unwrap();
        assert_eq!(store.dirty_count(), 1);
        store.write_at(4096, &[2u8; 512]).unwrap();
        assert_eq!(store.dirty_count(), 1);
        assert_eq!(store.dirty_bytes(), 4096);
        let mut inner = [0u8; 512];
        store.inner().read_at(0, &mut inner).unwrap();
        assert_eq!(inner, [1u8; 512]);
        store.inner().read_at(4096, &mut inner).unwrap();
        assert_eq!(inner, [0u8; 512]);
        store.read_at(4096, &mut inner).unwrap();
        assert_eq!(inner, [2u8; 512]);
    }

    struct Gate {
        open: Mutex<bool>,
        cv: Condvar,
    }

    impl Gate {
        fn closed() -> Arc<Self> {
            Arc::new(Self {
                open: Mutex::new(false),
                cv: Condvar::new(),
            })
        }

        fn release(&self) {
            *self.open.lock() = true;
            self.cv.notify_all();
        }
    }

    /// Opens the gate when dropped, so a blocked put cannot hang store Drop.
    struct Release(Arc<Gate>);

    impl Drop for Release {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    struct GateStore {
        inner: MemoryStore,
        gate: Arc<Gate>,
    }

    impl GateStore {
        fn new(gate: Arc<Gate>) -> Self {
            Self {
                inner: MemoryStore::new(8192, 512, 4096).unwrap(),
                gate,
            }
        }
    }

    impl BlockStore for GateStore {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), StoreError> {
            self.inner.read_at(offset, buf)
        }
        fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), StoreError> {
            let mut open = self.gate.open.lock();
            while !*open {
                self.gate.cv.wait(&mut open);
            }
            self.inner.write_at(offset, data)
        }
        fn capacity(&self) -> u64 {
            self.inner.capacity()
        }
        fn set_capacity(&self, new_capacity: u64) -> Result<(), StoreError> {
            self.inner.set_capacity(new_capacity)
        }
        fn flush(&self) -> Result<(), StoreError> {
            self.inner.flush()
        }
        fn block_size(&self) -> u32 {
            self.inner.block_size()
        }
        fn chunk_size(&self) -> u64 {
            self.inner.chunk_size()
        }
        fn present_chunks(&self) -> Result<Vec<u64>, StoreError> {
            self.inner.present_chunks()
        }
        fn delete_chunk(&self, index: u64) -> Result<(), StoreError> {
            self.inner.delete_chunk(index)
        }
    }

    fn fill_two(store: &WriteCachedStore<GateStore>) {
        store.write_at(0, &[1u8; 512]).unwrap();
        store.write_at(4096, &[2u8; 512]).unwrap();
    }

    #[test]
    fn buffer_handoff_stays_readable() {
        let gate = Gate::closed();
        let store = open_buf(GateStore::new(Arc::clone(&gate)), 4096, 8192);
        let _release = Release(Arc::clone(&gate));
        fill_two(&store);
        assert_eq!(store.dirty_count(), 1);
        assert_eq!(store.buffer_chunks(), 1);
        let mut buf = [0u8; 512];
        store.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [1u8; 512]);
        store.inner().read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [0u8; 512]);
    }

    #[test]
    fn buffer_rewrite_survives_inflight_put() {
        let gate = Gate::closed();
        let store = open_buf(GateStore::new(Arc::clone(&gate)), 4096, 8192);
        let _release = Release(Arc::clone(&gate));
        fill_two(&store);
        store.write_at(0, &[9u8; 512]).unwrap();
        gate.release();
        let mut seen = [0u8; 512];
        store.read_at(0, &mut seen).unwrap();
        assert_eq!(seen, [9u8; 512]);
        assert!(store.dirty_count() >= 1);
    }

    #[test]
    fn buffer_flush_waits_for_inflight_put() {
        let gate = Gate::closed();
        let store = Arc::new(open_buf(GateStore::new(Arc::clone(&gate)), 4096, 8192));
        let _release = Release(Arc::clone(&gate));
        fill_two(&store);
        let flushing = Arc::clone(&store);
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&done);
        let handle = thread::spawn(move || {
            flushing.flush().unwrap();
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        thread::sleep(Duration::from_millis(50));
        assert!(!done.load(std::sync::atomic::Ordering::SeqCst));
        gate.release();
        handle.join().unwrap();
        let mut buf = [0u8; 512];
        store.inner().read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [1u8; 512]);
        store.inner().read_at(4096, &mut buf).unwrap();
        assert_eq!(buf, [2u8; 512]);
    }

    #[test]
    fn buffer_discard_does_not_put_queued() {
        let gate = Gate::closed();
        let store = Arc::new(open_buf(GateStore::new(Arc::clone(&gate)), 4096, 8192));
        let _release = Release(Arc::clone(&gate));
        fill_two(&store);
        store.write_at(0, &[9u8; 512]).unwrap();
        assert_eq!(store.buffer_chunks(), 2);
        let discarding = Arc::clone(&store);
        let handle = thread::spawn(move || discarding.discard_dirty().unwrap());
        thread::sleep(Duration::from_millis(50));
        gate.release();
        handle.join().unwrap();
        let mut buf = [0u8; 512];
        store.inner().read_at(4096, &mut buf).unwrap();
        assert_eq!(buf, [0u8; 512]);
    }
}
