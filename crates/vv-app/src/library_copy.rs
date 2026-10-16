//! Copies of library media into the project's folder, in the background:
//! a video can take a while, and until its copy is ready the project keeps
//! using the library's file.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use vv_core::MediaId;
use vv_session::worker::Worker;

struct Job {
    epoch: u64,
    media: MediaId,
    src: PathBuf,
    dir: PathBuf,
}

pub(crate) struct CopyDone {
    /// Of the session the copy was asked by: a project opened meanwhile
    /// has other media under the same ids.
    pub epoch: u64,
    pub media: MediaId,
    pub src: PathBuf,
    pub result: Result<PathBuf, String>,
}

#[derive(Default)]
struct Shared {
    copied: AtomicU64,
    current: Mutex<String>,
    cancel: AtomicBool,
}

pub(crate) struct CopyProgress {
    pub name: String,
    pub fraction: f32,
    pub files: usize,
}

pub(crate) struct ProjectCopier {
    shared: Arc<Shared>,
    rx: mpsc::Receiver<CopyDone>,
    queued: HashSet<(u64, MediaId)>,
    /// Bytes of the files queued since the queue was last empty.
    total: u64,
    worker: Worker<Job>,
}

impl ProjectCopier {
    pub fn spawn() -> Self {
        let shared = Arc::new(Shared::default());
        let (done_tx, rx) = mpsc::channel();
        let worker_shared = shared.clone();
        let worker = Worker::spawn(move |jobs: mpsc::Receiver<Job>| {
            while let Ok(job) = jobs.recv() {
                if let Some(name) = job.src.file_name() {
                    *worker_shared.current.lock().unwrap() = name.to_string_lossy().into_owned();
                }
                let result = crate::library::copy_for_project(&job.src, &job.dir, &|bytes| {
                    worker_shared.copied.fetch_add(bytes, Ordering::Relaxed);
                    !worker_shared.cancel.load(Ordering::Relaxed)
                });
                let done = CopyDone {
                    epoch: job.epoch,
                    media: job.media,
                    src: job.src,
                    result,
                };
                if done_tx.send(done).is_err() {
                    return;
                }
            }
        });
        Self {
            shared,
            rx,
            queued: HashSet::new(),
            total: 0,
            worker,
        }
    }

    /// A media already waiting for its copy is not queued again.
    pub fn enqueue(&mut self, epoch: u64, media: MediaId, src: PathBuf, dir: PathBuf) {
        if !self.queued.insert((epoch, media)) {
            return;
        }
        self.total += std::fs::metadata(&src).map_or(0, |m| m.len());
        let job = Job {
            epoch,
            media,
            src,
            dir,
        };
        if !self.worker.send(job) {
            self.queued.remove(&(epoch, media));
        }
    }

    pub fn poll(&mut self) -> Vec<CopyDone> {
        let done: Vec<CopyDone> = self.rx.try_iter().collect();
        for d in &done {
            self.queued.remove(&(d.epoch, d.media));
        }
        if self.queued.is_empty() {
            self.total = 0;
            self.shared.copied.store(0, Ordering::Relaxed);
        }
        done
    }

    /// `None` with nothing to copy.
    pub fn progress(&self) -> Option<CopyProgress> {
        if self.queued.is_empty() {
            return None;
        }
        let copied = self.shared.copied.load(Ordering::Relaxed);
        Some(CopyProgress {
            name: self.shared.current.lock().unwrap().clone(),
            fraction: (copied as f64 / self.total.max(1) as f64).min(1.0) as f32,
            files: self.queued.len(),
        })
    }
}

impl Drop for ProjectCopier {
    /// Closing the program must not wait for a long copy: the next save
    /// starts it over.
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }
}
