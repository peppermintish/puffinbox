//! Bounded scheduled IPTV capture and crash recovery.
//!
//! Recordings are written below an identity-checked library root to a hidden
//! partial file. A file becomes catalog-visible only after capture stops at
//! the scheduled deadline, MPEG-TS framing and checksum validation succeed,
//! and the partial file is atomically linked to its final name.

use std::{
    collections::HashMap,
    ffi::CString,
    fs::{File, OpenOptions},
    future::Future,
    io::{Read, Seek, SeekFrom},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, Instant},
};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use futures_util::FutureExt;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Row, Transaction, types::Json};
use tokio::{
    fs::File as TokioFile,
    io::AsyncWriteExt,
    sync::{Mutex, oneshot, watch},
    task::{Id as TaskId, JoinHandle, JoinSet},
    time::{sleep, timeout},
};
use tracing::warn;
use uuid::Uuid;

use crate::{ApiError, auth::UserRecord, db, state::AppState};

use super::livetv::{
    FeedError, MAX_LIVE_INPUT_BYTES, MAX_LIVE_INPUT_SECONDS, OriginPin, open_live_input,
};

const MAX_RECORDERS: usize = 2;
const SCHEDULER_INTERVAL: Duration = Duration::from_millis(500);
const SOURCE_OPEN_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RECOVERY_ROWS: i64 = 256;
const MAX_CAPTURE_LATENESS: ChronoDuration = ChronoDuration::seconds(120);
const DUE_TIMER_CANDIDATE_BATCH: i64 = 64;
const FAILURE_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(250);
const FAILURE_RETRY_MAX_DELAY: Duration = Duration::from_secs(5);
const TS_PACKET_BYTES: usize = 188;
const TS_READ_PACKETS: usize = 512;

struct RecorderManager {
    active: Mutex<HashMap<Uuid, watch::Sender<bool>>>,
    supervisor: Mutex<Option<JoinHandle<()>>>,
}

fn manager() -> &'static RecorderManager {
    static MANAGER: OnceLock<RecorderManager> = OnceLock::new();
    MANAGER.get_or_init(|| RecorderManager {
        active: Mutex::new(HashMap::new()),
        supervisor: Mutex::new(None),
    })
}

#[derive(Clone)]
struct RecordingClaim {
    id: Uuid,
    timer_id: Uuid,
    owner_id: Uuid,
    channel_id: Uuid,
    channel_library_id: Uuid,
    output_library_id: Uuid,
    channel_name: String,
    title: String,
    source_url: String,
    pins: Vec<OriginPin>,
    capture_end: DateTime<Utc>,
    rating_system: Option<String>,
    content_rating: Option<String>,
    policy_rating_scale: Option<String>,
    policy_rating_value: Option<i16>,
}

struct OutputFile {
    root: File,
    file: TokioFile,
    partial_name: String,
    final_name: String,
    final_path: PathBuf,
}

struct PreparedRecording {
    root: File,
    partial_name: String,
    final_name: String,
    final_path: PathBuf,
    size: i64,
    sha256: String,
    modified: DateTime<Utc>,
}

enum RecordingJobResult {
    Finished(Uuid),
    Failed(Box<RecordingClaim>, ApiError),
}

pub(super) async fn start(state: AppState) {
    let mut supervisor = manager().supervisor.lock().await;
    if supervisor.as_ref().is_some_and(|task| !task.is_finished()) {
        return;
    }
    *supervisor = Some(tokio::spawn(run_scheduler(state)));
}

pub(super) async fn shutdown() -> bool {
    let active = manager().active.lock().await;
    for cancel in active.values() {
        cancel.send_replace(true);
    }
    drop(active);
    let Some(supervisor) = manager().supervisor.lock().await.take() else {
        return true;
    };
    supervisor.await.is_ok()
}

pub(super) async fn cancel(timer_id: Uuid) -> bool {
    manager()
        .active
        .lock()
        .await
        .get(&timer_id)
        .is_some_and(|cancel| {
            cancel.send_replace(true);
            true
        })
}

pub(super) async fn recover(state: &AppState) -> Result<usize, ApiError> {
    let rows = sqlx::query(
        "SELECT r.timer_id,r.library_id,r.relative_path,r.status,r.byte_count,r.sha256, \
         r.channel_item_id,r.channel_name,r.title,r.rating_system,r.content_rating, \
         r.policy_rating_scale,r.policy_rating_value,t.owner_user_id,t.output_library_id, \
         t.start_at,t.end_at,t.padding_after_seconds,t.padding_before_seconds,t.started_at, \
         c.library_id AS channel_library_id,c.stream_url,s.origin_pins \
         FROM live_tv_recordings r JOIN live_tv_timers t ON t.id=r.timer_id \
         JOIN live_tv_channels c ON c.item_id=r.channel_item_id \
         JOIN live_tv_sources s ON s.id=c.source_id \
         WHERE r.status IN ('recording','publishing') AND r.claimed_run_id IS DISTINCT FROM $1 \
         ORDER BY r.started_at,r.id LIMIT $2",
    )
    .bind(state.run_id)
    .bind(MAX_RECOVERY_ROWS)
    .fetch_all(&state.db)
    .await?;
    let mut recovered = 0;
    let row_count = rows.len();
    for row in rows {
        let claim = claim_from_row(&row)?;
        let status: String = row.try_get("status")?;
        let relative_path: String = row.try_get("relative_path")?;
        let expected_final = final_name(claim.id);
        let expected_partial = partial_name(claim.id);
        if relative_path != expected_final {
            return Err(ApiError::Unavailable);
        }
        if status == "publishing" {
            let byte_count: i64 = row.try_get("byte_count")?;
            let checksum: Option<String> = row.try_get("sha256")?;
            if let Some(checksum) = checksum
                && byte_count > 0
                && let Ok(prepared) = inspect_existing(
                    state,
                    claim.output_library_id,
                    claim.id,
                    &expected_partial,
                    &expected_final,
                    byte_count,
                    &checksum,
                )
                .await
            {
                let size = prepared.size;
                let hash = prepared.sha256.clone();
                let path = prepared.final_path.clone();
                let modified = prepared.modified;
                let mut owner_policy_tx = state.db.begin().await?;
                if let Err(error) =
                    lock_recording_policy_user(&mut owner_policy_tx, state, &claim).await
                {
                    drop(owner_policy_tx);
                    match error {
                        ApiError::Forbidden | ApiError::NotFound => {
                            cleanup_files(state, claim.output_library_id, claim.id).await?;
                            mark_recovered_interrupted(state, &claim).await?;
                            recovered += 1;
                            continue;
                        }
                        other => return Err(other),
                    }
                }
                if !claim_publishing_for_recovery(state, &claim).await? {
                    drop(owner_policy_tx);
                    cleanup_files(state, claim.output_library_id, claim.id).await?;
                    mark_recovered_interrupted(state, &claim).await?;
                    recovered += 1;
                    continue;
                }
                if !lock_publishing_rows(&mut owner_policy_tx, state.run_id, &claim).await? {
                    drop(owner_policy_tx);
                    cleanup_files(state, claim.output_library_id, claim.id).await?;
                    mark_recovered_interrupted(state, &claim).await?;
                    recovered += 1;
                    continue;
                }
                if !prepared.partial_name.is_empty() {
                    publish_file(prepared).await?;
                } else {
                    cleanup_partial(state, claim.output_library_id, claim.id).await?;
                }
                owner_policy_tx.commit().await?;
                if let Err(error) =
                    complete_recording(state, &claim, &path, size, &hash, Some(modified)).await
                {
                    warn!(recording_id = %claim.id, error = ?error, "valid Live TV publication remains pending recovery");
                }
                recovered += 1;
                continue;
            }
        }
        cleanup_files(state, claim.output_library_id, claim.id).await?;
        mark_recovered_interrupted(state, &claim).await?;
        recovered += 1;
    }
    if row_count as i64 == MAX_RECOVERY_ROWS {
        return Err(ApiError::Unavailable);
    }
    Ok(recovered)
}

async fn run_scheduler(state: AppState) {
    let mut jobs = JoinSet::new();
    let mut job_claims = HashMap::<TaskId, RecordingClaim>::new();
    let mut finalizers = JoinSet::new();
    let mut finalizer_claims = HashMap::<TaskId, RecordingClaim>::new();
    loop {
        if state
            .shutdown_requested
            .load(std::sync::atomic::Ordering::Acquire)
        {
            let active = manager().active.lock().await;
            for cancel in active.values() {
                cancel.send_replace(true);
            }
            drop(active);
            while let Some(result) = jobs.join_next_with_id().await {
                handle_recording_job(
                    &state,
                    result,
                    &mut job_claims,
                    &mut finalizers,
                    &mut finalizer_claims,
                )
                .await;
            }
            while let Some(result) = finalizers.join_next_with_id().await {
                handle_finalizer_result(&state, result, &mut finalizers, &mut finalizer_claims)
                    .await;
            }
            manager().active.lock().await.clear();
            return;
        }
        while jobs.len() < MAX_RECORDERS {
            match claim_due_timer(&state).await {
                Ok(Some(claim)) => {
                    let (cancel_tx, cancel_rx) = watch::channel(false);
                    manager()
                        .active
                        .lock()
                        .await
                        .insert(claim.timer_id, cancel_tx);
                    let worker_state = state.clone();
                    let worker_claim = claim.clone();
                    let task = jobs.spawn(async move {
                        let recording =
                            run_recording(worker_state, worker_claim.clone(), cancel_rx);
                        recording_job_result(worker_claim, recording).await
                    });
                    job_claims.insert(task.id(), claim);
                }
                Ok(None) => break,
                Err(error) => {
                    warn!(error = ?error, "could not claim a due Live TV recording");
                    break;
                }
            }
        }
        tokio::select! {
            _ = sleep(SCHEDULER_INTERVAL) => {},
            result = jobs.join_next_with_id(), if !jobs.is_empty() => {
                if let Some(result) = result {
                    handle_recording_job(
                        &state,
                        result,
                        &mut job_claims,
                        &mut finalizers,
                        &mut finalizer_claims,
                    ).await;
                }
            },
            result = finalizers.join_next_with_id(), if !finalizers.is_empty() => {
                if let Some(result) = result {
                    handle_finalizer_result(
                        &state,
                        result,
                        &mut finalizers,
                        &mut finalizer_claims,
                    ).await;
                }
            },
        }
    }
}

async fn handle_recording_job(
    state: &AppState,
    result: Result<(TaskId, RecordingJobResult), tokio::task::JoinError>,
    job_claims: &mut HashMap<TaskId, RecordingClaim>,
    finalizers: &mut JoinSet<Uuid>,
    finalizer_claims: &mut HashMap<TaskId, RecordingClaim>,
) {
    match result {
        Ok((task_id, RecordingJobResult::Finished(timer_id))) => {
            job_claims.remove(&task_id);
            manager().active.lock().await.remove(&timer_id);
        }
        Ok((task_id, RecordingJobResult::Failed(claim, error))) => {
            job_claims.remove(&task_id);
            spawn_failure_finalizer(state, *claim, error, finalizers, finalizer_claims);
        }
        Err(error) => match take_joined_task_claim(&error, job_claims) {
            Some(claim) => {
                warn!(
                    timer_id = %claim.timer_id,
                    error = %error,
                    "Live TV recorder worker panicked or was cancelled; retrying finalization"
                );
                spawn_failure_finalizer(
                    state,
                    claim,
                    ApiError::Unavailable,
                    finalizers,
                    finalizer_claims,
                );
            }
            None => {
                warn!(error = %error, "Live TV recorder worker terminated without a saved claim")
            }
        },
    }
}

async fn handle_finalizer_result(
    state: &AppState,
    result: Result<(TaskId, Uuid), tokio::task::JoinError>,
    finalizers: &mut JoinSet<Uuid>,
    finalizer_claims: &mut HashMap<TaskId, RecordingClaim>,
) {
    match result {
        Ok((task_id, timer_id)) => {
            finalizer_claims.remove(&task_id);
            manager().active.lock().await.remove(&timer_id);
        }
        Err(error) => match take_joined_task_claim(&error, finalizer_claims) {
            Some(claim) => {
                warn!(
                    timer_id = %claim.timer_id,
                    error = %error,
                    "Live TV failure finalizer panicked or was cancelled"
                );
                if sleep_backoff_or_shutdown(state, FAILURE_RETRY_INITIAL_DELAY).await {
                    spawn_failure_finalizer(
                        state,
                        claim,
                        ApiError::Unavailable,
                        finalizers,
                        finalizer_claims,
                    );
                } else {
                    warn!(
                        timer_id = %claim.timer_id,
                        "Live TV failure finalization stopped during shutdown; the next run will recover it"
                    );
                }
            }
            None => {
                warn!(error = %error, "Live TV failure finalizer terminated without a saved claim")
            }
        },
    }
}

fn take_joined_task_claim(
    error: &tokio::task::JoinError,
    claims: &mut HashMap<TaskId, RecordingClaim>,
) -> Option<RecordingClaim> {
    claims.remove(&error.id())
}

fn spawn_failure_finalizer(
    state: &AppState,
    claim: RecordingClaim,
    error: ApiError,
    finalizers: &mut JoinSet<Uuid>,
    finalizer_claims: &mut HashMap<TaskId, RecordingClaim>,
) {
    let retry_state = state.clone();
    let retained_claim = claim.clone();
    let task = finalizers.spawn(async move {
        retry_recording_failure(retry_state, claim, error).await;
        retained_claim.timer_id
    });
    finalizer_claims.insert(task.id(), retained_claim);
}

async fn recording_job_result<F>(claim: RecordingClaim, recording: F) -> RecordingJobResult
where
    F: Future<Output = Result<(), ApiError>>,
{
    match AssertUnwindSafe(recording).catch_unwind().await {
        Ok(Ok(())) => RecordingJobResult::Finished(claim.timer_id),
        Ok(Err(error)) => RecordingJobResult::Failed(Box::new(claim), error),
        Err(_) => {
            warn!(
                timer_id = %claim.timer_id,
                "Live TV recorder worker panicked; retrying finalization"
            );
            RecordingJobResult::Failed(Box::new(claim), ApiError::Unavailable)
        }
    }
}

async fn retry_recording_failure(state: AppState, claim: RecordingClaim, error: ApiError) {
    let code = classify_failure(&state, &claim, error).await;
    let mut delay = FAILURE_RETRY_INITIAL_DELAY;
    let mut first_attempt = true;
    loop {
        if !first_attempt
            && state
                .shutdown_requested
                .load(std::sync::atomic::Ordering::Acquire)
        {
            warn!(
                timer_id = %claim.timer_id,
                "Live TV failure finalization stopped during shutdown; the next run will recover it"
            );
            return;
        }
        first_attempt = false;

        match is_publishing(&state, &claim).await {
            Ok(true) => match recover_publishing_claim(&state, &claim).await {
                Ok(()) => return,
                Err(ApiError::Forbidden | ApiError::NotFound) => {
                    match fail_recording(&state, &claim, "source-unavailable").await {
                        Ok(()) => return,
                        Err(error) => warn!(
                            timer_id = %claim.timer_id,
                            error = ?error,
                            "could not persist Live TV publication failure; retrying"
                        ),
                    }
                }
                Err(error) => warn!(
                    timer_id = %claim.timer_id,
                    error = ?error,
                    "could not recover Live TV publication; retrying"
                ),
            },
            Ok(false) => match fail_recording(&state, &claim, code).await {
                Ok(()) => return,
                Err(error) => warn!(
                    timer_id = %claim.timer_id,
                    error = ?error,
                    "could not persist Live TV recording failure; retrying"
                ),
            },
            Err(error) => warn!(
                timer_id = %claim.timer_id,
                error = ?error,
                "could not verify Live TV state before failure finalization; retrying"
            ),
        }

        if state
            .shutdown_requested
            .load(std::sync::atomic::Ordering::Acquire)
        {
            warn!(
                timer_id = %claim.timer_id,
                "Live TV failure finalization stopped during shutdown; the next run will recover it"
            );
            return;
        }
        if !sleep_backoff_or_shutdown(&state, delay).await {
            warn!(
                timer_id = %claim.timer_id,
                "Live TV failure finalization stopped during shutdown; the next run will recover it"
            );
            return;
        }
        delay = delay.saturating_mul(2).min(FAILURE_RETRY_MAX_DELAY);
    }
}

async fn sleep_backoff_or_shutdown(state: &AppState, delay: Duration) -> bool {
    let mut remaining = delay;
    while !remaining.is_zero() {
        if state
            .shutdown_requested
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return false;
        }
        let step = Duration::from_millis(100).min(remaining);
        sleep(step).await;
        remaining = remaining.saturating_sub(step);
    }
    !state
        .shutdown_requested
        .load(std::sync::atomic::Ordering::Acquire)
}

async fn is_publishing(state: &AppState, claim: &RecordingClaim) -> Result<bool, ApiError> {
    let status: Option<String> = sqlx::query_scalar(
        "SELECT status FROM live_tv_recordings WHERE id=$1 AND claimed_run_id=$2",
    )
    .bind(claim.id)
    .bind(state.run_id)
    .fetch_optional(&state.db)
    .await?;
    Ok(status.as_deref() == Some("publishing"))
}

async fn recover_publishing_claim(
    state: &AppState,
    claim: &RecordingClaim,
) -> Result<(), ApiError> {
    let row = sqlx::query(
        "SELECT status,claimed_run_id,relative_path,byte_count,sha256 FROM live_tv_recordings \
         WHERE id=$1 AND timer_id=$2",
    )
    .bind(claim.id)
    .bind(claim.timer_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else {
        return Ok(());
    };
    let status: String = row.try_get("status")?;
    let claimed_run: Option<Uuid> = row.try_get("claimed_run_id")?;
    if status != "publishing" || claimed_run != Some(state.run_id) {
        return Ok(());
    }
    let relative_path: String = row.try_get("relative_path")?;
    if relative_path != final_name(claim.id) {
        return Err(ApiError::Unavailable);
    }
    let size: i64 = row.try_get("byte_count")?;
    let checksum: Option<String> = row.try_get("sha256")?;
    let Some(checksum) = checksum.filter(|value| size > 0 && !value.is_empty()) else {
        return Err(ApiError::Unavailable);
    };
    let prepared = inspect_existing(
        state,
        claim.output_library_id,
        claim.id,
        &partial_name(claim.id),
        &final_name(claim.id),
        size,
        &checksum,
    )
    .await?;
    let final_path = prepared.final_path.clone();
    let modified = prepared.modified;
    let mut policy_tx = state.db.begin().await?;
    lock_recording_policy_user(&mut policy_tx, state, claim).await?;
    if !lock_publishing_rows(&mut policy_tx, state.run_id, claim).await? {
        return Err(ApiError::Conflict(
            "Publishing claim changed during recovery".to_owned(),
        ));
    }
    if prepared.partial_name.is_empty() {
        cleanup_partial(state, claim.output_library_id, claim.id).await?;
    } else {
        publish_file(prepared).await?;
    }
    policy_tx.commit().await?;
    complete_recording(state, claim, &final_path, size, &checksum, Some(modified)).await
}

async fn claim_due_timer(state: &AppState) -> Result<Option<RecordingClaim>, ApiError> {
    // Commit overdue timer cleanup separately so this transaction can keep a
    // single source -> channel -> timer lock order with source lifecycle work.
    let mut expiry_tx = state.db.begin().await?;
    db::require_active_run(&mut expiry_tx, state.run_id).await?;
    sqlx::query(
        "UPDATE live_tv_timers SET status='failed',finished_at=NOW(),last_error_code='source-unavailable',updated_at=NOW() \
         WHERE status='scheduled' AND (end_at + padding_after_seconds * INTERVAL '1 second' <= NOW() \
         OR start_at - padding_before_seconds * INTERVAL '1 second' < NOW() - ($1 * INTERVAL '1 second'))",
    )
    .bind(MAX_CAPTURE_LATENESS.num_seconds())
    .execute(&mut *expiry_tx)
    .await?;
    expiry_tx.commit().await?;

    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    // Re-fetch bounded ordered batches after lock conflicts, excluding the
    // source/channel/timer that could not be locked. This advances past large
    // runs of one locked source while allowing another timer on the same
    // source when only a channel or timer row was contended.
    let mut skipped_sources = Vec::new();
    let mut skipped_channels = Vec::new();
    let mut skipped_timers = Vec::new();
    sqlx::query("SAVEPOINT livetv_claim_candidate")
        .execute(&mut *tx)
        .await?;
    loop {
        let candidates = sqlx::query(
            "SELECT t.id AS timer_id,c.item_id AS channel_item_id,c.source_id \
             FROM live_tv_timers t JOIN live_tv_channels c ON c.item_id=t.channel_item_id \
             JOIN live_tv_sources s ON s.id=c.source_id \
             WHERE t.status='scheduled' AND t.start_at - t.padding_before_seconds * INTERVAL '1 second' <= NOW() \
             AND t.start_at - t.padding_before_seconds * INTERVAL '1 second' >= NOW() - ($1 * INTERVAL '1 second') \
             AND t.end_at + t.padding_after_seconds * INTERVAL '1 second' > NOW() \
             AND c.enabled=TRUE AND s.enabled=TRUE AND s.deleted_at IS NULL \
             AND NOT (s.id = ANY($2::uuid[])) AND NOT (c.item_id = ANY($3::uuid[])) \
             AND NOT (t.id = ANY($4::uuid[])) \
             ORDER BY t.start_at,t.id LIMIT $5",
        )
        .bind(MAX_CAPTURE_LATENESS.num_seconds())
        .bind(&skipped_sources)
        .bind(&skipped_channels)
        .bind(&skipped_timers)
        .bind(DUE_TIMER_CANDIDATE_BATCH)
        .fetch_all(&mut *tx)
        .await?;
        if candidates.is_empty() {
            break;
        }

        for candidate in candidates {
            let timer_id: Uuid = candidate.try_get("timer_id")?;
            let channel_item_id: Uuid = candidate.try_get("channel_item_id")?;
            let source_id: Uuid = candidate.try_get("source_id")?;
            if skipped_sources.contains(&source_id)
                || skipped_channels.contains(&channel_item_id)
                || skipped_timers.contains(&timer_id)
            {
                continue;
            }

            let source_locked: Option<Uuid> = sqlx::query_scalar(
                "SELECT id FROM live_tv_sources WHERE id=$1 AND enabled=TRUE AND deleted_at IS NULL FOR UPDATE SKIP LOCKED",
            )
            .bind(source_id)
            .fetch_optional(&mut *tx)
            .await?;
            if source_locked.is_none() {
                skipped_sources.push(source_id);
                sqlx::query("ROLLBACK TO SAVEPOINT livetv_claim_candidate")
                    .execute(&mut *tx)
                    .await?;
                continue;
            }
            let channel_locked: Option<Uuid> = sqlx::query_scalar(
                "SELECT item_id FROM live_tv_channels WHERE item_id=$1 AND source_id=$2 AND enabled=TRUE FOR UPDATE SKIP LOCKED",
            )
            .bind(channel_item_id)
            .bind(source_id)
            .fetch_optional(&mut *tx)
            .await?;
            if channel_locked.is_none() {
                skipped_channels.push(channel_item_id);
                sqlx::query("ROLLBACK TO SAVEPOINT livetv_claim_candidate")
                    .execute(&mut *tx)
                    .await?;
                continue;
            }
            let timer_locked: Option<Uuid> = sqlx::query_scalar(
                "SELECT id FROM live_tv_timers WHERE id=$1 AND status='scheduled' \
                 AND start_at - padding_before_seconds * INTERVAL '1 second' <= NOW() \
                 AND start_at - padding_before_seconds * INTERVAL '1 second' >= NOW() - ($2 * INTERVAL '1 second') \
                 AND end_at + padding_after_seconds * INTERVAL '1 second' > NOW() FOR UPDATE SKIP LOCKED",
            )
            .bind(timer_id)
            .bind(MAX_CAPTURE_LATENESS.num_seconds())
            .fetch_optional(&mut *tx)
            .await?;
            if timer_locked.is_none() {
                skipped_timers.push(timer_id);
                sqlx::query("ROLLBACK TO SAVEPOINT livetv_claim_candidate")
                    .execute(&mut *tx)
                    .await?;
                continue;
            }

            // The separate read follows the row locks and observes any
            // transaction that committed while this claim waited for its
            // source lock.
            let row = sqlx::query(
                "SELECT t.id AS timer_id,t.owner_user_id,t.channel_item_id,t.output_library_id, \
                 t.start_at,t.padding_before_seconds,t.end_at,t.padding_after_seconds,t.rating_system,t.content_rating, \
                 t.policy_rating_scale,t.policy_rating_value,c.library_id AS channel_library_id, \
                 c.name AS channel_name,c.stream_url,s.origin_pins, \
                 CASE WHEN t.program_id IS NOT NULL THEN p.title ELSE COALESCE(t.display_name,c.name) END AS title \
                 FROM live_tv_timers t JOIN live_tv_channels c ON c.item_id=t.channel_item_id \
                 JOIN live_tv_sources s ON s.id=c.source_id LEFT JOIN live_tv_programs p ON p.id=t.program_id \
                 WHERE t.id=$1 AND c.enabled=TRUE AND s.enabled=TRUE AND s.deleted_at IS NULL",
            )
            .bind(timer_id)
            .fetch_optional(&mut *tx)
            .await?;
            let Some(row) = row else {
                skipped_timers.push(timer_id);
                sqlx::query("ROLLBACK TO SAVEPOINT livetv_claim_candidate")
                    .execute(&mut *tx)
                    .await?;
                continue;
            };
            let claim = claim_from_row(&row)?;
            let changed = sqlx::query(
                "UPDATE live_tv_timers SET status='recording',claimed_run_id=$2,started_at=NOW(),updated_at=NOW() \
                 WHERE id=$1 AND status='scheduled'",
            )
            .bind(timer_id)
            .bind(state.run_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if changed != 1 {
                return Err(ApiError::Conflict(
                    "Timer claim changed concurrently".to_owned(),
                ));
            }
            sqlx::query(
                "INSERT INTO live_tv_recordings(id,timer_id,channel_item_id,library_id,channel_name,title, \
                 relative_path,status,claimed_run_id,rating_system,content_rating,policy_rating_scale,policy_rating_value) \
                 VALUES($1,$2,$3,$4,$5,$6,$7,'recording',$8,$9,$10,$11,$12)",
            )
            .bind(claim.id)
            .bind(timer_id)
            .bind(claim.channel_id)
            .bind(claim.output_library_id)
            .bind(&claim.channel_name)
            .bind(&claim.title)
            .bind(final_name(claim.id))
            .bind(state.run_id)
            .bind(&claim.rating_system)
            .bind(&claim.content_rating)
            .bind(&claim.policy_rating_scale)
            .bind(claim.policy_rating_value)
            .execute(&mut *tx)
            .await?;
            sqlx::query("RELEASE SAVEPOINT livetv_claim_candidate")
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(Some(claim));
        }
    }
    sqlx::query("RELEASE SAVEPOINT livetv_claim_candidate")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(None)
}

fn claim_from_row(row: &sqlx::postgres::PgRow) -> Result<RecordingClaim, ApiError> {
    let pins: Json<Vec<OriginPin>> = row.try_get("origin_pins")?;
    let timer_id: Uuid = row.try_get("timer_id")?;
    // Recording IDs intentionally equal their timer IDs, making output names
    // stable across restarts and avoiding any path text from the feed.
    let id = timer_id;
    let end_at: DateTime<Utc> = row.try_get("end_at")?;
    let padding: i32 = row.try_get("padding_after_seconds").unwrap_or(0);
    Ok(RecordingClaim {
        id,
        timer_id,
        owner_id: row.try_get("owner_user_id")?,
        channel_id: row.try_get("channel_item_id")?,
        channel_library_id: row.try_get("channel_library_id")?,
        output_library_id: row.try_get("output_library_id")?,
        channel_name: row.try_get("channel_name")?,
        title: row.try_get("title")?,
        source_url: row.try_get("stream_url")?,
        pins: pins.0,
        capture_end: end_at + ChronoDuration::seconds(i64::from(padding)),
        rating_system: row.try_get("rating_system").unwrap_or(None),
        content_rating: row.try_get("content_rating").unwrap_or(None),
        policy_rating_scale: row.try_get("policy_rating_scale").unwrap_or(None),
        policy_rating_value: row.try_get("policy_rating_value").unwrap_or(None),
    })
}

async fn run_recording(
    state: AppState,
    claim: RecordingClaim,
    mut cancel: watch::Receiver<bool>,
) -> Result<(), ApiError> {
    authorize_claim(&state, &claim).await?;
    ensure_claim_active(&state, &claim).await?;
    let now = Utc::now();
    let remaining = claim.capture_end - now;
    if remaining <= ChronoDuration::zero() {
        return Err(ApiError::Unavailable);
    }
    let remaining_std = remaining.to_std().map_err(|_| ApiError::Unavailable)?;
    if remaining_std > MAX_LIVE_INPUT_SECONDS {
        return Err(ApiError::RateLimited);
    }
    let stop_at = Instant::now() + remaining_std;
    let mut output = prepare_output(&state, claim.output_library_id, claim.id).await?;
    let open_timeout = SOURCE_OPEN_TIMEOUT.min(remaining_std);
    let mut input = tokio::select! {
        _ = cancel.changed() => return Err(ApiError::Conflict("Recording stopped".to_owned())),
        result = timeout(open_timeout, open_live_input(&claim.source_url, &claim.pins)) => {
            result.map_err(|_| ApiError::Unavailable)?.map_err(feed_error_to_api)?
        }
    };
    let (input_cancel_tx, mut input_cancel_rx) = oneshot::channel();
    let mut cancel_bridge = cancel.clone();
    let bridge = tokio::spawn(async move {
        if !*cancel_bridge.borrow() {
            let _ = cancel_bridge.changed().await;
        }
        let _ = input_cancel_tx.send(());
    });
    let copy_result = input
        .copy_recording_until(
            &mut output.file,
            &mut input_cancel_rx,
            MAX_LIVE_INPUT_BYTES,
            stop_at,
        )
        .await;
    bridge.abort();
    let copied = copy_result.map_err(feed_error_to_api)?;
    if *cancel.borrow() {
        return Err(ApiError::Conflict("Recording stopped".to_owned()));
    }
    ensure_claim_active(&state, &claim).await?;
    output
        .file
        .flush()
        .await
        .map_err(|_| ApiError::Unavailable)?;
    output
        .file
        .sync_all()
        .await
        .map_err(|_| ApiError::Unavailable)?;
    let prepared = prepare_publish(output, copied).await?;
    publish_and_complete(&state, &claim, prepared).await
}

async fn authorize_claim(state: &AppState, claim: &RecordingClaim) -> Result<(), ApiError> {
    let user = db::get_user(&state.db, claim.owner_id)
        .await?
        .ok_or(ApiError::Forbidden)?;
    if !recording_user_enabled(&user) {
        return Err(ApiError::Forbidden);
    }
    if !db::library_visible_to_user(&state.db, &user, claim.channel_library_id).await?
        || !db::library_visible_to_user(&state.db, &user, claim.output_library_id).await?
    {
        return Err(ApiError::Forbidden);
    }
    let channel = db::get_item(&state.db, claim.channel_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !db::item_visible_to_user(&state.db, &user, &channel).await?
        || !rating_visible(&user, claim.policy_rating_value, "LiveTvProgram")
    {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

fn recording_user_enabled(user: &UserRecord) -> bool {
    !user.disabled && user.allow_media_playback && (user.is_admin || user.enable_live_tv_access)
}

async fn lock_recording_policy_user(
    tx: &mut Transaction<'_, Postgres>,
    state: &AppState,
    claim: &RecordingClaim,
) -> Result<UserRecord, ApiError> {
    db::require_active_run(tx, state.run_id).await?;
    let row = sqlx::query(
        "SELECT id,username,is_admin,disabled,enable_remote_access,allow_media_playback, \
         enable_content_downloading,enable_live_tv_access,enable_live_tv_management, \
         restrict_libraries,max_parental_rating,block_unrated_items,ARRAY[]::uuid[] AS allowed_library_ids \
         FROM users WHERE id=$1 FOR UPDATE",
    )
    .bind(claim.owner_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(ApiError::Forbidden)?;
    let mut user = db::user_from_row(&row)?;
    user.allowed_library_ids = sqlx::query_scalar(
        "SELECT library_id FROM user_library_access WHERE user_id=$1 ORDER BY library_id",
    )
    .bind(claim.owner_id)
    .fetch_all(&mut **tx)
    .await?;
    if !recording_user_enabled(&user)
        || (user.restrict_libraries
            && !user.is_admin
            && (!user.allowed_library_ids.contains(&claim.channel_library_id)
                || !user.allowed_library_ids.contains(&claim.output_library_id)))
        || !rating_visible(&user, claim.policy_rating_value, "LiveTvProgram")
    {
        return Err(ApiError::Forbidden);
    }
    let channel_rating: Option<i16> = sqlx::query_scalar(
        "SELECT policy_rating_value FROM item_metadata WHERE item_id=$1 \
         AND provider_key='local-nfo' AND policy_rating_scale='US-MPAA-v1'",
    )
    .bind(claim.channel_id)
    .fetch_optional(&mut **tx)
    .await?
    .flatten();
    if !rating_visible(&user, channel_rating, "LiveTvChannel") {
        return Err(ApiError::Forbidden);
    }
    Ok(user)
}

fn rating_visible(user: &UserRecord, rating: Option<i16>, category: &str) -> bool {
    if user.is_admin {
        return true;
    }
    if user
        .max_parental_rating
        .is_some_and(|maximum| rating.is_some_and(|value| i32::from(value) > maximum))
    {
        return false;
    }
    rating.is_some()
        || !user
            .block_unrated_items
            .iter()
            .any(|blocked| blocked == category)
}

async fn prepare_output(
    state: &AppState,
    library_id: Uuid,
    recording_id: Uuid,
) -> Result<OutputFile, ApiError> {
    let library = db::get_library(&state.db, library_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if library.locations.is_empty() || library.locations.len() > 64 {
        return Err(ApiError::Unavailable);
    }
    let mut identities = Vec::with_capacity(library.locations.len());
    for root in &library.locations {
        if let Some(identity) = db::library_root_identity(&state.db, library_id, root).await? {
            identities.push((root.clone(), identity));
        }
    }
    let permit = super::secure_path::filesystem_permit()?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        for (root_path, (device, inode)) in identities {
            match open_root(&root_path, device, inode) {
                Ok(root) => match create_partial(root, root_path, recording_id) {
                    Ok(output) => return Ok(output),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        return Err(ApiError::Conflict(
                            "Recording path already exists".to_owned(),
                        ));
                    }
                    Err(_) => continue,
                },
                Err(_) => continue,
            }
        }
        Err(ApiError::Unavailable)
    })
    .await
    .map_err(|_| ApiError::Unavailable)?
}

fn open_root(path: &Path, expected_device: u64, expected_inode: u64) -> std::io::Result<File> {
    if !path.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "root is not absolute",
        ));
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let root = options.open(path)?;
    let metadata = root.metadata()?;
    if !metadata.is_dir() || metadata.dev() != expected_device || metadata.ino() != expected_inode {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "library root identity changed",
        ));
    }
    Ok(root)
}

fn create_partial(root: File, root_path: PathBuf, id: Uuid) -> std::io::Result<OutputFile> {
    let partial_name = partial_name(id);
    let final_name = final_name(id);
    let partial = CString::new(partial_name.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid name"))?;
    let descriptor = unsafe {
        libc::openat(
            root.as_raw_fd(),
            partial.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    Ok(OutputFile {
        root,
        file: TokioFile::from_std(file),
        final_path: root_path.join(&final_name),
        partial_name,
        final_name,
    })
}

async fn prepare_publish(output: OutputFile, copied: u64) -> Result<PreparedRecording, ApiError> {
    let OutputFile {
        root,
        file: async_file,
        partial_name,
        final_name,
        final_path,
    } = output;
    let file = async_file.into_std().await;
    let permit = super::secure_path::filesystem_permit()?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut file = file;
        let metadata = file.metadata().map_err(|_| ApiError::Unavailable)?;
        if !metadata.is_file() || metadata.len() > MAX_LIVE_INPUT_BYTES || copied == 0 {
            return Err(ApiError::Unavailable);
        }
        let (size, sha256) = validate_and_hash(&mut file)?;
        file.sync_all().map_err(|_| ApiError::Unavailable)?;
        let modified: DateTime<Utc> = DateTime::from(
            file.metadata()
                .map_err(|_| ApiError::Unavailable)?
                .modified()
                .map_err(|_| ApiError::Unavailable)?,
        );
        Ok(PreparedRecording {
            root,
            partial_name,
            final_name,
            final_path,
            size,
            sha256,
            modified,
        })
    })
    .await
    .map_err(|_| ApiError::Unavailable)?
}

async fn publish_and_complete(
    state: &AppState,
    claim: &RecordingClaim,
    prepared: PreparedRecording,
) -> Result<(), ApiError> {
    // Re-read policy after the entire capture and checksum pass. Keep the
    // owner's row lock through the atomic link so a revocation either wins
    // before publication or commits after the completed file becomes visible.
    authorize_claim(state, claim).await?;
    let mut owner_policy_tx = state.db.begin().await?;
    lock_recording_policy_user(&mut owner_policy_tx, state, claim).await?;

    // Persist the publishing state in its own committed transaction before
    // the filesystem link. Startup recovery can therefore reconcile a crash
    // after linkat without losing the durable publication intent.
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    // Both cancellation and publication lock the timer first. The separate
    // recording-row read is deliberately after that lock so it cannot use a
    // statement snapshot from before a concurrent cancel/publish transition.
    let timer =
        sqlx::query("SELECT status,claimed_run_id FROM live_tv_timers WHERE id=$1 FOR UPDATE")
            .bind(claim.timer_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(timer) = timer else {
        return Err(ApiError::Conflict(
            "Timer was cancelled before publication".to_owned(),
        ));
    };
    let timer_status: String = timer.try_get("status")?;
    let timer_run: Option<Uuid> = timer.try_get("claimed_run_id")?;
    if timer_status != "recording" || timer_run != Some(state.run_id) {
        return Err(ApiError::Conflict(
            "Timer was cancelled before publication".to_owned(),
        ));
    }
    let recording = sqlx::query(
        "SELECT status,claimed_run_id FROM live_tv_recordings WHERE id=$1 AND timer_id=$2 FOR UPDATE",
    )
    .bind(claim.id)
    .bind(claim.timer_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(recording) = recording else {
        return Err(ApiError::Conflict("Recording claim expired".to_owned()));
    };
    let recording_status: String = recording.try_get("status")?;
    let recording_run: Option<Uuid> = recording.try_get("claimed_run_id")?;
    if recording_status != "recording" || recording_run != Some(state.run_id) {
        return Err(ApiError::Conflict("Recording claim expired".to_owned()));
    }
    let updated = sqlx::query(
        "UPDATE live_tv_recordings SET status='publishing',byte_count=$3,sha256=$4,updated_at=NOW() \
         WHERE id=$1 AND status='recording' AND claimed_run_id=$2",
    )
    .bind(claim.id)
    .bind(state.run_id)
    .bind(prepared.size)
    .bind(&prepared.sha256)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated != 1 {
        return Err(ApiError::Conflict("Recording claim expired".to_owned()));
    }
    tx.commit().await?;

    // Keep lock order user→timer→recording, matching timer cancellation and
    // policy updates. `publishing` is the point of no return for cancellation.
    if !lock_publishing_rows(&mut owner_policy_tx, state.run_id, claim).await? {
        return Err(ApiError::Conflict("Recording claim expired".to_owned()));
    }

    // `publishing` is durable before the filesystem boundary. A crash after
    // linkat leaves enough state for startup recovery to verify and finish.
    let final_path = prepared.final_path.clone();
    let size = prepared.size;
    let checksum = prepared.sha256.clone();
    let modified = prepared.modified;
    publish_file(prepared).await?;
    owner_policy_tx.commit().await?;
    complete_recording(state, claim, &final_path, size, &checksum, Some(modified)).await
}

async fn lock_publishing_rows(
    tx: &mut Transaction<'_, Postgres>,
    run_id: Uuid,
    claim: &RecordingClaim,
) -> Result<bool, ApiError> {
    let timer =
        sqlx::query("SELECT status,claimed_run_id FROM live_tv_timers WHERE id=$1 FOR UPDATE")
            .bind(claim.timer_id)
            .fetch_optional(&mut **tx)
            .await?;
    let Some(timer) = timer else {
        return Ok(false);
    };
    let timer_status: String = timer.try_get("status")?;
    let timer_run: Option<Uuid> = timer.try_get("claimed_run_id")?;
    if timer_status != "recording" || timer_run != Some(run_id) {
        return Ok(false);
    }
    let recording = sqlx::query(
        "SELECT status,claimed_run_id FROM live_tv_recordings WHERE id=$1 AND timer_id=$2 FOR UPDATE",
    )
    .bind(claim.id)
    .bind(claim.timer_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(recording) = recording else {
        return Ok(false);
    };
    let recording_status: String = recording.try_get("status")?;
    let recording_run: Option<Uuid> = recording.try_get("claimed_run_id")?;
    Ok(recording_status == "publishing" && recording_run == Some(run_id))
}

async fn ensure_claim_active(state: &AppState, claim: &RecordingClaim) -> Result<(), ApiError> {
    let active: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM live_tv_timers WHERE id=$1 AND status='recording' AND claimed_run_id=$2)",
    )
    .bind(claim.timer_id)
    .bind(state.run_id)
    .fetch_one(&state.db)
    .await?;
    if active {
        Ok(())
    } else {
        Err(ApiError::Conflict(
            "Recording claim is no longer active".to_owned(),
        ))
    }
}

async fn claim_publishing_for_recovery(
    state: &AppState,
    claim: &RecordingClaim,
) -> Result<bool, ApiError> {
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    let timer = sqlx::query(
        "UPDATE live_tv_timers SET claimed_run_id=$2,updated_at=NOW() \
         WHERE id=$1 AND status='recording' AND claimed_run_id IS DISTINCT FROM $2 RETURNING id",
    )
    .bind(claim.timer_id)
    .bind(state.run_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(_) = timer else {
        tx.rollback().await?;
        return Ok(false);
    };
    let recording = sqlx::query(
        "UPDATE live_tv_recordings SET claimed_run_id=$2,updated_at=NOW() \
         WHERE id=$1 AND status='publishing' AND claimed_run_id IS DISTINCT FROM $2 RETURNING id",
    )
    .bind(claim.id)
    .bind(state.run_id)
    .fetch_optional(&mut *tx)
    .await?;
    if recording.is_none() {
        tx.rollback().await?;
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

async fn publish_file(prepared: PreparedRecording) -> Result<(), ApiError> {
    let permit = super::secure_path::filesystem_permit()?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let partial =
            CString::new(prepared.partial_name.as_bytes()).map_err(|_| ApiError::Unavailable)?;
        let final_name =
            CString::new(prepared.final_name.as_bytes()).map_err(|_| ApiError::Unavailable)?;
        let result = unsafe {
            libc::linkat(
                prepared.root.as_raw_fd(),
                partial.as_ptr(),
                prepared.root.as_raw_fd(),
                final_name.as_ptr(),
                0,
            )
        };
        if result != 0 {
            let link_error = std::io::Error::last_os_error();
            if link_error.raw_os_error() == Some(libc::EEXIST)
                && final_file_matches(
                    &prepared.root,
                    &prepared.final_name,
                    prepared.size,
                    &prepared.sha256,
                )
            {
                unsafe {
                    libc::unlinkat(prepared.root.as_raw_fd(), partial.as_ptr(), 0);
                    libc::fsync(prepared.root.as_raw_fd());
                }
                return Ok(());
            }
            return Err(ApiError::Unavailable);
        }
        unsafe {
            libc::unlinkat(prepared.root.as_raw_fd(), partial.as_ptr(), 0);
            libc::fsync(prepared.root.as_raw_fd());
        }
        Ok(())
    })
    .await
    .map_err(|_| ApiError::Unavailable)?
}

async fn complete_recording(
    state: &AppState,
    claim: &RecordingClaim,
    final_path: &Path,
    size: i64,
    checksum: &str,
    modified: Option<DateTime<Utc>>,
) -> Result<(), ApiError> {
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    let timer =
        sqlx::query("SELECT status,claimed_run_id FROM live_tv_timers WHERE id=$1 FOR UPDATE")
            .bind(claim.timer_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(timer) = timer else {
        return Err(ApiError::Conflict(
            "Timer claim changed before publication".to_owned(),
        ));
    };
    let timer_status: String = timer.try_get("status")?;
    let timer_run: Option<Uuid> = timer.try_get("claimed_run_id")?;
    if timer_status != "recording" || timer_run != Some(state.run_id) {
        return Err(ApiError::Conflict(
            "Timer claim changed before publication".to_owned(),
        ));
    }
    let recording = sqlx::query(
        "SELECT status,claimed_run_id FROM live_tv_recordings WHERE id=$1 AND timer_id=$2 FOR UPDATE",
    )
    .bind(claim.id)
    .bind(claim.timer_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(recording) = recording else {
        return Err(ApiError::Conflict(
            "Recording claim changed before publication".to_owned(),
        ));
    };
    let recording_status: String = recording.try_get("status")?;
    let recording_run: Option<Uuid> = recording.try_get("claimed_run_id")?;
    if recording_status != "publishing" || recording_run != Some(state.run_id) {
        return Err(ApiError::Conflict(
            "Recording claim changed before publication".to_owned(),
        ));
    }
    complete_recording_in_transaction(state, claim, final_path, size, checksum, modified, &mut tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn complete_recording_in_transaction(
    state: &AppState,
    claim: &RecordingClaim,
    final_path: &Path,
    size: i64,
    checksum: &str,
    modified: Option<DateTime<Utc>>,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<(), ApiError> {
    let library = db::get_library(&state.db, claim.output_library_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let item_type = recording_item_type(&library.collection_type);
    let path_text = final_path.to_str().ok_or(ApiError::Unavailable)?;
    let rating = (claim.policy_rating_scale.as_deref() == Some("US-PARENTAL-v1"))
        .then_some(claim.policy_rating_value)
        .flatten();
    let metadata = json!({
        "LiveTvRecording": true,
        "TimerId": claim.timer_id,
        "ChannelId": claim.channel_id,
        "RatingSystem": claim.rating_system,
        "OfficialRating": claim.content_rating,
        "PolicyRatingScale": claim.policy_rating_scale,
    });
    let title = bounded_name(&claim.title);
    let started_at: DateTime<Utc> = sqlx::query_scalar(
        "SELECT started_at FROM live_tv_timers WHERE id=$1 AND status='recording' AND claimed_run_id=$2 FOR UPDATE",
    )
    .bind(claim.timer_id)
    .bind(state.run_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| ApiError::Conflict("Timer claim changed before publication".to_owned()))?;
    let runtime_ticks = Utc::now()
        .signed_duration_since(started_at)
        .num_nanoseconds()
        .unwrap_or(i64::MAX)
        .max(0)
        / 100;
    let scan_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT scan_id FROM library_scan_state WHERE library_id=$1 AND status='running' LIMIT 1 FOR UPDATE",
    )
    .bind(claim.output_library_id)
    .fetch_optional(&mut **tx)
    .await?;
    let inserted = sqlx::query(
        "INSERT INTO items(id,library_id,parent_id,name,sort_name,item_type,path,path_hash,container, \
         size_bytes,runtime_ticks,date_modified,rating,overview,metadata_json,last_seen_scan) \
         VALUES($1,$2,NULL,$3,$4,$5,$6,$7,'ts',$8,$9,$10,$11,NULL,$12,$13) \
         ON CONFLICT(library_id,path_hash) DO UPDATE SET name=EXCLUDED.name,sort_name=EXCLUDED.sort_name, \
         item_type=EXCLUDED.item_type,container=EXCLUDED.container, \
         size_bytes=EXCLUDED.size_bytes,runtime_ticks=EXCLUDED.runtime_ticks,date_modified=EXCLUDED.date_modified, \
         rating=EXCLUDED.rating,metadata_json=EXCLUDED.metadata_json,last_seen_scan=EXCLUDED.last_seen_scan \
         WHERE items.path=EXCLUDED.path AND items.item_type <> 'LiveTvChannel' \
         RETURNING id",
    )
    .bind(claim.id)
    .bind(claim.output_library_id)
    .bind(&title)
    .bind(title.to_lowercase())
    .bind(item_type)
    .bind(path_text)
    .bind(db::path_hash(path_text))
    .bind(size)
    .bind(runtime_ticks.max(0))
    .bind(modified)
    .bind(rating)
    .bind(Json(metadata))
    .bind(scan_id)
    .fetch_optional(&mut **tx)
    .await?;
    let item_id: Uuid = inserted
        .ok_or_else(|| ApiError::Conflict("Recording catalog identity collision".to_owned()))?
        .try_get("id")?;
    let recording_updated = sqlx::query(
        "UPDATE live_tv_recordings SET status='completed',item_id=$3,claimed_run_id=NULL,finished_at=NOW(), \
         byte_count=$4,sha256=$5,last_error_code=NULL,updated_at=NOW() \
         WHERE id=$1 AND status='publishing' AND claimed_run_id=$2",
    )
    .bind(claim.id)
    .bind(state.run_id)
    .bind(item_id)
    .bind(size)
    .bind(checksum)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    let timer_updated = sqlx::query(
        "UPDATE live_tv_timers SET status='completed',claimed_run_id=NULL,finished_at=NOW(),updated_at=NOW() \
         WHERE id=$1 AND status='recording' AND claimed_run_id=$2",
    )
    .bind(claim.timer_id)
    .bind(state.run_id)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if recording_updated != 1 || timer_updated != 1 {
        return Err(ApiError::Conflict(
            "Timer claim changed before publication".to_owned(),
        ));
    }
    Ok(())
}

async fn fail_recording(
    state: &AppState,
    claim: &RecordingClaim,
    code: &'static str,
) -> Result<(), ApiError> {
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    let status = if code == "server-restarted" {
        "interrupted"
    } else {
        "failed"
    };
    // Keep the global lock order timer→recording used by cancellation and
    // publication. This avoids a deadlock when a source failure and DELETE
    // arrive at the same time.
    let timer =
        sqlx::query("SELECT status,claimed_run_id FROM live_tv_timers WHERE id=$1 FOR UPDATE")
            .bind(claim.timer_id)
            .fetch_optional(&mut *tx)
            .await?;
    let recording = sqlx::query(
        "SELECT status,claimed_run_id,item_id FROM live_tv_recordings WHERE id=$1 FOR UPDATE",
    )
    .bind(claim.id)
    .fetch_optional(&mut *tx)
    .await?;
    let (should_update_recording, should_update_timer, cleanup_terminal_failure) =
        match (timer, recording) {
            (Some(timer), Some(recording)) => {
                let timer_status: String = timer.try_get("status")?;
                let timer_run: Option<Uuid> = timer.try_get("claimed_run_id")?;
                let recording_status: String = recording.try_get("status")?;
                let recording_run: Option<Uuid> = recording.try_get("claimed_run_id")?;
                let item_id: Option<Uuid> = recording.try_get("item_id")?;
                let timer_is_active =
                    timer_status == "recording" && timer_run == Some(state.run_id);
                let timer_was_cancelled = timer_status == "cancelled" && timer_run.is_none();
                let recording_is_active =
                    matches!(recording_status.as_str(), "recording" | "publishing")
                        && recording_run == Some(state.run_id);
                (
                    recording_is_active && (timer_is_active || timer_was_cancelled),
                    recording_is_active && timer_is_active,
                    matches!(recording_status.as_str(), "failed" | "interrupted")
                        && recording_run.is_none()
                        && item_id.is_none(),
                )
            }
            _ => (false, false, false),
        };
    // Make terminal status the durable indication that output cleanup is
    // complete. Keeping both rows locked through the idempotent unlink also
    // ensures a retry/recovery cannot observe a terminal failure while a
    // visible partial (or final) file is still present.
    if should_update_recording || cleanup_terminal_failure {
        cleanup_files(state, claim.output_library_id, claim.id).await?;
    }
    if should_update_recording {
        sqlx::query(
            "UPDATE live_tv_recordings SET status=$3,claimed_run_id=NULL,finished_at=NOW(), \
             last_error_code=$4,updated_at=NOW() WHERE id=$1 AND claimed_run_id=$2 \
             AND status IN ('recording','publishing')",
        )
        .bind(claim.id)
        .bind(state.run_id)
        .bind(status)
        .bind(code)
        .execute(&mut *tx)
        .await?;
    }
    if should_update_timer {
        sqlx::query(
            "UPDATE live_tv_timers SET status=$3,claimed_run_id=NULL,finished_at=NOW(), \
             last_error_code=$4,updated_at=NOW() WHERE id=$1 AND claimed_run_id=$2 AND status='recording'",
        )
        .bind(claim.timer_id)
        .bind(state.run_id)
        .bind(status)
        .bind(code)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn classify_failure(
    state: &AppState,
    claim: &RecordingClaim,
    error: ApiError,
) -> &'static str {
    if state
        .shutdown_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return "server-restarted";
    }
    let cancelled: bool =
        sqlx::query_scalar("SELECT status='cancelled' FROM live_tv_timers WHERE id=$1")
            .bind(claim.timer_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
            .unwrap_or(false);
    if cancelled {
        return "source-unavailable";
    }
    match error {
        ApiError::RateLimited => "quota-exceeded",
        ApiError::Unavailable | ApiError::Forbidden | ApiError::NotFound => "source-unavailable",
        ApiError::Conflict(_) => "server-restarted",
        _ => "source-unavailable",
    }
}

fn feed_error_to_api(error: FeedError) -> ApiError {
    match error {
        FeedError::QuotaExceeded | FeedError::TooLarge | FeedError::LimitExceeded => {
            ApiError::RateLimited
        }
        FeedError::Cancelled => ApiError::Conflict("Recording stopped".to_owned()),
        FeedError::Unavailable | FeedError::DisallowedOrigin => ApiError::Unavailable,
        FeedError::UnsupportedTransport
        | FeedError::InvalidEncoding
        | FeedError::InvalidStructure
        | FeedError::InvalidValue
        | FeedError::DuplicateId => ApiError::BadRequest("Unsupported IPTV stream".to_owned()),
    }
}

fn recording_item_type(collection_type: &str) -> &'static str {
    match collection_type.to_ascii_lowercase().as_str() {
        "movies" => "Movie",
        _ => "Video",
    }
}

fn bounded_name(value: &str) -> String {
    let cleaned = value
        .chars()
        .filter(|character| !character.is_control())
        .take(512)
        .collect::<String>();
    if cleaned.trim().is_empty() {
        "Live TV recording".to_owned()
    } else {
        cleaned
    }
}

fn final_name(id: Uuid) -> String {
    format!("recording-{id}.ts")
}

fn partial_name(id: Uuid) -> String {
    format!(".puffinbox-recording-{id}.partial")
}

async fn cleanup_partial(state: &AppState, library_id: Uuid, id: Uuid) -> Result<(), ApiError> {
    cleanup_output_names(state, library_id, id, false).await
}

async fn cleanup_files(state: &AppState, library_id: Uuid, id: Uuid) -> Result<(), ApiError> {
    cleanup_output_names(state, library_id, id, true).await
}

async fn cleanup_output_names(
    state: &AppState,
    library_id: Uuid,
    id: Uuid,
    include_final: bool,
) -> Result<(), ApiError> {
    let library = db::get_library(&state.db, library_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if library.locations.is_empty() || library.locations.len() > 64 {
        return Err(ApiError::Unavailable);
    }
    let mut roots = Vec::new();
    for path in library.locations {
        let identity = db::library_root_identity(&state.db, library_id, &path)
            .await?
            .ok_or(ApiError::Unavailable)?;
        roots.push((path, identity));
    }
    let permit = super::secure_path::filesystem_permit()?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        // Validate every configured root before unlinking from any of them.
        // A missing or replaced root leaves the active DB row recoverable and
        // prevents cleanup from being treated as complete.
        let mut opened_roots = Vec::with_capacity(roots.len());
        for (path, (device, inode)) in roots {
            opened_roots.push(open_root(&path, device, inode).map_err(|_| ApiError::Unavailable)?);
        }
        for root in opened_roots {
            let mut names = vec![partial_name(id)];
            if include_final {
                names.push(final_name(id));
            }
            for name in names {
                let name = CString::new(name).map_err(|_| ApiError::Unavailable)?;
                let result = unsafe { libc::unlinkat(root.as_raw_fd(), name.as_ptr(), 0) };
                if result != 0
                    && std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT)
                {
                    return Err(ApiError::Unavailable);
                }
            }
            if unsafe { libc::fsync(root.as_raw_fd()) } != 0 {
                return Err(ApiError::Unavailable);
            }
        }
        Ok(())
    })
    .await
    .map_err(|_| ApiError::Unavailable)?
}

async fn mark_recovered_interrupted(
    state: &AppState,
    claim: &RecordingClaim,
) -> Result<(), ApiError> {
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    sqlx::query(
        "UPDATE live_tv_recordings SET status='interrupted',claimed_run_id=NULL,finished_at=NOW(), \
         last_error_code='server-restarted',updated_at=NOW() WHERE id=$1 AND status IN ('recording','publishing')",
    )
    .bind(claim.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE live_tv_timers SET status='interrupted',claimed_run_id=NULL,finished_at=NOW(), \
         last_error_code='server-restarted',updated_at=NOW() WHERE id=$1 AND status='recording'",
    )
    .bind(claim.timer_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn inspect_existing(
    state: &AppState,
    library_id: Uuid,
    _id: Uuid,
    partial: &str,
    final_file: &str,
    expected_size: i64,
    expected_hash: &str,
) -> Result<PreparedRecording, ApiError> {
    let roots = output_root_identities(state, library_id).await?;
    let permit = super::secure_path::filesystem_permit()?;
    let partial = partial.to_owned();
    let final_file = final_file.to_owned();
    let expected_hash = expected_hash.to_owned();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut opened_roots = Vec::with_capacity(roots.len());
        for (root_path, (device, inode)) in roots {
            let root = open_root(&root_path, device, inode).map_err(|_| ApiError::Unavailable)?;
            opened_roots.push((root_path, root));
        }
        for (root_path, root) in opened_roots {
            for (name, is_partial) in [(final_file.clone(), false), (partial.clone(), true)] {
                let c_name = CString::new(name.as_bytes()).map_err(|_| ApiError::Unavailable)?;
                let fd = unsafe {
                    libc::openat(
                        root.as_raw_fd(),
                        c_name.as_ptr(),
                        libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
                        continue;
                    }
                    return Err(ApiError::Unavailable);
                }
                let mut file = unsafe { File::from_raw_fd(fd) };
                let metadata = file.metadata().map_err(|_| ApiError::Unavailable)?;
                if !metadata.is_file() || metadata.len() != expected_size as u64 {
                    continue;
                }
                file.seek(SeekFrom::Start(0))
                    .map_err(|_| ApiError::Unavailable)?;
                let (size, hash) = validate_and_hash(&mut file)?;
                if size == expected_size && hash == expected_hash {
                    let modified =
                        DateTime::from(metadata.modified().map_err(|_| ApiError::Unavailable)?);
                    let partial_name = if is_partial {
                        name.clone()
                    } else {
                        String::new()
                    };
                    return Ok(PreparedRecording {
                        root,
                        partial_name,
                        final_name: final_file.clone(),
                        final_path: root_path.join(&final_file),
                        size,
                        sha256: hash,
                        modified,
                    });
                }
            }
        }
        Err(ApiError::NotFound)
    })
    .await
    .map_err(|_| ApiError::Unavailable)?
}

async fn output_root_identities(
    state: &AppState,
    library_id: Uuid,
) -> Result<Vec<(PathBuf, (u64, u64))>, ApiError> {
    let library = db::get_library(&state.db, library_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if library.locations.is_empty() || library.locations.len() > 64 {
        return Err(ApiError::Unavailable);
    }
    let mut roots = Vec::with_capacity(library.locations.len());
    for path in library.locations {
        let identity = db::library_root_identity(&state.db, library_id, &path)
            .await?
            .ok_or(ApiError::Unavailable)?;
        roots.push((path, identity));
    }
    Ok(roots)
}

fn validate_and_hash(file: &mut File) -> Result<(i64, String), ApiError> {
    let metadata = file.metadata().map_err(|_| ApiError::Unavailable)?;
    if !metadata.is_file() || metadata.len() > MAX_LIVE_INPUT_BYTES {
        return Err(ApiError::Unavailable);
    }
    let aligned_size = metadata.len() - metadata.len() % TS_PACKET_BYTES as u64;
    if aligned_size == 0 {
        return Err(ApiError::Unavailable);
    }
    file.set_len(aligned_size)
        .map_err(|_| ApiError::Unavailable)?;
    file.seek(SeekFrom::Start(0))
        .map_err(|_| ApiError::Unavailable)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; TS_PACKET_BYTES * TS_READ_PACKETS];
    let mut offset = 0_u64;
    loop {
        let count = file.read(&mut buffer).map_err(|_| ApiError::Unavailable)?;
        if count == 0 {
            break;
        }
        if count % TS_PACKET_BYTES != 0
            || buffer[..count]
                .as_chunks::<TS_PACKET_BYTES>()
                .0
                .iter()
                .any(|packet| packet[0] != 0x47)
        {
            return Err(ApiError::Unavailable);
        }
        digest.update(&buffer[..count]);
        offset += count as u64;
    }
    if offset != aligned_size {
        return Err(ApiError::Unavailable);
    }
    Ok((
        i64::try_from(aligned_size).map_err(|_| ApiError::Unavailable)?,
        encode_hex(&digest.finalize()),
    ))
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[usize::from(byte >> 4)] as char);
        output.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    output
}

fn final_file_matches(root: &File, name: &str, expected_size: i64, expected_hash: &str) -> bool {
    let Ok(name) = CString::new(name) else {
        return false;
    };
    let descriptor = unsafe {
        libc::openat(
            root.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return false;
    }
    let mut file = unsafe { File::from_raw_fd(descriptor) };
    file.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() == expected_size as u64)
        && validate_and_hash(&mut file)
            .is_ok_and(|(size, hash)| size == expected_size && hash == expected_hash)
}

#[cfg(test)]
mod tests {
    use super::{
        RecordingClaim, RecordingJobResult, claim_due_timer, final_name, partial_name,
        recording_item_type, recording_job_result, recording_user_enabled, take_joined_task_claim,
        validate_and_hash,
    };
    use crate::{auth::UserRecord, config::Config, state::AppState};
    use sqlx::postgres::PgPoolOptions;
    use std::{env, fs::File, io::Write, path::PathBuf, sync::Arc, time::Duration};
    use uuid::Uuid;

    fn sample_claim(timer_id: Uuid) -> RecordingClaim {
        RecordingClaim {
            id: Uuid::new_v4(),
            timer_id,
            owner_id: Uuid::new_v4(),
            channel_id: Uuid::new_v4(),
            channel_library_id: Uuid::new_v4(),
            output_library_id: Uuid::new_v4(),
            channel_name: "Fixture channel".to_owned(),
            title: "Fixture recording".to_owned(),
            source_url: "http://127.0.0.1/fixture.ts".to_owned(),
            pins: Vec::new(),
            capture_end: chrono::Utc::now(),
            rating_system: None,
            content_rating: None,
            policy_rating_scale: None,
            policy_rating_value: None,
        }
    }

    #[tokio::test]
    async fn panicking_recorder_future_returns_its_claim_for_finalization() {
        let timer_id = Uuid::new_v4();
        let claim = sample_claim(timer_id);
        let panicking_recording =
            std::future::poll_fn(|_| -> std::task::Poll<Result<(), crate::ApiError>> {
                panic!("injected recorder panic")
            });

        match recording_job_result(claim, panicking_recording).await {
            RecordingJobResult::Failed(claim, crate::ApiError::Unavailable) => {
                assert_eq!(claim.timer_id, timer_id);
            }
            RecordingJobResult::Finished(_) => panic!("panicking worker was reported as finished"),
            RecordingJobResult::Failed(_, _) => {
                panic!("panicking worker did not report an unavailable failure")
            }
        }
    }

    #[tokio::test]
    async fn join_error_recovers_the_claim_for_worker_or_finalizer_retry() {
        let timer_id = Uuid::new_v4();
        let claim = sample_claim(timer_id);
        let mut tasks = tokio::task::JoinSet::<()>::new();
        let task_id = tasks.spawn(async { panic!("injected task panic") }).id();
        let mut claims = std::collections::HashMap::from([(task_id, claim)]);
        let error = tasks
            .join_next_with_id()
            .await
            .expect("panicked task returns a join result")
            .expect_err("injected task should panic");

        let recovered = take_joined_task_claim(&error, &mut claims).unwrap();
        assert_eq!(recovered.timer_id, timer_id);
        assert!(claims.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
    async fn due_timer_claim_serializes_with_stale_channel_cleanup() {
        let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
            .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
        let admin_pool = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect(&database_url)
            .await
            .unwrap();
        let schema = format!("puffinbox_livetv_claim_race_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
            .execute(&admin_pool)
            .await
            .unwrap();

        let connection_schema = schema.clone();
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(5))
            .after_connect(move |connection, _metadata| {
                let schema = connection_schema.clone();
                Box::pin(async move {
                    sqlx::query(&format!("SET search_path TO \"{schema}\""))
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url)
            .await
            .unwrap();
        sqlx::raw_sql(
            "CREATE TABLE instance_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL); \
             CREATE TABLE live_tv_sources(id UUID PRIMARY KEY,enabled BOOLEAN NOT NULL, \
             deleted_at TIMESTAMPTZ,origin_pins JSONB NOT NULL); \
             CREATE TABLE live_tv_channels(item_id UUID PRIMARY KEY,source_id UUID NOT NULL, \
             library_id UUID NOT NULL,name TEXT NOT NULL,enabled BOOLEAN NOT NULL, \
             stream_url TEXT,logo_url TEXT); \
             CREATE TABLE live_tv_timers(id UUID PRIMARY KEY,owner_user_id UUID NOT NULL, \
             channel_item_id UUID NOT NULL,output_library_id UUID NOT NULL,status TEXT NOT NULL, \
             start_at TIMESTAMPTZ NOT NULL,padding_before_seconds INTEGER NOT NULL, \
             end_at TIMESTAMPTZ NOT NULL,padding_after_seconds INTEGER NOT NULL, \
             rating_system TEXT,content_rating TEXT,policy_rating_scale TEXT, \
             policy_rating_value SMALLINT,program_id UUID,display_name TEXT,claimed_run_id UUID, \
             started_at TIMESTAMPTZ,finished_at TIMESTAMPTZ,last_error_code TEXT,updated_at TIMESTAMPTZ); \
             CREATE TABLE live_tv_programs(id UUID PRIMARY KEY,title TEXT NOT NULL); \
             CREATE TABLE live_tv_recordings(id UUID PRIMARY KEY,timer_id UUID NOT NULL, \
             channel_item_id UUID NOT NULL,library_id UUID NOT NULL,channel_name TEXT NOT NULL, \
             title TEXT NOT NULL,relative_path TEXT NOT NULL,status TEXT NOT NULL, \
             claimed_run_id UUID,rating_system TEXT,content_rating TEXT, \
             policy_rating_scale TEXT,policy_rating_value SMALLINT);",
        )
        .execute(&pool)
        .await
        .unwrap();

        let run_id = Uuid::new_v4();
        let library_id = Uuid::new_v4();
        let first_source_id = Uuid::new_v4();
        let second_source_id = Uuid::new_v4();
        let first_channel_id = Uuid::new_v4();
        let second_channel_id = Uuid::new_v4();
        let first_timer_ids: Vec<Uuid> = (0..=super::DUE_TIMER_CANDIDATE_BATCH as usize)
            .map(|_| Uuid::new_v4())
            .collect();
        let second_timer_id = Uuid::new_v4();
        let second_fallback_timer_id = Uuid::new_v4();
        let owner_id = Uuid::new_v4();
        sqlx::query("INSERT INTO instance_meta(key,value) VALUES('active_run_id',$1)")
            .bind(run_id.to_string())
            .execute(&pool)
            .await
            .unwrap();
        for source_id in [first_source_id, second_source_id] {
            sqlx::query(
                "INSERT INTO live_tv_sources(id,enabled,origin_pins) VALUES($1,TRUE,'[]'::jsonb)",
            )
            .bind(source_id)
            .execute(&pool)
            .await
            .unwrap();
        }
        for (channel_id, source_id, name, suffix) in [
            (first_channel_id, first_source_id, "first", "first"),
            (second_channel_id, second_source_id, "second", "second"),
        ] {
            sqlx::query(
                "INSERT INTO live_tv_channels(item_id,source_id,library_id,name,enabled,stream_url,logo_url) \
                 VALUES($1,$2,$3,$4,TRUE,$5,$6)",
            )
            .bind(channel_id)
            .bind(source_id)
            .bind(library_id)
            .bind(name)
            .bind(format!("https://feed.example/{suffix}.ts?token=fixture"))
            .bind(format!("https://feed.example/{suffix}.png?token=fixture"))
            .execute(&pool)
            .await
            .unwrap();
        }
        for timer_id in &first_timer_ids {
            sqlx::query(
                "INSERT INTO live_tv_timers(id,owner_user_id,channel_item_id,output_library_id,status, \
                 start_at,padding_before_seconds,end_at,padding_after_seconds,updated_at) \
                 VALUES($1,$2,$3,$4,'scheduled',NOW()-($5 * INTERVAL '1 second'),0, \
                 NOW()+INTERVAL '30 minutes',0,NOW())",
            )
            .bind(timer_id)
            .bind(owner_id)
            .bind(first_channel_id)
            .bind(library_id)
            .bind(60)
            .execute(&pool)
            .await
            .unwrap();
        }
        for (timer_id, start_offset) in [(second_timer_id, 30), (second_fallback_timer_id, 25)] {
            sqlx::query(
                "INSERT INTO live_tv_timers(id,owner_user_id,channel_item_id,output_library_id,status, \
                 start_at,padding_before_seconds,end_at,padding_after_seconds,updated_at) \
                 VALUES($1,$2,$3,$4,'scheduled',NOW()-($5 * INTERVAL '1 second'),0, \
                 NOW()+INTERVAL '30 minutes',0,NOW())",
            )
            .bind(timer_id)
            .bind(owner_id)
            .bind(second_channel_id)
            .bind(library_id)
            .bind(start_offset)
            .execute(&pool)
            .await
            .unwrap();
        }

        let config = Arc::new(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            public_base_url: None,
            database_url: database_url.clone(),
            server_name: "Live TV claim lock fixture".to_owned(),
            web_root: PathBuf::new(),
            data_dir: PathBuf::new(),
            ffmpeg_path: None,
            max_scan_workers: 1,
            max_page_size: 20,
            access_token_lifetime_hours: 24,
            cookie_secure: false,
            cors_origins: Vec::new(),
            trusted_proxies: Vec::new(),
            local_networks: Vec::new(),
            setup_token: None,
            bootstrap_admin_username: None,
            bootstrap_admin_password: None,
        });
        let state = AppState::new_for_run(pool.clone(), config, Uuid::new_v4(), run_id, None);

        // Hold the first source lock while its omitted-channel cleanup disables
        // the channel. The recorder must skip this source and claim the next
        // due timer from another source in the same scheduler pass.
        let mut cleanup_tx = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM live_tv_sources WHERE id=$1 FOR UPDATE")
            .bind(first_source_id)
            .fetch_one(&mut *cleanup_tx)
            .await
            .unwrap();
        let mut timer_lock_tx = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM live_tv_timers WHERE id=$1 FOR UPDATE")
            .bind(second_timer_id)
            .fetch_one(&mut *timer_lock_tx)
            .await
            .unwrap();
        let claim = claim_due_timer(&state).await.unwrap().unwrap();
        assert_eq!(claim.timer_id, second_fallback_timer_id);
        timer_lock_tx.rollback().await.unwrap();
        sqlx::query(
            "UPDATE live_tv_channels SET enabled=FALSE,stream_url=NULL,logo_url=NULL WHERE item_id=$1",
        )
        .bind(first_channel_id)
        .execute(&mut *cleanup_tx)
        .await
        .unwrap();
        cleanup_tx.commit().await.unwrap();
        let first_scheduled_timers: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM live_tv_timers WHERE id=ANY($1) AND status='scheduled'",
        )
        .bind(&first_timer_ids)
        .fetch_one(&pool)
        .await
        .unwrap();
        let first_channel_urls: (Option<String>, Option<String>) =
            sqlx::query_as("SELECT stream_url,logo_url FROM live_tv_channels WHERE item_id=$1")
                .bind(first_channel_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(first_scheduled_timers, first_timer_ids.len() as i64);
        assert_eq!(first_channel_urls, (None, None));
        let skipped_timer_status: String =
            sqlx::query_scalar("SELECT status FROM live_tv_timers WHERE id=$1")
                .bind(second_timer_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(skipped_timer_status, "scheduled");

        // A timer claim commits its recording row before refresh can inspect
        // active captures. Cleanup then retains the feed URLs needed by
        // recorder recovery.
        let mut refresh_tx = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM live_tv_sources WHERE id=$1 FOR UPDATE")
            .bind(second_source_id)
            .fetch_one(&mut *refresh_tx)
            .await
            .unwrap();
        let active_capture_ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM live_tv_recordings WHERE channel_item_id=$1 \
             AND status IN ('recording','publishing') ORDER BY id FOR UPDATE",
        )
        .bind(second_channel_id)
        .fetch_all(&mut *refresh_tx)
        .await
        .unwrap();
        assert_eq!(active_capture_ids, vec![second_fallback_timer_id]);
        sqlx::query(
            "UPDATE live_tv_channels c SET enabled=FALSE, \
             stream_url=CASE WHEN EXISTS(SELECT 1 FROM live_tv_recordings r \
             WHERE r.channel_item_id=c.item_id AND r.status IN ('recording','publishing')) \
             THEN c.stream_url ELSE NULL END, \
             logo_url=CASE WHEN EXISTS(SELECT 1 FROM live_tv_recordings r \
             WHERE r.channel_item_id=c.item_id AND r.status IN ('recording','publishing')) \
             THEN c.logo_url ELSE NULL END WHERE c.item_id=$1",
        )
        .bind(second_channel_id)
        .execute(&mut *refresh_tx)
        .await
        .unwrap();
        refresh_tx.commit().await.unwrap();
        let second_channel: (bool, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT enabled,stream_url,logo_url FROM live_tv_channels WHERE item_id=$1",
        )
        .bind(second_channel_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!second_channel.0);
        assert!(second_channel.1.is_some());
        assert!(second_channel.2.is_some());

        pool.close().await;
        sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
            .execute(&admin_pool)
            .await
            .unwrap();
        admin_pool.close().await;
    }

    #[test]
    fn recording_names_are_deterministic_and_hidden_partial_is_private() {
        let id = Uuid::nil();
        assert_eq!(
            final_name(id),
            "recording-00000000-0000-0000-0000-000000000000.ts"
        );
        assert!(partial_name(id).starts_with('.'));
    }

    #[test]
    fn captured_transport_stream_is_packet_validated_and_checksummed() {
        let path = std::env::temp_dir().join(format!("puffinbox-ts-{}.ts", Uuid::new_v4()));
        let mut file = File::create(&path).unwrap();
        let mut packet = [0_u8; 188];
        packet[0] = 0x47;
        file.write_all(&packet).unwrap();
        file.write_all(&packet).unwrap();
        drop(file);
        let mut file = File::options().read(true).write(true).open(&path).unwrap();
        let (size, hash) = validate_and_hash(&mut file).unwrap();
        assert_eq!(size, 376);
        assert_eq!(hash.len(), 64);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn incomplete_packet_tail_is_truncated_but_bad_sync_bytes_fail() {
        let path = std::env::temp_dir().join(format!("puffinbox-ts-{}.ts", Uuid::new_v4()));
        let mut file = File::create(&path).unwrap();
        let mut packet = [0_u8; 188];
        packet[0] = 0x47;
        file.write_all(&packet).unwrap();
        file.write_all(b"tail").unwrap();
        drop(file);
        let mut file = File::options().read(true).write(true).open(&path).unwrap();
        assert_eq!(validate_and_hash(&mut file).unwrap().0, 188);
        drop(file);
        std::fs::write(&path, [0_u8; 188]).unwrap();
        let mut file = File::options().read(true).write(true).open(&path).unwrap();
        assert!(validate_and_hash(&mut file).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn recording_catalog_type_is_video_playable() {
        assert_eq!(recording_item_type("mixed"), "Video");
        assert_eq!(recording_item_type("movies"), "Movie");
    }

    #[test]
    fn recording_requires_current_live_tv_access_but_admin_bypasses_stored_flag() {
        let mut user = UserRecord {
            id: Uuid::new_v4(),
            username: "viewer".to_owned(),
            is_admin: false,
            disabled: false,
            enable_remote_access: true,
            allow_media_playback: true,
            enable_content_downloading: true,
            enable_live_tv_access: false,
            enable_live_tv_management: false,
            restrict_libraries: false,
            max_parental_rating: None,
            block_unrated_items: Vec::new(),
            allowed_library_ids: Vec::new(),
        };
        assert!(!recording_user_enabled(&user));

        user.enable_live_tv_access = true;
        assert!(recording_user_enabled(&user));

        user.enable_live_tv_access = false;
        user.is_admin = true;
        assert!(recording_user_enabled(&user));

        user.disabled = true;
        assert!(!recording_user_enabled(&user));
    }
}
