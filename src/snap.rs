use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use axum::body::{Body, HttpBody};
use chrono::{TimeDelta, Utc};
use serde::Serialize;
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{Mutex, Semaphore},
    time::{MissedTickBehavior, interval, timeout},
};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    config::Config,
    error::{AppError, concise_output},
    state::{Expiration, LifecycleStatus, StateStore},
    validation,
};

const SNAP: &str = "/usr/bin/snap";
const UNSQUASHFS: &str = "/usr/bin/unsquashfs";
const READINESS_TIMEOUT: Duration = Duration::from_secs(5);
const CHANGE_POLL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Serialize)]
pub struct InstallResult {
    pub name: String,
    pub installed_at: chrono::DateTime<Utc>,
    pub expires_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct LogsResult {
    pub target: String,
    pub lines: u16,
    pub logs: String,
}

#[derive(Debug, Serialize)]
pub struct ActionResult {
    pub target: String,
    pub action: &'static str,
    pub output: String,
}

#[derive(Clone)]
pub struct SnapManager {
    config: Arc<Config>,
    store: StateStore,
    mutation: Arc<Mutex<()>>,
    uploads: Arc<Semaphore>,
    ready: Arc<AtomicBool>,
}

impl SnapManager {
    pub fn new(config: Arc<Config>, store: StateStore) -> Self {
        Self {
            config,
            store,
            mutation: Arc::new(Mutex::new(())),
            uploads: Arc::new(Semaphore::new(2)),
            ready: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    pub async fn refresh_readiness(&self) -> Result<(), AppError> {
        let result = self.check_readiness().await;
        self.ready.store(result.is_ok(), Ordering::Relaxed);
        result
    }

    pub async fn list_managed(&self) -> Vec<Expiration> {
        self.store.list().await
    }

    pub async fn install(
        &self,
        body: Body,
        content_length: Option<u64>,
        lifetime_seconds: u64,
    ) -> Result<InstallResult, AppError> {
        if lifetime_seconds == 0 || lifetime_seconds > self.config.max_lifetime.as_secs() {
            return Err(AppError::BadRequest(format!(
                "lifetime_seconds must be between 1 and {}",
                self.config.max_lifetime.as_secs()
            )));
        }
        if content_length.is_some_and(|length| length > self.config.max_upload_bytes) {
            return Err(AppError::PayloadTooLarge {
                limit: self.config.max_upload_bytes,
            });
        }
        let _upload_permit = self
            .uploads
            .try_acquire()
            .map_err(|_| AppError::InstallCapacity)?;

        let uploaded = self.receive_upload(body).await?;
        self.validate_snap_magic(uploaded.path()).await?;
        let metadata = self.read_snap_metadata(uploaded.path()).await?;
        if metadata.confinement.as_deref() != Some("strict") {
            return Err(AppError::BadRequest(
                "only snaps with strict confinement are accepted".to_owned(),
            ));
        }
        if !matches!(metadata.snap_type.as_deref(), None | Some("app")) {
            return Err(AppError::BadRequest(
                "system, base, gadget, kernel, and snapd snaps are not accepted".to_owned(),
            ));
        }
        let name = metadata.name;
        if is_reserved_snap_name(&name) {
            return Err(AppError::BadRequest(
                "reserved system snap names are not accepted".to_owned(),
            ));
        }
        let _guard = self.mutation.lock().await;

        if self.store.get(&name).await.is_some() {
            return Err(AppError::Conflict(format!(
                "snap {name} already has a managed or pending installation"
            )));
        }
        if self.installed_status(&name).await? {
            return Err(AppError::Conflict(format!(
                "snap {name} is already installed; remove it before installing a test build"
            )));
        }

        let pending_since = Utc::now();
        let lifetime = duration_delta(lifetime_seconds, "lifetime_seconds is too large")?;
        let provisional_expiry = checked_deadline(pending_since, lifetime, "snap lifetime")?;
        let record = Expiration {
            name: name.clone(),
            installed_at: pending_since,
            expires_at: provisional_expiry,
            status: LifecycleStatus::Pending,
            pending_since: Some(pending_since),
            requested_lifetime_seconds: Some(lifetime_seconds),
            change_id: None,
        };
        self.store.set(record.clone()).await?;

        let start = self
            .run_command(
                SNAP,
                vec![
                    "install".into(),
                    "--dangerous".into(),
                    "--no-wait".into(),
                    uploaded.path().as_os_str().to_owned(),
                ],
                "start snap install",
            )
            .await;
        let output = match start {
            Ok(output) => output,
            Err(error) => {
                if matches!(&error, AppError::CommandTimeout { .. }) {
                    warn!(snap = %name, %error, "starting install timed out; retaining pending state for reconciliation");
                } else if let Err(cleanup_error) = self.store.remove(&name).await {
                    warn!(snap = %name, %cleanup_error, "failed to remove state after a definitive install failure");
                }
                return Err(error);
            }
        };
        let change_id = match parse_change_id(&output.stdout) {
            Ok(change_id) => change_id,
            Err(error) => {
                warn!(snap = %name, %error, "snapd returned an invalid change id; retaining pending state for discovery");
                return Err(error);
            }
        };
        let mut record = record;
        record.change_id = Some(change_id.clone());
        self.store.set(record).await?;

        if let Err(error) = self
            .run_command(
                SNAP,
                vec!["watch".into(), change_id.into()],
                "watch snap install",
            )
            .await
        {
            if matches!(&error, AppError::CommandTimeout { .. }) {
                warn!(snap = %name, %error, "install is still pending; retaining state for reconciliation");
                return Err(error);
            }
            self.resolve_failed_install(&name).await?;
            return Err(error);
        }
        if !self.installed_status(&name).await? {
            self.store.remove(&name).await?;
            return Err(AppError::Command {
                operation: "snap install",
                message: "snapd reported success but the expected snap is not installed".to_owned(),
            });
        }
        let record = self.promote_pending(&name, Utc::now()).await?;

        info!(snap = %name, expires_at = %record.expires_at, "installed test snap");
        Ok(InstallResult {
            name,
            installed_at: record.installed_at,
            expires_at: record.expires_at,
        })
    }

    pub async fn remove(&self, name: &str, purge: bool) -> Result<(), AppError> {
        validation::snap_name(name)?;
        let _guard = self.mutation.lock().await;
        self.ensure_managed(name).await?;
        self.remove_locked(name, purge).await?;
        self.store.remove(name).await?;
        info!(snap = %name, purge, "removed snap");
        Ok(())
    }

    pub async fn get_config(&self, name: &str, key: Option<&str>) -> Result<Value, AppError> {
        validation::snap_name(name)?;
        self.ensure_managed(name).await?;
        if let Some(key) = key {
            validation::config_key(key)?;
        }
        let mut args = vec!["get".into(), "-d".into(), name.into()];
        if let Some(key) = key {
            args.push(key.into());
        }
        let output = self.run_command(SNAP, args, "snap get").await?;
        serde_json::from_slice(&output.stdout).map_err(|error| AppError::Command {
            operation: "snap get",
            message: format!("snap returned invalid JSON: {error}"),
        })
    }

    pub async fn set_config(&self, name: &str, key: &str, value: Value) -> Result<(), AppError> {
        validation::snap_name(name)?;
        validation::config_key(key)?;
        let encoded = serde_json::to_string(&value)
            .map_err(|error| AppError::BadRequest(format!("invalid JSON value: {error}")))?;
        if encoded.len() > 64 * 1024 {
            return Err(AppError::BadRequest(
                "configuration value exceeds 64 KiB".to_owned(),
            ));
        }
        let _guard = self.mutation.lock().await;
        self.ensure_managed(name).await?;
        self.run_command(
            SNAP,
            vec![
                "set".into(),
                "-t".into(),
                name.into(),
                format!("{key}={encoded}").into(),
            ],
            "snap set",
        )
        .await?;
        Ok(())
    }

    pub async fn unset_config(&self, name: &str, key: &str) -> Result<(), AppError> {
        validation::snap_name(name)?;
        validation::config_key(key)?;
        let _guard = self.mutation.lock().await;
        self.ensure_managed(name).await?;
        self.run_command(
            SNAP,
            vec!["unset".into(), name.into(), key.into()],
            "snap unset",
        )
        .await?;
        Ok(())
    }

    pub async fn logs(
        &self,
        name: &str,
        service: Option<&str>,
        lines: u16,
    ) -> Result<LogsResult, AppError> {
        let target = service_target(name, service)?;
        self.ensure_managed(name).await?;
        let output = self
            .run_command(
                SNAP,
                vec![
                    "logs".into(),
                    "-n".into(),
                    lines.to_string().into(),
                    target.clone().into(),
                ],
                "snap logs",
            )
            .await?;
        Ok(LogsResult {
            target,
            lines,
            logs: String::from_utf8_lossy(&output.stdout).into_owned(),
        })
    }

    pub async fn service_action(
        &self,
        name: &str,
        service: Option<&str>,
        action: &'static str,
    ) -> Result<ActionResult, AppError> {
        let target = service_target(name, service)?;
        let _guard = self.mutation.lock().await;
        self.ensure_managed(name).await?;
        let output = self
            .run_command(
                SNAP,
                vec![action.into(), target.clone().into()],
                "snap service action",
            )
            .await?;
        Ok(ActionResult {
            target,
            action,
            output: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        })
    }

    pub async fn reconcile_state(&self) -> Result<(), AppError> {
        for record in self.store.list().await {
            if let Err(error) = self.reconcile_record(&record.name, Utc::now()).await {
                warn!(snap = %record.name, %error, "startup reconciliation failed; will retry");
            }
        }
        Ok(())
    }

    pub async fn run_maintenance_loop(self: Arc<Self>) {
        let mut ticker = interval(std::time::Duration::from_secs(5));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if !self.is_ready() {
                continue;
            }
            for record in self.store.list().await {
                if let Err(error) = self.reconcile_record(&record.name, Utc::now()).await {
                    warn!(snap = %record.name, %error, "failed to reconcile managed snap; will retry");
                }
            }
        }
    }

    pub async fn run_readiness_loop(self: Arc<Self>) {
        let mut ticker = interval(std::time::Duration::from_secs(5));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if let Err(error) = self.refresh_readiness().await {
                warn!(%error, "snapd readiness probe failed");
            }
        }
    }

    async fn reconcile_record(
        &self,
        name: &str,
        now: chrono::DateTime<Utc>,
    ) -> Result<(), AppError> {
        let _guard = self.mutation.lock().await;
        let Some(record) = self.store.get(name).await else {
            return Ok(());
        };

        if record.status == LifecycleStatus::Installed {
            if !self.installed_status(name).await? {
                self.store.remove(name).await?;
                warn!(snap = %name, "removed state for a snap no longer installed");
            } else if record.expires_at <= now {
                self.remove_locked(name, true).await?;
                self.store.remove(name).await?;
                info!(snap = %name, "purged expired snap");
            }
            return Ok(());
        }

        self.reconcile_pending(record, now).await
    }

    async fn reconcile_pending(
        &self,
        mut record: Expiration,
        now: chrono::DateTime<Utc>,
    ) -> Result<(), AppError> {
        let stale = pending_is_stale(&record, now, self.config.pending_timeout)?;
        if let Some(change_id) = record.change_id.as_deref() {
            match self.poll_change(change_id).await? {
                ChangeState::Done => {
                    return self.resolve_pending_terminal(&record, stale, now).await;
                }
                ChangeState::Failed => {
                    return self.resolve_pending_terminal(&record, stale, now).await;
                }
                ChangeState::InProgress if stale => {
                    self.abort_change(change_id).await?;
                    warn!(snap = %record.name, change_id, "aborted stale pending install; awaiting snapd rollback");
                }
                ChangeState::InProgress => {}
            }
            return Ok(());
        }

        let active_changes = self.active_changes(&record.name).await?;
        if active_changes.len() == 1 {
            record.change_id = active_changes.first().cloned();
            self.store.set(record.clone()).await?;
        }
        if !active_changes.is_empty() {
            if stale {
                for change_id in active_changes {
                    self.abort_change(&change_id).await?;
                }
                warn!(snap = %record.name, "aborted discovered stale install change; awaiting snapd rollback");
            }
            return Ok(());
        }

        self.resolve_pending_terminal(&record, stale, now).await
    }

    async fn resolve_pending_terminal(
        &self,
        record: &Expiration,
        stale: bool,
        now: chrono::DateTime<Utc>,
    ) -> Result<(), AppError> {
        if self.installed_status(&record.name).await? {
            if stale {
                self.remove_locked(&record.name, true).await?;
                self.store.remove(&record.name).await?;
                info!(snap = %record.name, "purged an install that completed after its pending deadline");
            } else {
                let promoted = self.promote_pending(&record.name, now).await?;
                info!(snap = %record.name, expires_at = %promoted.expires_at, "reconciled completed install");
            }
        } else {
            self.store.remove(&record.name).await?;
            info!(snap = %record.name, "removed state for an install that did not complete");
        }
        Ok(())
    }

    async fn resolve_failed_install(&self, name: &str) -> Result<(), AppError> {
        if self.installed_status(name).await? {
            let record = self.promote_pending(name, Utc::now()).await?;
            warn!(snap = %name, expires_at = %record.expires_at, "snap was installed despite a failed watch; retaining it as managed");
        } else {
            self.store.remove(name).await?;
        }
        Ok(())
    }

    async fn promote_pending(
        &self,
        name: &str,
        installed_at: chrono::DateTime<Utc>,
    ) -> Result<Expiration, AppError> {
        let record =
            self.store.get(name).await.ok_or_else(|| {
                AppError::Internal(format!("pending state for {name} disappeared"))
            })?;
        let installed = promoted_record(record, installed_at)?;
        self.store.set(installed.clone()).await?;
        Ok(installed)
    }

    async fn poll_change(&self, change_id: &str) -> Result<ChangeState, AppError> {
        validate_change_id(change_id)?;
        let output = self
            .raw_command_with_timeout(
                SNAP,
                vec!["watch".into(), change_id.into()],
                "poll snap change",
                CHANGE_POLL_TIMEOUT,
            )
            .await;
        match output {
            Err(AppError::CommandTimeout { .. }) => Ok(ChangeState::InProgress),
            Err(error) => Err(error),
            Ok(output) if output.status.success() => Ok(ChangeState::Done),
            Ok(_) => {
                self.refresh_readiness().await?;
                Ok(ChangeState::Failed)
            }
        }
    }

    async fn active_changes(&self, name: &str) -> Result<Vec<String>, AppError> {
        validation::snap_name(name)?;
        let output = self
            .raw_command(
                SNAP,
                vec!["changes".into(), name.into()],
                "list snap changes",
            )
            .await?;
        if output.status.success() {
            return parse_active_change_ids(&output.stdout);
        }
        let message = concise_output(&output.stderr);
        if message.contains("no changes found") {
            return Ok(Vec::new());
        }
        self.refresh_readiness().await?;
        Err(AppError::Command {
            operation: "list snap changes",
            message,
        })
    }

    async fn abort_change(&self, change_id: &str) -> Result<(), AppError> {
        validate_change_id(change_id)?;
        match self
            .run_command(
                SNAP,
                vec!["abort".into(), change_id.into()],
                "abort snap change",
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(error) => match self.poll_change(change_id).await? {
                ChangeState::Done | ChangeState::Failed => Ok(()),
                ChangeState::InProgress => Err(error),
            },
        }
    }

    async fn remove_locked(&self, name: &str, purge: bool) -> Result<(), AppError> {
        let mut args = vec!["remove".into()];
        if purge {
            args.push("--purge".into());
        }
        args.push(name.into());
        self.run_command(SNAP, args, "snap remove").await?;
        Ok(())
    }

    async fn installed_status(&self, name: &str) -> Result<bool, AppError> {
        let output = self
            .raw_command(SNAP, vec!["list".into(), name.into()], "snap list")
            .await?;
        if output.status.success() {
            return Ok(true);
        }
        let stderr = concise_output(&output.stderr);
        if stderr.contains("no matching snaps installed") || stderr.contains("not installed") {
            Ok(false)
        } else {
            Err(AppError::Command {
                operation: "snap list",
                message: stderr,
            })
        }
    }

    async fn ensure_managed(&self, name: &str) -> Result<(), AppError> {
        if self.store.get(name).await.is_some() {
            Ok(())
        } else {
            Err(AppError::NotFound(format!(
                "snap {name} is not managed by telesnap"
            )))
        }
    }

    async fn validate_snap_magic(&self, path: &Path) -> Result<(), AppError> {
        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(|error| AppError::Internal(format!("cannot open upload: {error}")))?;
        let mut magic = [0_u8; 4];
        file.read_exact(&mut magic)
            .await
            .map_err(|_| AppError::BadRequest("upload is not a SquashFS snap".to_owned()))?;
        if &magic != b"hsqs" {
            return Err(AppError::BadRequest(
                "upload is not a SquashFS snap".to_owned(),
            ));
        }
        Ok(())
    }

    async fn read_snap_metadata(&self, path: &Path) -> Result<SnapMetadata, AppError> {
        let output = self
            .run_command(
                UNSQUASHFS,
                vec![
                    "-cat".into(),
                    path.as_os_str().to_owned(),
                    "meta/snap.yaml".into(),
                ],
                "read snap metadata",
            )
            .await?;
        let metadata = String::from_utf8(output.stdout)
            .map_err(|_| AppError::BadRequest("snap metadata is not valid UTF-8".to_owned()))?;
        let name = top_level_yaml_scalar(&metadata, "name")
            .map_err(|_| AppError::BadRequest("snap metadata name is ambiguous".to_owned()))?
            .ok_or_else(|| AppError::BadRequest("snap metadata has no name".to_owned()))?;
        validation::snap_name(&name)?;
        let confinement = top_level_yaml_scalar(&metadata, "confinement").map_err(|_| {
            AppError::BadRequest("snap metadata confinement is ambiguous".to_owned())
        })?;
        let snap_type = top_level_yaml_scalar(&metadata, "type")
            .map_err(|_| AppError::BadRequest("snap metadata type is ambiguous".to_owned()))?;
        Ok(SnapMetadata {
            name,
            confinement,
            snap_type,
        })
    }

    async fn receive_upload(&self, mut body: Body) -> Result<UploadedFile, AppError> {
        let path = self
            .config
            .upload_dir
            .join(format!("{}.snap", Uuid::new_v4()));
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .map_err(|error| AppError::Internal(format!("cannot create upload: {error}")))?;
        let uploaded = UploadedFile(path);
        let receive = async move {
            let mut received = 0_u64;
            while let Some(frame) =
                std::future::poll_fn(|context| Pin::new(&mut body).poll_frame(context)).await
            {
                let frame = frame.map_err(|error| AppError::Upload(error.to_string()))?;
                let Ok(chunk) = frame.into_data() else {
                    continue;
                };
                received =
                    checked_upload_size(received, chunk.len(), self.config.max_upload_bytes)?;
                file.write_all(&chunk)
                    .await
                    .map_err(|error| AppError::Internal(format!("cannot write upload: {error}")))?;
            }
            if received == 0 {
                return Err(AppError::BadRequest("upload body is empty".to_owned()));
            }
            file.flush()
                .await
                .map_err(|error| AppError::Internal(format!("cannot flush upload: {error}")))?;
            info!(bytes = received, "received snap upload");
            Ok(uploaded)
        };
        timeout(self.config.upload_timeout, receive)
            .await
            .map_err(|_| AppError::UploadTimeout {
                seconds: self.config.upload_timeout.as_secs(),
            })?
    }

    async fn run_command(
        &self,
        program: &'static str,
        args: Vec<OsString>,
        operation: &'static str,
    ) -> Result<std::process::Output, AppError> {
        let output = self.raw_command(program, args, operation).await?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(AppError::Command {
                operation,
                message: concise_output(&output.stderr),
            })
        }
    }

    async fn run_command_with_timeout(
        &self,
        program: &'static str,
        args: Vec<OsString>,
        operation: &'static str,
        duration: Duration,
    ) -> Result<std::process::Output, AppError> {
        let output = self
            .raw_command_with_timeout(program, args, operation, duration)
            .await?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(AppError::Command {
                operation,
                message: concise_output(&output.stderr),
            })
        }
    }

    async fn check_readiness(&self) -> Result<(), AppError> {
        self.run_command_with_timeout(
            SNAP,
            vec!["wait".into(), "system".into(), "seed.loaded".into()],
            "snapd readiness check",
            READINESS_TIMEOUT,
        )
        .await
        .map(|_| ())
    }

    async fn raw_command(
        &self,
        program: &'static str,
        args: Vec<OsString>,
        operation: &'static str,
    ) -> Result<std::process::Output, AppError> {
        self.raw_command_with_timeout(program, args, operation, self.config.command_timeout)
            .await
    }

    async fn raw_command_with_timeout(
        &self,
        program: &'static str,
        args: Vec<OsString>,
        operation: &'static str,
        duration: Duration,
    ) -> Result<std::process::Output, AppError> {
        let mut command = Command::new(program);
        command
            .args(args)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| AppError::Command {
            operation,
            message: error.to_string(),
        })?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let collect = async move {
            let (stdout, stderr, status) =
                tokio::join!(read_bounded(stdout), read_bounded(stderr), child.wait());
            Ok::<_, std::io::Error>(std::process::Output {
                status: status?,
                stdout: stdout?,
                stderr: stderr?,
            })
        };
        timeout(duration, collect)
            .await
            .map_err(|_| AppError::CommandTimeout {
                operation,
                seconds: duration.as_secs(),
            })?
            .map_err(|error| AppError::Command {
                operation,
                message: error.to_string(),
            })
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ChangeState {
    InProgress,
    Done,
    Failed,
}

fn duration_delta(seconds: u64, message: &'static str) -> Result<TimeDelta, AppError> {
    i64::try_from(seconds)
        .ok()
        .and_then(TimeDelta::try_seconds)
        .ok_or_else(|| AppError::Internal(message.to_owned()))
}

fn checked_deadline(
    start: chrono::DateTime<Utc>,
    duration: TimeDelta,
    label: &'static str,
) -> Result<chrono::DateTime<Utc>, AppError> {
    start
        .checked_add_signed(duration)
        .ok_or_else(|| AppError::Internal(format!("{label} exceeds the supported date range")))
}

fn pending_lifetime_seconds(record: &Expiration) -> Result<u64, AppError> {
    if let Some(seconds) = record.requested_lifetime_seconds {
        return Ok(seconds);
    }
    u64::try_from((record.expires_at - record.installed_at).num_seconds())
        .ok()
        .filter(|seconds| *seconds > 0)
        .ok_or_else(|| {
            AppError::Internal(format!(
                "pending state for {} has no valid requested lifetime",
                record.name
            ))
        })
}

fn promoted_record(
    record: Expiration,
    installed_at: chrono::DateTime<Utc>,
) -> Result<Expiration, AppError> {
    let lifetime_seconds = pending_lifetime_seconds(&record)?;
    let lifetime = duration_delta(lifetime_seconds, "stored snap lifetime is too large")?;
    let expires_at = checked_deadline(installed_at, lifetime, "stored snap lifetime")?;
    Ok(Expiration {
        name: record.name,
        installed_at,
        expires_at,
        status: LifecycleStatus::Installed,
        pending_since: None,
        requested_lifetime_seconds: None,
        change_id: None,
    })
}

fn pending_is_stale(
    record: &Expiration,
    now: chrono::DateTime<Utc>,
    command_timeout: Duration,
) -> Result<bool, AppError> {
    let pending_since = record.pending_since.unwrap_or(record.installed_at);
    let timeout = duration_delta(command_timeout.as_secs(), "pending timeout is too large")?;
    Ok(now >= checked_deadline(pending_since, timeout, "pending timeout")?)
}

fn parse_change_id(output: &[u8]) -> Result<String, AppError> {
    let text = String::from_utf8_lossy(output);
    let change_id = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or_default()
        .to_owned();
    validate_change_id(&change_id)?;
    Ok(change_id)
}

fn validate_change_id(change_id: &str) -> Result<(), AppError> {
    if !change_id.is_empty()
        && change_id.len() <= 32
        && change_id.bytes().all(|byte| byte.is_ascii_digit())
    {
        Ok(())
    } else {
        Err(AppError::Command {
            operation: "parse snap change id",
            message: "snapd returned an invalid change id".to_owned(),
        })
    }
}

fn parse_active_change_ids(output: &[u8]) -> Result<Vec<String>, AppError> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let id = fields.next()?;
            let status = fields.next()?;
            if validate_change_id(id).is_err() || !line.contains("Install ") {
                return None;
            }
            Some(match status {
                "Do" | "Doing" | "Abort" | "Undo" | "Undoing" | "Wait" => Ok(Some(id.to_owned())),
                "Done" | "Undone" | "Error" | "Hold" => Ok(None),
                _ => Err(AppError::Command {
                    operation: "parse snap changes",
                    message: format!("snapd returned unknown change status {status}"),
                }),
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|ids| ids.into_iter().flatten().collect())
}

fn checked_upload_size(received: u64, chunk_size: usize, limit: u64) -> Result<u64, AppError> {
    let total = received
        .checked_add(chunk_size as u64)
        .ok_or(AppError::PayloadTooLarge { limit })?;
    if total > limit {
        Err(AppError::PayloadTooLarge { limit })
    } else {
        Ok(total)
    }
}

struct UploadedFile(PathBuf);

struct SnapMetadata {
    name: String,
    confinement: Option<String>,
    snap_type: Option<String>,
}

impl UploadedFile {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for UploadedFile {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!(path = %self.0.display(), %error, "failed to delete uploaded snap");
        }
    }
}

fn service_target(name: &str, service: Option<&str>) -> Result<String, AppError> {
    validation::snap_name(name)?;
    match service {
        Some(service) => {
            validation::service_name(service)?;
            Ok(format!("{name}.{service}"))
        }
        None => Ok(name.to_owned()),
    }
}

fn unquote_yaml_scalar(value: &str) -> String {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
            || (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
        {
            return value[1..value.len() - 1].to_owned();
        }
    }
    value.to_owned()
}

fn top_level_yaml_scalar(document: &str, key: &str) -> Result<Option<String>, ()> {
    let mut found = None;
    for line in document.lines() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some((candidate, value)) = line.split_once(':') else {
            continue;
        };
        if candidate.trim() != key {
            continue;
        }
        if found.is_some() {
            return Err(());
        }
        let value = value.trim();
        if value.is_empty() || value.starts_with(['|', '>']) {
            return Err(());
        }
        found = Some(unquote_yaml_scalar(value));
    }
    Ok(found)
}

fn is_reserved_snap_name(name: &str) -> bool {
    name == "snapd"
        || name == "bare"
        || name
            .strip_prefix("core")
            .is_some_and(|suffix| suffix.bytes().all(|byte| byte.is_ascii_digit()))
}

async fn read_bounded<R>(reader: R) -> Result<Vec<u8>, std::io::Error>
where
    R: AsyncRead + Unpin,
{
    const LIMIT: u64 = 2 * 1024 * 1024;
    let mut bytes = Vec::new();
    reader.take(LIMIT + 1).read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > LIMIT {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "command output exceeded 2 MiB",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use axum::body::Body;
    use chrono::{TimeDelta, Utc};
    use tempfile::{TempDir, tempdir};

    use crate::{
        config::Config,
        error::AppError,
        state::{Expiration, LifecycleStatus, StateStore},
    };

    use super::{
        SnapManager, checked_upload_size, parse_active_change_ids, parse_change_id,
        pending_is_stale, pending_lifetime_seconds, promoted_record, service_target,
        top_level_yaml_scalar, unquote_yaml_scalar,
    };

    async fn test_manager(max_upload_bytes: u64) -> (SnapManager, TempDir) {
        let directory = tempdir().unwrap();
        let upload_dir = directory.path().join("uploads");
        tokio::fs::create_dir(&upload_dir).await.unwrap();
        let config = Arc::new(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            api_token: "test-token-that-is-at-least-32-bytes".to_owned(),
            state_path: directory.path().join("state.json"),
            upload_dir,
            max_upload_bytes,
            max_lifetime: Duration::from_secs(86_400),
            command_timeout: Duration::from_secs(300),
            pending_timeout: Duration::from_secs(600),
            upload_timeout: Duration::from_secs(60),
        });
        let store = StateStore::load(config.state_path.clone()).await.unwrap();
        (SnapManager::new(config, store), directory)
    }

    #[test]
    fn builds_service_targets_without_shell_syntax() {
        assert_eq!(service_target("hello", None).unwrap(), "hello");
        assert_eq!(
            service_target("hello", Some("worker")).unwrap(),
            "hello.worker"
        );
        assert!(service_target("hello", Some("../../unit")).is_err());
    }

    #[test]
    fn removes_simple_yaml_quotes() {
        assert_eq!(unquote_yaml_scalar("'hello-world'"), "hello-world");
        assert_eq!(unquote_yaml_scalar("\"hello-world\""), "hello-world");
        assert_eq!(unquote_yaml_scalar("hello-world"), "hello-world");
    }

    #[test]
    fn parses_only_unambiguous_top_level_metadata() {
        let document = "name: 'hello-world'\nconfinement: strict\napps:\n  name: nested\n";
        assert_eq!(
            top_level_yaml_scalar(document, "name").unwrap().as_deref(),
            Some("hello-world")
        );
        assert_eq!(
            top_level_yaml_scalar(document, "confinement")
                .unwrap()
                .as_deref(),
            Some("strict")
        );
        assert!(top_level_yaml_scalar("name: |\n  hello", "name").is_err());
        assert!(top_level_yaml_scalar("name: first\nname: second", "name").is_err());
    }

    #[test]
    fn validates_and_discovers_snap_change_ids() {
        assert_eq!(parse_change_id(b"notice\n42\n").unwrap(), "42");
        assert!(parse_change_id(b"--help\n").is_err());

        let changes = b"ID Status Spawn Ready Summary\n41 Done now now Install snap\n42 Doing now - Install snap\n43 Hold now now Install snap\n44 Wait now - Install snap\n45 Doing now - Remove snap\n";
        assert_eq!(parse_active_change_ids(changes).unwrap(), ["42", "44"]);
    }

    #[test]
    fn pending_state_has_a_bounded_window_and_preserves_requested_lifetime() {
        let started = Utc::now();
        let record = Expiration {
            name: "example".to_owned(),
            installed_at: started,
            expires_at: started + TimeDelta::minutes(30),
            status: LifecycleStatus::Pending,
            pending_since: Some(started),
            requested_lifetime_seconds: Some(1_800),
            change_id: Some("42".to_owned()),
        };

        assert_eq!(pending_lifetime_seconds(&record).unwrap(), 1_800);
        assert!(
            !pending_is_stale(
                &record,
                started + TimeDelta::seconds(299),
                Duration::from_secs(300)
            )
            .unwrap()
        );
        assert!(
            pending_is_stale(
                &record,
                started + TimeDelta::seconds(300),
                Duration::from_secs(300)
            )
            .unwrap()
        );

        let confirmed_at = started + TimeDelta::minutes(10);
        let installed = promoted_record(record, confirmed_at).unwrap();
        assert_eq!(installed.installed_at, confirmed_at);
        assert_eq!(installed.expires_at, confirmed_at + TimeDelta::minutes(30));
        assert_eq!(installed.status, LifecycleStatus::Installed);
        assert!(installed.change_id.is_none());
    }

    #[test]
    fn legacy_pending_state_recovers_its_lifetime() {
        let started = Utc::now();
        let record = Expiration {
            name: "example".to_owned(),
            installed_at: started,
            expires_at: started + TimeDelta::minutes(15),
            status: LifecycleStatus::Pending,
            pending_since: None,
            requested_lifetime_seconds: None,
            change_id: None,
        };

        assert_eq!(pending_lifetime_seconds(&record).unwrap(), 900);
    }

    #[test]
    fn upload_limit_accepts_files_larger_than_one_gibibyte() {
        let one_gibibyte = 1024_u64 * 1024 * 1024;
        let limit = 2 * one_gibibyte;
        assert_eq!(
            checked_upload_size(1, one_gibibyte as usize, limit).unwrap(),
            one_gibibyte + 1
        );
        assert!(checked_upload_size(limit, 1, limit).is_err());
    }

    #[tokio::test]
    async fn streams_uploads_to_temporary_files_and_removes_them_on_drop() {
        let (manager, _directory) = test_manager(1024).await;
        let bytes = b"hsqs-test-snap";
        let uploaded = manager
            .receive_upload(Body::from(bytes.as_slice()))
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(uploaded.path()).await.unwrap(), bytes);
        let path = uploaded.path().to_owned();
        drop(uploaded);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn removes_partial_files_when_streamed_limit_is_exceeded() {
        let (manager, _directory) = test_manager(4).await;
        let error = match manager.receive_upload(Body::from("12345")).await {
            Ok(_) => panic!("oversized upload was accepted"),
            Err(error) => error,
        };
        assert!(matches!(error, AppError::PayloadTooLarge { limit: 4 }));
        assert_eq!(
            std::fs::read_dir(&manager.config.upload_dir)
                .unwrap()
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn rejects_a_third_concurrent_install_before_reading_its_body() {
        let (manager, _directory) = test_manager(1024).await;
        let _first = manager.uploads.try_acquire().unwrap();
        let _second = manager.uploads.try_acquire().unwrap();
        let error = manager
            .install(Body::from("not-read"), None, 60)
            .await
            .unwrap_err();
        assert!(matches!(error, AppError::InstallCapacity));
    }

    #[tokio::test]
    async fn rejects_an_oversized_content_length_before_reading_the_body() {
        let (manager, _directory) = test_manager(4).await;
        let error = manager
            .install(Body::from("not-read"), Some(5), 60)
            .await
            .unwrap_err();

        assert!(matches!(error, AppError::PayloadTooLarge { limit: 4 }));
        assert_eq!(
            std::fs::read_dir(&manager.config.upload_dir)
                .unwrap()
                .count(),
            0
        );
    }
}
