use crate::config::Config;
use anyhow::Result;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use tracing::warn;

// One worker serializes Bridge START/STOP, including cleanup of superseded attempts.
pub struct HueReconnect<T: Send + 'static> {
    commands: mpsc::Sender<(u64, Option<Config>)>,
    results: mpsc::Receiver<(u64, Result<T>)>,
    generation: Arc<AtomicU64>,
    settled: Arc<AtomicU64>,
    enabled: bool,
    pending: bool,
    worker: Option<JoinHandle<()>>,
}

impl<T: Send + 'static> HueReconnect<T> {
    pub fn new(
        connect: impl Fn(&Config) -> Result<T> + Send + 'static,
        stop: impl Fn(&Config) -> Result<()> + Send + 'static,
    ) -> Result<Self> {
        let (commands, receiver) = mpsc::channel::<(u64, Option<Config>)>();
        let (sender, results) = mpsc::channel();
        let generation = Arc::new(AtomicU64::new(0));
        let current = generation.clone();
        let settled = Arc::new(AtomicU64::new(0));
        let completed = settled.clone();
        let worker = thread::Builder::new()
            .name("hue-reconnect".into())
            .spawn(move || {
                let mut active = None;
                while let Ok(mut command) = receiver.recv() {
                    for newer in receiver.try_iter() {
                        command = newer;
                    }
                    if let Some(config) = active.take() {
                        if let Err(error) = stop(&config) {
                            warn!("Failed to deactivate previous Hue session: {error:#}");
                        }
                    }
                    let (epoch, config) = command;
                    let Some(config) = config else {
                        completed.store(epoch, Ordering::SeqCst);
                        continue;
                    };
                    if current.load(Ordering::SeqCst) != epoch {
                        continue;
                    }
                    let result = connect(&config);
                    if result.is_err() || current.load(Ordering::SeqCst) != epoch {
                        if let Err(error) = stop(&config) {
                            warn!("Failed to deactivate unsuccessful Hue session: {error:#}");
                        }
                        if current.load(Ordering::SeqCst) == epoch {
                            let _ = sender.send((epoch, result));
                        }
                    } else {
                        active = Some(config);
                        if sender.send((epoch, result)).is_err() {
                            break;
                        }
                    }
                    completed.store(epoch, Ordering::SeqCst);
                }
                if let Some(config) = active {
                    if let Err(error) = stop(&config) {
                        warn!("Failed to deactivate Hue session at shutdown: {error:#}");
                    }
                }
            })?;
        Ok(Self {
            commands,
            results,
            generation,
            settled,
            enabled: false,
            pending: false,
            worker: Some(worker),
        })
    }

    pub fn request(&mut self, config: Option<&Config>) {
        if config.is_none() && !self.enabled {
            return;
        }
        self.enabled = config.is_some();
        let epoch = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.pending = config.is_some();
        if self.commands.send((epoch, config.cloned())).is_err() {
            self.pending = false;
        }
    }

    pub fn pending(&self) -> bool {
        self.pending
    }

    pub fn is_settled(&self) -> bool {
        self.settled.load(Ordering::SeqCst) == self.generation.load(Ordering::SeqCst)
    }

    pub fn poll(&mut self) -> Option<Result<T>> {
        while let Ok((epoch, result)) = self.results.try_recv() {
            if self.generation.load(Ordering::SeqCst) == epoch {
                self.pending = false;
                return Some(result);
            }
        }
        None
    }
}

impl<T: Send + 'static> Drop for HueReconnect<T> {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        // Close the queue before joining; network operations have finite deadlines.
        let (replacement, _) = mpsc::channel();
        drop(std::mem::replace(&mut self.commands, replacement));
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                warn!("Hue reconnect worker panicked");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn config(area: &str) -> Config {
        Config::new_default("127.0.0.1", "test", "test", area)
    }

    fn result(worker: &mut HueReconnect<String>) -> Result<String> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(result) = worker.poll() {
                return result;
            }
            assert!(Instant::now() < deadline, "worker did not complete");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn stalled_connect_keeps_control_progress_and_rejects_success_after_stop() {
        let (started, started_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (stopped, stopped_rx) = mpsc::channel();
        let mut worker = HueReconnect::new(
            move |_| {
                started.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                Ok("stale".to_string())
            },
            move |_| {
                stopped.send(()).unwrap();
                Ok(())
            },
        )
        .unwrap();
        worker.request(Some(&config("old")));
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        // Polling and control invalidation do not wait for the blocked network operation.
        for _ in 0..100 {
            assert!(worker.poll().is_none());
        }
        worker.request(None);
        assert!(!worker.pending());
        release.send(()).unwrap();
        stopped_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(worker.poll().is_none());
    }

    #[test]
    fn reconfigure_serializes_old_cleanup_before_new_start() {
        let (started, started_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (events, events_rx) = mpsc::channel();
        let connect_events = events.clone();
        let mut worker = HueReconnect::new(
            move |config| {
                connect_events
                    .send(format!("start {}", config.entertainment_area_id))
                    .unwrap();
                if config.entertainment_area_id == "old" {
                    started.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                Ok(config.entertainment_area_id.clone())
            },
            move |config| {
                events
                    .send(format!("stop {}", config.entertainment_area_id))
                    .unwrap();
                Ok(())
            },
        )
        .unwrap();
        worker.request(Some(&config("old")));
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.request(Some(&config("superseded")));
        worker.request(Some(&config("new")));
        release.send(()).unwrap();
        assert_eq!(result(&mut worker).unwrap(), "new");
        assert_eq!(
            events_rx.try_iter().collect::<Vec<_>>(),
            ["start old", "stop old", "start new"]
        );
        drop(worker);
        assert_eq!(
            events_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "stop new"
        );
    }

    #[test]
    fn failed_initial_connection_cleans_up_and_can_retry() {
        let attempts = Arc::new(AtomicU64::new(0));
        let calls = attempts.clone();
        let (stopped, stopped_rx) = mpsc::channel();
        let mut worker = HueReconnect::new(
            move |_| {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    anyhow::bail!("unavailable");
                }
                Ok("connected".to_string())
            },
            move |_| {
                stopped.send(()).unwrap();
                Ok(())
            },
        )
        .unwrap();
        worker.request(Some(&config("area")));
        assert_eq!(result(&mut worker).unwrap_err().to_string(), "unavailable");
        assert!(!worker.pending());
        stopped_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.request(Some(&config("area")));
        assert_eq!(result(&mut worker).unwrap(), "connected");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }
}
