use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::error::AppError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Expiration {
    pub name: String,
    pub installed_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    #[serde(default)]
    pub status: LifecycleStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_since: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_lifetime_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleStatus {
    Pending,
    #[default]
    Installed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StateFile {
    #[serde(default)]
    snaps: BTreeMap<String, Expiration>,
}

#[derive(Debug, Clone)]
pub struct StateStore {
    path: PathBuf,
    inner: Arc<RwLock<StateFile>>,
}

impl StateStore {
    pub async fn load(path: PathBuf) -> Result<Self, AppError> {
        let state = match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                AppError::Internal(format!("cannot parse {}: {error}", path.display()))
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => StateFile::default(),
            Err(error) => {
                return Err(AppError::Internal(format!(
                    "cannot read {}: {error}",
                    path.display()
                )));
            }
        };
        Ok(Self {
            path,
            inner: Arc::new(RwLock::new(state)),
        })
    }

    pub async fn list(&self) -> Vec<Expiration> {
        self.inner.read().await.snaps.values().cloned().collect()
    }

    pub async fn get(&self, name: &str) -> Option<Expiration> {
        self.inner.read().await.snaps.get(name).cloned()
    }

    pub async fn set(&self, record: Expiration) -> Result<(), AppError> {
        let mut state = self.inner.write().await;
        let mut next = state.clone();
        next.snaps.insert(record.name.clone(), record);
        self.persist(&next).await?;
        *state = next;
        Ok(())
    }

    pub async fn remove(&self, name: &str) -> Result<(), AppError> {
        let mut state = self.inner.write().await;
        let mut next = state.clone();
        next.snaps.remove(name);
        self.persist(&next).await?;
        *state = next;
        Ok(())
    }

    async fn persist(&self, state: &StateFile) -> Result<(), AppError> {
        let bytes = serde_json::to_vec_pretty(state)
            .map_err(|error| AppError::Internal(format!("cannot serialize state: {error}")))?;
        let temporary = self.path.with_extension(format!("tmp-{}", Uuid::new_v4()));
        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .await
            .map_err(|error| {
                AppError::Internal(format!("cannot create {}: {error}", temporary.display()))
            })?;
        file.write_all(&bytes).await.map_err(|error| {
            AppError::Internal(format!("cannot write {}: {error}", temporary.display()))
        })?;
        file.sync_all().await.map_err(|error| {
            AppError::Internal(format!("cannot sync {}: {error}", temporary.display()))
        })?;
        drop(file);
        tokio::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(|error| {
                AppError::Internal(format!(
                    "cannot set permissions on {}: {error}",
                    temporary.display()
                ))
            })?;
        if let Err(error) = tokio::fs::rename(&temporary, &self.path).await {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(AppError::Internal(format!(
                "cannot replace {}: {error}",
                self.path.display()
            )));
        }
        if let Some(parent) = self.path.parent() {
            let parent = parent.to_owned();
            tokio::task::spawn_blocking(move || std::fs::File::open(parent)?.sync_all())
                .await
                .map_err(|error| AppError::Internal(format!("state sync task failed: {error}")))?
                .map_err(|error| {
                    AppError::Internal(format!("cannot sync state directory: {error}"))
                })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, Utc};
    use tempfile::tempdir;

    use super::{Expiration, LifecycleStatus, StateStore};

    #[tokio::test]
    async fn persists_and_reloads_expirations() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.json");
        let store = StateStore::load(path.clone()).await.unwrap();
        let now = Utc::now();
        store
            .set(Expiration {
                name: "example".to_owned(),
                installed_at: now,
                expires_at: now + TimeDelta::minutes(5),
                status: LifecycleStatus::Installed,
                pending_since: None,
                requested_lifetime_seconds: None,
                change_id: None,
            })
            .await
            .unwrap();

        let reloaded = StateStore::load(path).await.unwrap();
        assert_eq!(reloaded.list().await.len(), 1);
        reloaded.remove("example").await.unwrap();
        assert!(reloaded.list().await.is_empty());
    }

    #[tokio::test]
    async fn loads_pending_state_from_the_previous_schema() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.json");
        tokio::fs::write(
            &path,
            br#"{
                "snaps": {
                    "example": {
                        "name": "example",
                        "installed_at": "2026-01-01T00:00:00Z",
                        "expires_at": "2026-01-01T00:15:00Z",
                        "status": "pending"
                    }
                }
            }"#,
        )
        .await
        .unwrap();

        let store = StateStore::load(path).await.unwrap();
        let record = store.get("example").await.unwrap();
        assert_eq!(record.status, LifecycleStatus::Pending);
        assert!(record.pending_since.is_none());
        assert!(record.requested_lifetime_seconds.is_none());
        assert!(record.change_id.is_none());
    }
}
