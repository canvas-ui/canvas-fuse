use crate::api::ApiClient;
use fuser::ReplyData;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;

/// One pending kernel read waiting for a blob to arrive.
pub struct PendingRead {
    pub offset: i64,
    pub size: u32,
    pub reply: ReplyData,
}

/// In-memory LRU blob cache + fetch pool. FUSE read callbacks must not block
/// the (single-threaded) session loop on the network, so cache misses hand
/// their ReplyData to a fetch worker and return immediately. Concurrent reads
/// of the same blob (kernel readahead issues many) are deduplicated: one
/// download, all waiters answered from it.
pub struct BlobStore {
    api: Arc<ApiClient>,
    cache: Mutex<Lru>,
    /// blobs currently being downloaded -> reads waiting on them
    in_flight: Mutex<HashMap<String, Vec<PendingRead>>>,
    fetch_tx: Sender<FetchJob>,
}

struct FetchJob {
    key: String,
    workspace_id: String,
    doc_id: u64,
}

struct Lru {
    map: HashMap<String, Arc<Vec<u8>>>,
    order: VecDeque<String>,
    used: usize,
    budget: usize,
}

impl Lru {
    fn get(&mut self, key: &str) -> Option<Arc<Vec<u8>>> {
        let hit = self.map.get(key).cloned();
        if hit.is_some() {
            // bump recency
            if let Some(pos) = self.order.iter().position(|k| k == key) {
                let k = self.order.remove(pos).unwrap();
                self.order.push_back(k);
            }
        }
        hit
    }

    fn put(&mut self, key: String, blob: Arc<Vec<u8>>) {
        if self.map.contains_key(&key) {
            return;
        }
        self.used += blob.len();
        self.map.insert(key.clone(), blob);
        self.order.push_back(key);
        while self.used > self.budget && self.order.len() > 1 {
            if let Some(old) = self.order.pop_front() {
                if let Some(b) = self.map.remove(&old) {
                    self.used -= b.len();
                }
            }
        }
    }
}

pub fn reply_slice(reply: ReplyData, blob: &[u8], offset: i64, size: u32) {
    let start = (offset.max(0) as usize).min(blob.len());
    let end = (start + size as usize).min(blob.len());
    reply.data(&blob[start..end]);
}

impl BlobStore {
    pub fn new(api: Arc<ApiClient>, budget_bytes: usize, workers: usize) -> Arc<Self> {
        let (fetch_tx, fetch_rx) = channel::<FetchJob>();
        let store = Arc::new(Self {
            api,
            cache: Mutex::new(Lru {
                map: HashMap::new(),
                order: VecDeque::new(),
                used: 0,
                budget: budget_bytes.max(8 * 1024 * 1024),
            }),
            in_flight: Mutex::new(HashMap::new()),
            fetch_tx,
        });

        // Single dispatcher feeding a small pool keeps the channel simple;
        // jobs are already deduped per blob so workers do real downloads only
        let pool: Vec<Sender<FetchJob>> = (0..workers.max(1))
            .map(|i| {
                let (tx, rx) = channel::<FetchJob>();
                let store = store.clone();
                std::thread::Builder::new()
                    .name(format!("canvas-fuse-fetch-{i}"))
                    .spawn(move || {
                        while let Ok(job) = rx.recv() {
                            store.run_fetch(job);
                        }
                    })
                    .expect("spawning fetch worker");
                tx
            })
            .collect();
        {
            let rx = fetch_rx;
            std::thread::Builder::new()
                .name("canvas-fuse-fetch-dispatch".into())
                .spawn(move || {
                    let mut next = 0usize;
                    while let Ok(job) = rx.recv() {
                        if pool[next % pool.len()].send(job).is_err() {
                            break;
                        }
                        next += 1;
                    }
                })
                .expect("spawning fetch dispatcher");
        }
        store
    }

    /// Serve a read: from cache if possible, otherwise queue it on the blob's
    /// (possibly new) download. Never blocks on the network.
    pub fn read(
        &self,
        key: &str,
        workspace_id: &str,
        doc_id: u64,
        offset: i64,
        size: u32,
        reply: ReplyData,
    ) {
        if let Some(blob) = self.cache.lock().get(key) {
            reply_slice(reply, &blob, offset, size);
            return;
        }
        let pending = PendingRead {
            offset,
            size,
            reply,
        };
        let mut in_flight = self.in_flight.lock();
        if let Some(waiters) = in_flight.get_mut(key) {
            waiters.push(pending);
            return;
        }
        in_flight.insert(key.to_string(), vec![pending]);
        drop(in_flight);
        let _ = self.fetch_tx.send(FetchJob {
            key: key.to_string(),
            workspace_id: workspace_id.to_string(),
            doc_id,
        });
    }

    fn run_fetch(&self, job: FetchJob) {
        let result = self.api.fetch_content(&job.workspace_id, job.doc_id);
        let waiters = self.in_flight.lock().remove(&job.key).unwrap_or_default();
        match result {
            Ok(bytes) => {
                let blob = Arc::new(bytes);
                log::debug!(
                    "fetched blob {} ({} bytes, {} waiting reads)",
                    job.key,
                    blob.len(),
                    waiters.len()
                );
                self.cache.lock().put(job.key, blob.clone());
                for w in waiters {
                    reply_slice(w.reply, &blob, w.offset, w.size);
                }
            }
            Err(e) => {
                log::warn!("blob fetch {} failed: {e:#}", job.key);
                for w in waiters {
                    w.reply.error(libc::EIO);
                }
            }
        }
    }
}
