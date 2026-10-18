//! Bot progress is independent of Slack read cursors. Atomic snapshots provide
//! restart recovery, not a transaction with PostgreSQL (a crash can duplicate an insert).
use super::{RecoveryConfig, lock, now};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
};

const VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 16 * 1024 * 1024;
pub(super) const MAX_CHANNELS: usize = 4096;
const MAX_THREADS: usize = 4096;
const MAX_SEEN: usize = 32768;
pub(super) const OVERLAP: i64 = 120;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Channel {
    pub covered_until: i64,
    pub threads: BTreeMap<String, i64>,
    #[serde(skip)]
    pub recovering: bool,
    #[serde(skip)]
    pub failed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Seen {
    channel: String,
    ts: String,
    processed_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    team: String,
    user: String,
    channels: BTreeMap<String, Channel>,
    seen: VecDeque<Seen>,
}

#[derive(Debug)]
pub(super) struct Store {
    path: PathBuf,
    data: Mutex<Snapshot>,
    save_lock: tokio::sync::Mutex<()>,
    // Keep an OS lock for this workspace/account for the entire run.
    _file_lock: File,
    window: i64,
}

pub(super) fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|c| c.is_ascii_alphanumeric())
}

fn valid_ts(ts: &str) -> bool {
    ts.len() <= 32 && crate::harvest::message_timestamp(ts).is_ok()
}

impl Store {
    pub fn open(config: &RecoveryConfig, team: &str, user: &str) -> anyhow::Result<Self> {
        ensure!(
            valid_id(team) && valid_id(user),
            "invalid Slack account identity"
        );
        let dir = Path::new(&config.state_dir);
        std::fs::create_dir_all(dir).context("create recovery state directory")?;
        let path = dir.join(format!("{team}-{user}.json"));
        let file_lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("lock"))
            .context("open recovery state lock")?;
        file_lock
            .try_lock()
            .context("recovery state is already in use by another process")?;
        let data = match File::open(&path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(MAX_STATE_BYTES + 1).read_to_end(&mut bytes)?;
                ensure!(
                    bytes.len() as u64 <= MAX_STATE_BYTES,
                    "recovery state exceeds 16 MiB"
                );
                let data: Snapshot = serde_json::from_slice(&bytes)
                    .context("invalid recovery state; refusing to reset progress")?;
                ensure!(
                    data.version == VERSION && data.team == team && data.user == user,
                    "recovery state identity/version mismatch"
                );
                ensure!(
                    data.channels.len() <= MAX_CHANNELS && data.seen.len() <= MAX_SEEN,
                    "recovery state entry limit exceeded"
                );
                let mut threads = 0;
                for (id, channel) in &data.channels {
                    ensure!(
                        valid_id(id)
                            && channel.covered_until >= 0
                            && channel.covered_until <= now() + 300,
                        "invalid channel checkpoint"
                    );
                    threads += channel.threads.len();
                    ensure!(
                        channel.threads.keys().all(|ts| valid_ts(ts)),
                        "invalid thread timestamp"
                    );
                }
                ensure!(threads <= MAX_THREADS, "too many tracked threads");
                ensure!(
                    data.seen
                        .iter()
                        .all(|s| valid_id(&s.channel) && valid_ts(&s.ts)),
                    "invalid duplicate detection state"
                );
                data
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Snapshot {
                version: VERSION,
                team: team.into(),
                user: user.into(),
                channels: BTreeMap::new(),
                seen: VecDeque::new(),
            },
            Err(error) => return Err(error).context("read recovery state"),
        };
        Ok(Self {
            path,
            data: Mutex::new(data),
            save_lock: tokio::sync::Mutex::new(()),
            _file_lock: file_lock,
            window: config.window_seconds as i64,
        })
    }

    pub fn begin_connection(&self, channels: &[String], at: i64) -> anyhow::Result<()> {
        let mut data = lock(&self.data);
        data.channels.retain(|id, _| channels.contains(id));
        for id in channels {
            ensure!(valid_id(id), "invalid channel id");
            let existed = data.channels.contains_key(id);
            ensure!(
                existed || data.channels.len() < MAX_CHANNELS,
                "too many joined channels"
            );
            let channel = data.channels.entry(id.clone()).or_insert_with(|| Channel {
                covered_until: at,
                threads: BTreeMap::new(),
                recovering: false,
                failed: false,
            });
            channel.recovering = existed;
            channel.failed = false;
            if channel.covered_until < at - self.window {
                tracing::warn!(
                    channel = id,
                    "Recovery window exceeded; older gap will not be recovered"
                );
                channel.covered_until = at - self.window;
            }
        }
        Self::prune(&mut data, at, self.window);
        Ok(())
    }

    pub fn add_channel(&self, id: &str, at: i64) -> anyhow::Result<()> {
        let mut data = lock(&self.data);
        ensure!(
            valid_id(id) && (data.channels.contains_key(id) || data.channels.len() < MAX_CHANNELS),
            "channel limit exceeded"
        );
        data.channels.entry(id.into()).or_insert_with(|| Channel {
            covered_until: at,
            threads: BTreeMap::new(),
            recovering: false,
            failed: false,
        });
        Ok(())
    }

    pub fn recovery_channels(&self) -> Vec<(String, Channel)> {
        lock(&self.data)
            .channels
            .iter()
            .filter(|(_, c)| c.recovering)
            .map(|(id, c)| (id.clone(), c.clone()))
            .collect()
    }

    pub fn threads(&self, id: &str) -> Vec<String> {
        lock(&self.data)
            .channels
            .get(id)
            .map(|c| c.threads.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn retry_channel(&self, id: &str) {
        if let Some(channel) = lock(&self.data).channels.get_mut(id) {
            channel.failed = false;
        }
    }

    pub fn forget_thread(&self, id: &str, ts: &str) {
        if let Some(channel) = lock(&self.data).channels.get_mut(id) {
            channel.threads.remove(ts);
        }
    }

    pub fn processed(&self, channel: &str, ts: &str, thread: Option<&str>, at: i64) {
        let mut data = lock(&self.data);
        data.seen.push_back(Seen {
            channel: channel.into(),
            ts: ts.into(),
            processed_at: at,
        });
        if let Some(parent) = thread.filter(|ts| valid_ts(ts))
            && let Some(channel) = data.channels.get_mut(channel)
        {
            channel.threads.insert(parent.into(), at);
        }
        Self::prune(&mut data, at, self.window);
    }

    pub fn seen(&self, channel: &str, ts: &str) -> bool {
        lock(&self.data)
            .seen
            .iter()
            .any(|s| s.channel == channel && s.ts == ts)
    }

    pub fn remember_thread(&self, id: &str, parent: &str) {
        let mut data = lock(&self.data);
        if valid_ts(parent)
            && let Some(channel) = data.channels.get_mut(id)
        {
            channel.threads.insert(parent.into(), now());
        }
        Self::prune(&mut data, now(), self.window);
    }

    pub fn failed(&self, channel: &str) {
        if let Some(channel) = lock(&self.data).channels.get_mut(channel) {
            channel.failed = true;
        }
    }

    pub fn finish_recovery(&self, id: &str, through: i64) {
        if let Some(channel) = lock(&self.data).channels.get_mut(id)
            && !channel.failed
        {
            channel.covered_until = channel.covered_until.max(through);
            channel.recovering = false;
        }
    }

    pub fn checkpoint(&self, through: i64) {
        let mut data = lock(&self.data);
        for channel in data
            .channels
            .values_mut()
            .filter(|c| !c.recovering && !c.failed)
        {
            channel.covered_until = channel.covered_until.max(through);
        }
        Self::prune(&mut data, now(), self.window);
    }

    fn prune(data: &mut Snapshot, at: i64, window: i64) {
        data.seen
            .retain(|s| s.processed_at >= at - window - OVERLAP);
        while data.seen.len() > MAX_SEEN {
            data.seen.pop_front();
        }
        for channel in data.channels.values_mut() {
            channel.threads.retain(|_, time| *time >= at - window);
        }
        let count: usize = data.channels.values().map(|c| c.threads.len()).sum();
        if count > MAX_THREADS {
            let mut threads: Vec<_> = data
                .channels
                .iter()
                .flat_map(|(id, c)| {
                    c.threads
                        .iter()
                        .map(move |(ts, at)| (*at, id.clone(), ts.clone()))
                })
                .collect();
            threads.sort_unstable();
            for (_, id, ts) in threads.into_iter().take(count - MAX_THREADS) {
                if let Some(channel) = data.channels.get_mut(&id) {
                    channel.threads.remove(&ts);
                }
            }
        }
    }

    pub async fn save(&self) -> anyhow::Result<()> {
        let _guard = self.save_lock.lock().await;
        let snapshot = lock(&self.data).clone();
        let bytes = serde_json::to_vec(&snapshot)?;
        ensure!(
            bytes.len() as u64 <= MAX_STATE_BYTES,
            "recovery state exceeds 16 MiB"
        );
        let temp = self.path.with_extension("json.tmp");
        use tokio::io::AsyncWriteExt;
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temp).await?;
        file.write_all(&bytes).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(temp, &self.path)
            .await
            .context("persist recovery checkpoint")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> RecoveryConfig {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        RecoveryConfig {
            state_dir: format!(
                "/tmp/sjmb-state-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn recovery_freezes_progress_and_persists_dedup_and_threads() {
        let config = config();
        let at = now();
        let store = Store::open(&config, "T1", "U1").unwrap();
        store.begin_connection(&["C1".into()], at - 60).unwrap();
        store.processed("C1", "1234.000001", Some("1230.000001"), at);
        store.save().await.unwrap();
        assert!(Store::open(&config, "T1", "U1").is_err());
        drop(store);
        let store = Store::open(&config, "T1", "U1").unwrap();
        store.begin_connection(&["C1".into()], at).unwrap();
        store.checkpoint(at + 10);
        assert_eq!(store.recovery_channels()[0].1.covered_until, at - 60);
        assert!(store.seen("C1", "1234.000001"));
        assert_eq!(store.recovery_channels()[0].1.threads.len(), 1);
        store.finish_recovery("C1", at);
        store.checkpoint(at + 10);
        assert_eq!(lock(&store.data).channels["C1"].covered_until, at + 10);
        store.failed("C1");
        store.checkpoint(at + 20);
        assert_eq!(lock(&store.data).channels["C1"].covered_until, at + 10);
        drop(store);
        std::fs::remove_dir_all(&config.state_dir).unwrap();
    }

    #[test]
    fn corrupt_state_is_not_silently_reset() {
        let config = config();
        let store = Store::open(&config, "T1", "U1").unwrap();
        std::fs::write(&store.path, "{").unwrap();
        drop(store);
        assert!(Store::open(&config, "T1", "U1").is_err());
        assert!(Store::open(&config, "../T1", "U1").is_err());
        std::fs::remove_dir_all(&config.state_dir).unwrap();
    }
}
