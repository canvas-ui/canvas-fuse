//! Startup diagnostics stay alive even while a single disk operation blocks.
//! This observes preparation only; it never schedules scans or network work.

use anyhow::Result;
use parking_lot::Mutex;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Status {
    phase: &'static str,
    checked: u64,
    hashing: u64,
    bytes: u64,
}

pub(crate) struct StartupProgress {
    status: Arc<Mutex<Status>>,
    started: Instant,
    report: Arc<dyn Fn(String) + Send + Sync>,
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl StartupProgress {
    pub(crate) fn start() -> Result<Self> {
        Self::with_reporter(
            Duration::from_secs(5),
            Arc::new(|line| log::info!("{line}")),
        )
    }

    fn with_reporter(
        interval: Duration,
        report: Arc<dyn Fn(String) + Send + Sync>,
    ) -> Result<Self> {
        let started = Instant::now();
        let status = Arc::new(Mutex::new(Status {
            phase: "validating local mirror",
            ..Status::default()
        }));
        let (stop, rx) = mpsc::channel();
        let thread_status = status.clone();
        let thread_report = report.clone();
        report(Self::message(&status.lock(), started));
        let thread = std::thread::Builder::new()
            .name("mirror-startup-progress".into())
            .spawn(move || {
                while matches!(
                    rx.recv_timeout(interval),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    thread_report(Self::message(&thread_status.lock(), started));
                }
            })?;
        Ok(Self {
            status,
            started,
            report,
            stop,
            thread: Some(thread),
        })
    }

    fn message(status: &Status, started: Instant) -> String {
        format!(
            "mirror startup: {}; {} files checked, {} files needing hashing, {} MiB read ({:.1}s elapsed)",
            status.phase, status.checked, status.hashing, status.bytes / (1024 * 1024),
            started.elapsed().as_secs_f64()
        )
    }

    pub(crate) fn phase(&self, phase: &'static str) {
        let mut status = self.status.lock();
        status.phase = phase;
        (self.report)(Self::message(&status, self.started));
    }

    pub(crate) fn checked(&self) {
        self.status.lock().checked += 1;
    }
    pub(crate) fn hashing(&self) {
        self.status.lock().hashing += 1;
    }
    pub(crate) fn read(&self, bytes: usize) {
        self.status.lock().bytes += bytes as u64;
    }

    pub(crate) fn finish(mut self) {
        self.stop();
        self.phase("local preparation complete");
    }

    fn stop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for StartupProgress {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mirror::sync::{Mirror, MirrorConfig};
    use crate::mirror::{DeviceIdentity, MirrorOptions};

    #[test]
    fn real_scan_reports_checked_files_and_hash_bytes_without_changing_reconciliation() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = || MirrorConfig {
            data_dir: dir.path().join("state"),
            home_dir: dir.path().join("mount/Home"),
            server: "http://127.0.0.1:9".into(),
            token: "test".into(),
            tls: None,
            workspace_id: "ws".into(),
            backend: "workspace:home".into(),
            opts: MirrorOptions::default(),
            device: DeviceIdentity {
                id: "test".into(),
                name: "test".into(),
            },
            mountpoint: dir.path().join("mount"),
            status_path: None,
        };
        drop(Mirror::open(cfg()).unwrap());
        let bytes = vec![42; 2 * 1024 * 1024 + 17];
        std::fs::write(cfg().home_dir.join("offline.bin"), &bytes).unwrap();
        for changed in [true, false, true] {
            let progress =
                StartupProgress::with_reporter(Duration::from_secs(5), Arc::new(|_| {})).unwrap();
            let mirror = Mirror::open_with_progress(cfg(), &progress).unwrap();
            {
                let status = progress.status.lock();
                assert_eq!(status.checked, 1);
                assert_eq!(status.hashing, u64::from(changed));
                assert_eq!(status.bytes, if changed { bytes.len() as u64 } else { 0 });
            }
            assert_eq!(mirror.store.jobs().len(), 1, "offline upload stays queued");
            assert_eq!(
                mirror.entry("offline.bin").unwrap().sha256,
                crate::mirror::sha256_hex(&bytes)
            );
            if !changed {
                // A reboot may change only the device component of a cached
                // fingerprint. Startup must report the resulting reread and
                // preserve the entry/queue when its digest is unchanged.
                let stamp = mirror.store.meta("local-stamp:offline.bin").unwrap();
                let (_, rest) = stamp.split_once(':').unwrap();
                mirror
                    .store
                    .set_meta("local-stamp:offline.bin", &format!("old-device:{rest}"))
                    .unwrap();
            }
            progress.finish();
            drop(mirror);
        }
    }

    #[test]
    fn reports_while_work_is_blocked_and_stops_on_success_or_error() {
        for success in [true, false] {
            let (tx, rx) = mpsc::channel();
            let progress = StartupProgress::with_reporter(
                Duration::from_millis(10),
                Arc::new(move |line| {
                    tx.send(line).unwrap();
                }),
            )
            .unwrap();
            assert!(rx.recv().unwrap().contains("validating local mirror"));
            progress.phase("checking local files for offline changes");
            progress.checked();
            progress.hashing();
            progress.read(2 * 1024 * 1024);
            // No subsequent work is needed to emit progress, even for one
            // large file or a blocked filesystem call. Wait for its heartbeat.
            loop {
                let line = rx.recv_timeout(Duration::from_secs(2)).unwrap();
                if line.contains("1 files checked, 1 files needing hashing, 2 MiB read") {
                    assert!(line.contains("checking local files for offline changes"));
                    break;
                }
            }
            if success {
                progress.finish();
            } else {
                drop(progress);
            }
            let remaining: Vec<_> = rx.iter().collect();
            assert_eq!(
                remaining
                    .iter()
                    .any(|line| line.contains("local preparation complete")),
                success
            );
        }
    }
}
