use super::models::{JobStatus, NonEmptyQueueStatistics, QueueStatistics};
use crate::prover_api::fri_job_manager::{FriJob, JobState};
use crate::prover_api::metrics::{PROVER_METRICS, ProverStage, ProverType};
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify};
use zksync_os_l1_sender::batcher_model::{BatchMetadata, SignedBatchEnvelope};

#[derive(Debug)]
pub struct JobEntry<T> {
    pub batch_envelope: SignedBatchEnvelope<T>,
    pub status: JobStatus,
}

/// Concurrent map of prover jobs
/// Keys are batch numbers stored in a BTreeMap for ordered iteration.
/// Features:
///  * add_job - adds a new job
///     * blocks if adding this job would exceed max_assigned_batch_range until space is available
///     * O(log n)
///  * pick_job - picks the first job that is either pending or assigned and older than min_age
///     * currently, it iterates over all jobs and picks the first one that meets the criteria
///     * O(n)
///  * complete_job - marks a job as complete by removing it from the map
///     * O(log n)
///
/// Note that the current implementation uses async Mutex which is locked on each operation.
/// This may be problematic with a large number of in-progress jobs and a lot of polling -
/// but should be OK for hundreds of jobs/provers.
#[derive(Debug)]
pub struct ProverJobMap<T> {
    // == state ==
    jobs: Mutex<BTreeMap<u64, JobEntry<T>>>,
    // Notification for waiting when batch range limit is hit
    space_available: Notify,

    // == config ==
    // assigns to another prover if it takes longer than this
    assignment_timeout: Duration,
    // maximum allowed range between min and max batch numbers
    max_assigned_batch_range: usize,
    // FRI/SNARK - used in logging
    prover_stage: ProverStage,
}

impl<T: Clone> ProverJobMap<T> {
    pub fn new(
        assignment_timeout: Duration,
        max_assigned_batch_range: usize,
        prover_stage: ProverStage,
    ) -> Self {
        Self {
            jobs: Mutex::new(BTreeMap::new()),
            space_available: Notify::new(),
            assignment_timeout,
            max_assigned_batch_range,
            prover_stage,
        }
    }

    /// Adds a pending job to the map.
    /// Awaits if adding this job would exceed max_assigned_batch_range until space is available.
    pub async fn add_job(&self, batch_envelope: SignedBatchEnvelope<T>) {
        let batch_number = batch_envelope.batch_number();
        let mut jobs = self.jobs.lock().await;

        // Wait until there's space available (await if batch range limit would be exceeded)
        while self.is_queue_full(&jobs) {
            let queue_statistics = Self::queue_statistics(&jobs);

            tracing::info!(
                batch_number,
                ?queue_statistics,
                ?self.prover_stage,
                max_assigned_batch_range = self.max_assigned_batch_range,
                "Waiting for space in job map"
            );
            // Drop lock before awaiting notification
            drop(jobs);
            self.space_available.notified().await;
            // Re-acquire lock after notification
            jobs = self.jobs.lock().await;
        }

        let entry = JobEntry {
            batch_envelope,
            status: JobStatus::new_pending(),
        };

        jobs.insert(batch_number, entry);

        tracing::info!(
            batch_number,
            queue_statistics = ?Self::queue_statistics(&jobs),
            ?self.prover_stage,
            "Job added"
        );
    }

    // todo: pick job while
    /// Picks the first job (lowest batch number) that is either:
    /// - Pending and older than min_age (fake provers use non-empty min_age)
    /// - Assigned and timed out
    ///
    /// Returns None if no eligible job is found.
    pub async fn pick_job(
        &self,
        min_age: Duration,
        prover_id: &'static str,
    ) -> Option<(FriJob, T)> {
        let now = Instant::now();
        let mut jobs = self.jobs.lock().await;

        // Find an eligible job
        let eligible_batch_number = jobs.iter().find_map(|(batch_number, entry)| {
            match entry.status.assigned_at {
                // Pending: check if job meets minimum age requirement
                None if now.duration_since(entry.status.added_at) >= min_age => Some(*batch_number),
                // Assigned: check if job has timed out
                Some(assigned_at) if now.duration_since(assigned_at) > self.assignment_timeout => {
                    Some(*batch_number)
                }
                _ => None,
            }
        })?;

        let queue_statistics = Self::queue_statistics(&jobs);
        // Update status and extract data
        let entry = jobs.get_mut(&eligible_batch_number).unwrap();
        entry.status.assign(now);

        let proving_execution_version = entry
            .batch_envelope
            .batch
            .proving_version()
            .expect("Must be valid execution as set by the server");

        let fri_job = FriJob {
            batch_number: eligible_batch_number,
            vk_hash: proving_execution_version.vk_hash().to_string(),
        };

        tracing::info!(
            batch_number = eligible_batch_number,
            prover_id,
            vk_hash = fri_job.vk_hash,
            current_attempt = entry.status.current_attempt,
            time_since_added = ?now.duration_since(entry.status.added_at),
            ?queue_statistics,
            ?self.prover_stage,
            "Job assigned"
        );

        Some((fri_job, entry.batch_envelope.data.clone()))
    }

    /// Picks multiple consecutive jobs that satisfy the predicate.
    /// Only returns consecutive batch ranges with no gaps, and all jobs must have the same prover_version.
    ///
    /// The predicate receives (batch_number, &JobEntry<T>) and should return true for jobs that should be picked.
    ///
    /// Returns empty Vec if no eligible jobs are found.
    pub async fn pick_jobs_while<F>(
        &self,
        limit: usize,
        prover_id: &'static str,
        mut predicate: F,
    ) -> Vec<(FriJob, T)>
    where
        F: FnMut(&JobEntry<T>) -> bool,
    {
        let now = Instant::now();
        let mut jobs = self.jobs.lock().await;

        let mut result = Vec::new();
        let mut expected_next_batch: Option<u64> = None;
        let mut prover_version = None;

        for (batch_number, entry) in jobs.iter() {
            if result.len() >= limit {
                break;
            }
            match entry.status.assigned_at {
                // is already assigned and not timed out
                Some(assigned_at) if now.duration_since(assigned_at) < self.assignment_timeout => {
                    break;
                }
                // not assigned or timed out
                _ => {}
            };

            // Check if predicate allows this job
            if !predicate(entry) {
                break;
            }

            // Check if this is a consecutive batch
            if let Some(expected) = expected_next_batch
                && *batch_number != expected
            {
                // Gap detected, stop the sequence
                break;
            }

            if let Some(version) = prover_version {
                if version
                    != entry
                        .batch_envelope
                        .batch
                        .proving_version()
                        .expect("Must be valid execution as set by the server")
                {
                    // Prover version changed, stop the sequence
                    break;
                }
            } else {
                prover_version = Some(
                    entry
                        .batch_envelope
                        .batch
                        .proving_version()
                        .expect("Must be valid execution as set by the server"),
                );
            }

            let fri_job = FriJob {
                batch_number: *batch_number,
                vk_hash: prover_version.unwrap().vk_hash().to_string(),
            };

            result.push((fri_job, entry.batch_envelope.data.clone()));

            // Update expected next batch
            expected_next_batch = Some(batch_number + 1);
        }

        // Assign all selected jobs
        let queue_statistics = Self::queue_statistics(&jobs);
        for (number, (fri_job, _)) in result.iter().enumerate() {
            let entry = jobs.get_mut(&fri_job.batch_number).unwrap();
            entry.status.assign(now);

            tracing::info!(
                batch_number = fri_job.batch_number,
                prover_id,
                vk_hash = fri_job.vk_hash,
                current_attempt = entry.status.current_attempt,
                time_since_added = ?now.duration_since(entry.status.added_at),
                ?queue_statistics,
                ?self.prover_stage,
                "Job assigned ({} / {})",
                number,
                result.len(),
            );
        }

        result
    }

    /// If a job is present for a given batch_number, returns the corresponding BatchMetadata
    pub async fn get_job(&self, batch_number: u64) -> Option<BatchMetadata> {
        let jobs = self.jobs.lock().await;
        jobs.get(&batch_number)
            .map(|entry| entry.batch_envelope.batch.clone())
    }

    /// If a job is present for given batch_number, returns (vk, data)
    pub async fn get_batch_data(&self, batch_number: u64) -> Option<(&'static str, T)> {
        let jobs = self.jobs.lock().await;
        jobs.get(&batch_number).map(|entry| {
            (
                entry
                    .batch_envelope
                    .batch
                    .verification_key_hash()
                    .expect("VK hash must exist"),
                entry.batch_envelope.data.clone(),
            )
        })
    }

    /// Marks a job as complete by removing it from the map.
    /// Notifies inbound jobs waiting in add_job() that space may be available.
    /// Records metrics and logs timing info. Returns the batch envelope if the job existed.
    pub async fn complete_job(
        &self,
        batch_number: u64,
        prover_type: ProverType,
        prover_id: &'static str,
    ) -> Option<SignedBatchEnvelope<T>> {
        let mut jobs = self.jobs.lock().await;
        let entry = jobs.remove(&batch_number)?;

        let completion_stats = entry.status.completion_statistics();
        let batch_envelope = entry.batch_envelope;

        drop(jobs);
        self.space_available.notify_one();

        // Record Prometheus metrics
        PROVER_METRICS.prove_time[&(self.prover_stage, prover_type, prover_id)]
            .observe(completion_stats.prove_time);
        if batch_envelope.batch.tx_count > 0 {
            PROVER_METRICS.prove_time_per_tx[&(self.prover_stage, prover_type, prover_id)]
                .observe(completion_stats.prove_time / batch_envelope.batch.tx_count as u32);
        }
        PROVER_METRICS.proved_after_attempts[&(self.prover_stage, prover_type)]
            .observe(completion_stats.attempts_took as f64);

        tracing::info!(
            batch_number,
            ?completion_stats,
            batch_envelope.batch.tx_count,
            ?prover_type,
            prover_id,
            ?self.prover_stage,
            "Prover job completed and removed from map"
        );

        Some(batch_envelope)
    }

    pub async fn complete_many_jobs(
        &self,
        batch_number_from: u64,
        batch_number_to: u64,
        prover_type: ProverType,
        prover_id: &'static str,
    ) -> Option<Vec<SignedBatchEnvelope<T>>> {
        let mut jobs = self.jobs.lock().await;

        // First, verify all jobs exist -
        // it's possible a different job with an overlapping set of proofs was submitted.
        // There is no race condition possible within this method due to the mutex.
        for batch_number in batch_number_from..=batch_number_to {
            if !jobs.contains_key(&batch_number) {
                tracing::warn!(
                    batch_number_from,
                    batch_number_to,
                    missing_batch_number = batch_number,
                    prover_id,
                    ?prover_type,
                    "Cannot complete many jobs: job missing from map (race condition)"
                );
                return None;
            }
        }

        // All jobs exist - can mark as completed
        let mut completed = Vec::new();
        for batch_number in batch_number_from..=batch_number_to {
            let entry = jobs.remove(&batch_number).unwrap();
            let completion_stats = entry.status.completion_statistics();
            let batch_envelope = entry.batch_envelope;

            // Record Prometheus metrics
            PROVER_METRICS.prove_time[&(self.prover_stage, prover_type, prover_id)]
                .observe(completion_stats.prove_time);
            if batch_envelope.batch.tx_count > 0 {
                PROVER_METRICS.prove_time_per_tx[&(self.prover_stage, prover_type, prover_id)]
                    .observe(completion_stats.prove_time / batch_envelope.batch.tx_count as u32);
            }
            PROVER_METRICS.proved_after_attempts[&(self.prover_stage, prover_type)]
                .observe(completion_stats.attempts_took as f64);

            tracing::info!(
                batch_number,
                ?completion_stats,
                batch_envelope.batch.tx_count,
                ?prover_type,
                prover_id,
                stage = ?self.prover_stage,
                "Prover job completed and removed from map (batch {} within range {} - {})",
                batch_number,
                batch_number_from,
                batch_number_to,
            );

            completed.push(batch_envelope);
        }

        drop(jobs);
        // Notify once for all completed jobs
        self.space_available.notify_waiters();

        Some(completed)
    }

    /// Check if the queue is full (range between oldest and newest batch >= max_assigned_batch_range)
    fn is_queue_full(&self, jobs: &BTreeMap<u64, JobEntry<T>>) -> bool {
        if let (Some(&min), Some(&max)) = (jobs.keys().next(), jobs.keys().next_back()) {
            max - min >= self.max_assigned_batch_range as u64
        } else {
            false
        }
    }

    fn queue_statistics(jobs: &BTreeMap<u64, JobEntry<T>>) -> QueueStatistics {
        let min_batch = jobs.values().next();
        match min_batch {
            Some(min_batch) => QueueStatistics::NonEmpty(NonEmptyQueueStatistics {
                min_batch_added_at: min_batch.status.added_at,
                min_batch_current_attempt: min_batch.status.current_attempt,
                min_batch_number: min_batch.batch_envelope.batch_number(),
                max_batch_number: *jobs.keys().next_back().unwrap(),
                jobs_count: jobs.len(),
            }),
            None => QueueStatistics::Empty,
        }
    }

    pub async fn status(&self) -> Vec<JobState> {
        let jobs = self.jobs.lock().await;
        jobs.iter()
            .map(|(batch_number, entry)| JobState {
                fri_job: FriJob {
                    batch_number: *batch_number,
                    vk_hash: entry
                        .batch_envelope
                        .batch
                        .verification_key_hash()
                        .expect("VK hash must exist")
                        .to_string(),
                },
                assigned_seconds_ago: entry
                    .status
                    .assigned_at
                    .map(|assigned_at| assigned_at.elapsed().as_secs()),
                current_attempt: entry.status.current_attempt,
                added_seconds_ago: entry.status.added_at.elapsed().as_secs(),
            })
            .collect() // Already sorted by BTreeMap ordering
    }
}
