use std::fmt::Debug;
use std::ops::Deref;
use std::sync::atomic::AtomicI64;
use std::sync::Arc;

use rusqlite::{named_params, OptionalExtension};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tracing::{event, instrument, Level};
use uuid::Uuid;

use crate::job_status::RunInfo;
use crate::shared_state::SharedState;
use crate::{Error, Result};

#[derive(Debug, Clone)]
pub struct Job(pub Arc<JobData>);

impl Deref for Job {
    type Target = JobData;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub struct JobData {
    pub id: Uuid,
    pub(crate) job_id: i64,
    pub worker_id: u64,
    pub heartbeat_increment: i32,
    pub job_type: String,
    pub priority: i32,
    pub payload: Vec<u8>,
    pub expires: AtomicI64,

    pub start_time: OffsetDateTime,

    pub backoff_multiplier: f64,
    pub backoff_randomization: f64,
    pub backoff_initial_interval: i32,
    pub current_try: i32,
    pub max_retries: i32,

    pub(crate) done: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    pub(crate) queue: SharedState,
}

impl Debug for JobData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("id", &self.id)
            .field("job_id", &self.job_id)
            .field("worker_id", &self.worker_id)
            .field("heartbeat_increment", &self.heartbeat_increment)
            .field("job_type", &self.job_type)
            .field("priority", &self.priority)
            .field("payload", &self.payload)
            .field("expires", &self.expires)
            .field("start_time", &self.start_time)
            .field("backoff_multiplier", &self.backoff_multiplier)
            .field("backoff_randomization", &self.backoff_randomization)
            .field("backoff_initial_interval", &self.backoff_initial_interval)
            .field("current_try", &self.current_try)
            .field("max_retries", &self.max_retries)
            .finish_non_exhaustive()
    }
}

impl JobData {
    /// Checkpoint the task, replacing the payload with the passed in value.
    pub async fn checkpoint_blob(&self, new_payload: Vec<u8>) -> Result<OffsetDateTime> {
        // This counts as a heartbeat, so update the expiration.
        // Update the checkpoint_payload.
        let job_id = self.job_id;
        let worker_id = self.worker_id;
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let new_expire_time = now + (self.heartbeat_increment as i64);

        let actual_new_expire_time = self
            .queue
            .write_db(move |db| {
                let mut stmt = db.prepare_cached(
                    r##"UPDATE active_jobs
                SET checkpointed_payload=$payload,
                    last_heartbeat=$now,
                    expires_at = MAX(expires_at, $new_expire_time)
                WHERE job_id=$job_id AND active_worker_id=$worker_id
                RETURNING expires_at"##,
                )?;

                let actual_new_expire_time: Option<i64> = stmt
                    .query_row(
                        named_params! {
                            "$payload": new_payload,
                            "$new_expire_time": new_expire_time,
                            "$now": now,
                            "$job_id": job_id,
                            "$worker_id": worker_id,
                        },
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()?;

                Ok(actual_new_expire_time)
            })
            .await?;

        let new_time = actual_new_expire_time.ok_or(Error::Expired).and_then(|t| {
            OffsetDateTime::from_unix_timestamp(t)
                .map_err(|_| Error::TimestampOutOfRange("new expiration time"))
        })?;

        self.update_expiration(new_time);

        Ok(new_time)
    }

    /// Checkpoint the task, replacing the payload with the passed in value.
    pub async fn checkpoint_json<T: Serialize>(
        &mut self,
        new_payload: &T,
    ) -> Result<OffsetDateTime> {
        let blob = serde_json::to_vec(new_payload).map_err(Error::PayloadError)?;
        self.checkpoint_blob(blob).await
    }

    /// Tell the queue that the task is still running.
    pub async fn heartbeat(&mut self) -> Result<OffsetDateTime> {
        let new_time = send_heartbeat(
            self.job_id,
            self.worker_id,
            self.heartbeat_increment,
            &self.queue,
        )
        .await?;

        self.update_expiration(new_time);

        Ok(new_time)
    }

    fn update_expiration(&self, new_expiration: OffsetDateTime) {
        self.expires.store(
            new_expiration.unix_timestamp(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    pub(crate) async fn is_done(&self) -> bool {
        let done = self.done.lock().await;
        done.is_none()
    }

    /// Return if the task is past the expiration time or not.
    pub fn is_expired(&self) -> bool {
        self.expires.load(std::sync::atomic::Ordering::Relaxed)
            <= OffsetDateTime::now_utc().unix_timestamp()
    }

    pub fn json_payload<'a, T: Deserialize<'a>>(&'a self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(self.payload.as_slice())
    }

    async fn mark_job_done<T: Serialize + Send + Debug>(
        &self,
        info: T,
        success: bool,
    ) -> Result<(), Error> {
        let mut done = self.done.lock().await;
        let _chan = done.take().expect("Called complete after job finished");
        drop(done);

        let info = RunInfo {
            success,
            start: self.start_time,
            end: OffsetDateTime::now_utc(),
            info,
        };

        let this_run_info = serde_json::to_string(&info).map_err(Error::InvalidJobRunInfo)?;

        let job_id = self.job_id;
        let worker_id = self.worker_id;
        self.queue
            .write_db(move |db| {
                let tx = db.transaction()?;
                {
                    // Move job from active_jobs to done_jobs, and add the run info
                    let mut stmt = tx.prepare_cached(r##"INSERT INTO done_jobs
                      (job_id, external_id, job_type, priority, status, from_recurring_job, orig_run_at, payload,
                       max_retries, backoff_multiplier, backoff_randomization, backoff_initial_interval, added_at, finished_at, default_timeout,
                       heartbeat_increment, run_info)
                       SELECT job_id, external_id, job_type, priority, $status, from_recurring_job, orig_run_at, payload,
                              max_retries, backoff_multiplier, backoff_randomization, backoff_initial_interval, added_at, $now, default_timeout,
                              heartbeat_increment,
                              json_array_append(run_info, $this_run_info) AS run_info
                        FROM active_jobs
                        WHERE job_id=$job_id AND active_worker_id=$worker_id"##)?;

                    let altered = stmt.execute(named_params! {
                            "$job_id": job_id,
                            "$worker_id": worker_id,
                            "$now": OffsetDateTime::now_utc().unix_timestamp(),
                            "$this_run_info": this_run_info,
                            "$status": if success { "success" } else { "failed" },
                        })?;

                    if altered == 0 {
                        // The job expired before we could record the success. Return a special error
                        // for this so that we can log it. There isn't much to do about this though
                        // since the job may already be running again. Generally, though, this
                        // shouldn't happen if the heartbeat mechanism is used.
                        return Err(Error::ExpiredWhileRecordingSuccess);
                    }

                    // Clean up the old entry.
                    let mut stmt = tx.prepare_cached(r##"DELETE FROM active_jobs WHERE job_id=$1"##)?;
                    stmt.execute(&[&job_id])?;
                }

                tx.commit()?;
                Ok(())
            })
            .await?;

        Ok(())
    }

    /// Mark the job as successful.
    #[instrument]
    pub async fn complete<T: Serialize + Send + Debug>(&self, info: T) -> Result<(), Error> {
        self.mark_job_done(info, true).await
    }

    /// Mark the job as failed.
    #[instrument]
    pub async fn fail<T: Serialize + Send + Debug>(&self, info: T) -> Result<(), Error> {
        let mut done = self.done.lock().await;
        let _chan = done.take().expect("Called fail after job finished");
        drop(done);
        // Remove task from running jobs, update job info, calculate new retry time, and stick the
        // job back into pending.
        // If there is a checkpointed payload, use that. Otherwise use the original payload from the
        // job.

        if self.current_try + 1 > self.max_retries {
            return self.mark_job_done(info, false).await;
        }

        // Calculate the next run time, given the backoff.
        let now = OffsetDateTime::now_utc();
        let next_try_count = self.current_try + 1;
        let run_delta = (self.backoff_initial_interval as f64)
            * (self.backoff_multiplier).powi(next_try_count)
            * (1.0 + rand::random::<f64>() * self.backoff_randomization);
        let next_run_time = now.unix_timestamp() + (run_delta as i64);
        let job_id = self.job_id;
        let worker_id = self.worker_id;

        let info = RunInfo {
            success: false,
            start: self.start_time,
            end: now,
            info,
        };

        let this_run_info = serde_json::to_string(&info).map_err(Error::InvalidJobRunInfo)?;

        self.queue
            .write_db(move |db| {
                let tx = db.transaction()?;

                {
                    let mut stmt = tx.prepare_cached(
                        r##"UPDATE active_jobs SET
                    active_worker_id=null,
                    run_at=$next_run_time,
                    current_try = current_try + 1,
                    run_info = json_array_append(COALESCE(run_info, '[]'), $this_run_info)
                    WHERE job_id=$job_id AND active_worker_id=$worker_id"##,
                    )?;

                    let altered = stmt.execute(named_params! {
                        "$job_id": job_id,
                        "$worker_id": worker_id,
                        "$this_run_info": this_run_info,
                        "$next_run_time": next_run_time,
                    })?;

                    if altered == 0 {
                        return Err(Error::Expired);
                    }
                }

                tx.commit()?;
                Ok(())
            })
            .await?;

        Ok(())
    }
}

pub(crate) async fn send_heartbeat(
    job_id: i64,
    worker_id: u64,
    heartbeat_increment: i32,
    queue: &SharedState,
) -> Result<OffsetDateTime> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let new_expire_time = now + heartbeat_increment as i64;
    let actual_new_expire_time = queue
        .write_db(move |db| {
            let mut stmt = db.prepare_cached(
                r##"UPDATE active_jobs
                SET last_heartbeat=$now,
                    expires_at = MAX(expires_at, $new_expire_time)
                WHERE job_id=$job_id AND worker_id=$worker_id
                RETURNING expires_at"##,
            )?;

            let actual_new_expire_time: Option<i64> = stmt
                .query_row(
                    named_params! {
                        "$new_expire_time": new_expire_time,
                        "$now": now,
                        "$job_id": job_id,
                        "$worker_id": worker_id,
                    },
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;

            Ok(actual_new_expire_time)
        })
        .await?;

    let new_time = actual_new_expire_time.ok_or(Error::Expired).and_then(|t| {
        OffsetDateTime::from_unix_timestamp(t)
            .map_err(|_| Error::TimestampOutOfRange("new expiration time"))
    })?;

    Ok(new_time)
}
