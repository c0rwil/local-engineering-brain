//! Bounded, prioritized scheduling for canonical mutations.
//!
//! Logical writers such as subagents submit work concurrently, while exactly
//! one scheduler thread enters canonical write paths. This provides explicit
//! backpressure and prevents background work from starving interactive work.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, mpsc};
use std::thread;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MutationPriority {
    Analytics = 1,
    BackgroundLearning = 2,
    EvidenceVerification = 3,
    ChangedFileRefresh = 4,
    Interactive = 5,
    CorrectnessCritical = 6,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SchedulerStats {
    pub queue_capacity: u64,
    pub queued: u64,
    pub submitted: u64,
    pub completed: u64,
    pub failed: u64,
    pub rejected: u64,
}

#[derive(Debug)]
struct SchedulerMetrics {
    queue_capacity: u64,
    queued: AtomicU64,
    submitted: AtomicU64,
    completed: AtomicU64,
    failed: AtomicU64,
    rejected: AtomicU64,
}

impl SchedulerMetrics {
    fn snapshot(&self) -> SchedulerStats {
        SchedulerStats {
            queue_capacity: self.queue_capacity,
            queued: self.queued.load(AtomicOrdering::Relaxed),
            submitted: self.submitted.load(AtomicOrdering::Relaxed),
            completed: self.completed.load(AtomicOrdering::Relaxed),
            failed: self.failed.load(AtomicOrdering::Relaxed),
            rejected: self.rejected.load(AtomicOrdering::Relaxed),
        }
    }
}

type Mutation = Box<dyn FnOnce() + Send + 'static>;

struct QueuedMutation {
    priority: MutationPriority,
    sequence: u64,
    operation: Mutation,
}

impl PartialEq for QueuedMutation {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.sequence == other.sequence
    }
}

impl Eq for QueuedMutation {}

impl PartialOrd for QueuedMutation {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for QueuedMutation {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .cmp(&other.priority)
            // `BinaryHeap` is max-first; older sequence wins within a priority.
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

#[derive(Clone)]
pub struct MutationScheduler {
    sender: mpsc::SyncSender<QueuedMutation>,
    sequence: Arc<AtomicU64>,
    metrics: Arc<SchedulerMetrics>,
}

#[must_use = "dropping the handle discards the mutation result"]
pub struct MutationHandle<T> {
    receiver: mpsc::Receiver<Result<T>>,
}

impl MutationScheduler {
    /// Start a single canonical writer with a bounded admission queue.
    ///
    /// # Panics
    ///
    /// Panics if `queue_capacity` is zero or the worker thread cannot start.
    #[must_use]
    pub fn start(queue_capacity: usize) -> Self {
        assert!(queue_capacity > 0, "writer queue must be positive");
        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let metrics = Arc::new(SchedulerMetrics {
            queue_capacity: u64::try_from(queue_capacity).unwrap_or(u64::MAX),
            queued: AtomicU64::new(0),
            submitted: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
        });
        let worker_metrics = Arc::clone(&metrics);
        thread::Builder::new()
            .name("engram-canonical-writer".to_owned())
            .spawn(move || writer_loop(&receiver, &worker_metrics))
            .expect("canonical writer thread must start");
        Self {
            sender,
            sequence: Arc::new(AtomicU64::new(0)),
            metrics,
        }
    }

    /// Submit one mutation without waiting for queue capacity.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Backpressure`] when the bounded queue is full or
    /// [`Error::SchedulerStopped`] if the writer has exited.
    pub fn submit<T>(
        &self,
        priority: MutationPriority,
        operation: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<MutationHandle<T>>
    where
        T: Send + 'static,
    {
        let sequence = self.sequence.fetch_add(1, AtomicOrdering::Relaxed);
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        let operation_metrics = Arc::clone(&self.metrics);
        let queued = QueuedMutation {
            priority,
            sequence,
            operation: Box::new(move || {
                let result = catch_unwind(AssertUnwindSafe(operation))
                    .unwrap_or(Err(Error::MutationPanicked));
                if result.is_err() {
                    operation_metrics
                        .failed
                        .fetch_add(1, AtomicOrdering::Relaxed);
                }
                operation_metrics
                    .completed
                    .fetch_add(1, AtomicOrdering::Relaxed);
                let _ = result_sender.send(result);
            }),
        };

        self.metrics.queued.fetch_add(1, AtomicOrdering::Relaxed);
        match self.sender.try_send(queued) {
            Ok(()) => {
                self.metrics.submitted.fetch_add(1, AtomicOrdering::Relaxed);
                Ok(MutationHandle {
                    receiver: result_receiver,
                })
            }
            Err(mpsc::TrySendError::Full(_)) => {
                self.metrics.queued.fetch_sub(1, AtomicOrdering::Relaxed);
                self.metrics.rejected.fetch_add(1, AtomicOrdering::Relaxed);
                Err(Error::Backpressure)
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.metrics.queued.fetch_sub(1, AtomicOrdering::Relaxed);
                Err(Error::SchedulerStopped)
            }
        }
    }

    #[must_use]
    pub fn stats(&self) -> SchedulerStats {
        self.metrics.snapshot()
    }
}

impl<T> MutationHandle<T> {
    /// Wait for the mutation to commit or fail.
    ///
    /// # Errors
    ///
    /// Returns the mutation's domain/storage error, or a scheduler error if the
    /// worker exits without delivering a result.
    pub fn wait(self) -> Result<T> {
        self.receiver
            .recv()
            .map_err(|_| Error::SchedulerResponseClosed)?
    }
}

fn writer_loop(receiver: &mpsc::Receiver<QueuedMutation>, metrics: &SchedulerMetrics) {
    let mut pending = BinaryHeap::new();
    while let Ok(first) = receiver.recv() {
        pending.push(first);
        while let Ok(next) = receiver.try_recv() {
            pending.push(next);
        }
        while let Some(mutation) = pending.pop() {
            metrics.queued.fetch_sub(1, AtomicOrdering::Relaxed);
            (mutation.operation)();
            while let Ok(next) = receiver.try_recv() {
                pending.push(next);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier, Mutex};

    use super::*;

    #[test]
    fn higher_priority_overtakes_queued_background_work() {
        let scheduler = MutationScheduler::start(8);
        let barrier = Arc::new(Barrier::new(2));
        let worker_barrier = Arc::clone(&barrier);
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let blocker = scheduler
            .submit(MutationPriority::Interactive, move || {
                started_sender.send(()).expect("started");
                worker_barrier.wait();
                Ok(())
            })
            .expect("blocker");
        started_receiver.recv().expect("blocker started");

        let order = Arc::new(Mutex::new(Vec::new()));
        let background_order = Arc::clone(&order);
        let background = scheduler
            .submit(MutationPriority::Analytics, move || {
                background_order.lock().expect("order").push("background");
                Ok(())
            })
            .expect("background");
        let critical_order = Arc::clone(&order);
        let critical = scheduler
            .submit(MutationPriority::CorrectnessCritical, move || {
                critical_order.lock().expect("order").push("critical");
                Ok(())
            })
            .expect("critical");

        barrier.wait();
        blocker.wait().expect("blocker result");
        critical.wait().expect("critical result");
        background.wait().expect("background result");
        assert_eq!(*order.lock().expect("order"), ["critical", "background"]);
        assert_eq!(scheduler.stats().queued, 0);
    }

    #[test]
    fn full_queue_applies_backpressure() {
        let scheduler = MutationScheduler::start(1);
        let barrier = Arc::new(Barrier::new(2));
        let worker_barrier = Arc::clone(&barrier);
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let blocker = scheduler
            .submit(MutationPriority::Interactive, move || {
                started_sender.send(()).expect("started");
                worker_barrier.wait();
                Ok(())
            })
            .expect("blocker");
        started_receiver.recv().expect("blocker started");
        let queued = scheduler
            .submit(MutationPriority::Analytics, || Ok(()))
            .expect("queued");
        let rejected = scheduler.submit(MutationPriority::Analytics, || Ok(()));
        assert!(matches!(rejected, Err(Error::Backpressure)));
        barrier.wait();
        blocker.wait().expect("blocker result");
        queued.wait().expect("queued result");
        assert_eq!(scheduler.stats().rejected, 1);
    }

    #[test]
    fn mutation_errors_are_counted_and_returned() {
        let scheduler = MutationScheduler::start(2);
        let handle: MutationHandle<()> = scheduler
            .submit(MutationPriority::Interactive, || {
                Err(Error::Config("expected".to_owned()))
            })
            .expect("submitted");
        assert!(handle.wait().is_err());
        let stats = scheduler.stats();
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.failed, 1);
    }

    #[test]
    fn mutation_panic_is_isolated_and_reported() {
        let scheduler = MutationScheduler::start(2);
        let panicked: MutationHandle<()> = scheduler
            .submit(MutationPriority::Interactive, || panic!("probe"))
            .expect("submitted panic");
        assert!(matches!(panicked.wait(), Err(Error::MutationPanicked)));
        let healthy = scheduler
            .submit(MutationPriority::Interactive, || Ok(42))
            .expect("scheduler remains alive");
        assert_eq!(healthy.wait().expect("healthy result"), 42);
    }
}
