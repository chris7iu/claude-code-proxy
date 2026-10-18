//! Manager-owned credential maintenance, never model generation. Request
//! cancellation releases only its waiter. The stable lock is acquired BEFORE
//! the bounded network phase; a complete validated rotation is then persisted
//! outside that timer, with no cancellation point in the filesystem phase.
use super::{
    auth::{Credential, Lock, Store, bounded_json, now, update_tokens},
    config::Config,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{sync::watch, task::JoinHandle};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccessError {
    Storage(&'static str),
    Lock(&'static str),
    Missing,
    LoginRequired,
    ClientConfiguration,
    Network,
    NetworkTimeout,
    InvalidResponse,
    PendingPersistence,
    ProfileChanged,
    UncertainRotation,
    Cancelled,
    Closed,
}
impl AccessError {
    pub fn message(self) -> &'static str {
        match self {
            Self::Storage(_) => "Antigravity credential storage is invalid or unreadable",
            Self::Lock(_) => "Antigravity credential lock is unavailable or timed out",
            Self::Missing => "Antigravity authentication missing; run antigravity auth login",
            Self::LoginRequired => "Antigravity login required",
            Self::ClientConfiguration => "configure CCP_ANTIGRAVITY_CLIENT_ID before refreshing",
            Self::Network => {
                "Antigravity refresh failed; its outcome may be unknown, no obsolete-token retry attempted"
            }
            Self::NetworkTimeout => {
                "Antigravity refresh network deadline exceeded; no obsolete-token retry attempted"
            }
            Self::InvalidResponse => {
                "Antigravity refresh response invalid; no obsolete-token retry attempted"
            }
            Self::PendingPersistence => {
                "Rotated Antigravity credentials await persistence; repair local storage and retry without starting a new login"
            }
            Self::ProfileChanged => {
                "Antigravity profile or epoch changed; obsolete credentials were not saved"
            }
            Self::UncertainRotation => {
                "Antigravity refresh outcome is unknown; explicit login required instead of retrying the old refresh token"
            }
            Self::Cancelled => "Antigravity credential waiter cancelled before refresh began",
            Self::Closed => "Antigravity credential manager is shutting down",
        }
    }
}
impl std::fmt::Display for AccessError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message())
    }
}
impl std::error::Error for AccessError {}
type AccessResult = std::result::Result<Credential, AccessError>;

#[derive(Clone)]
struct Scope {
    epoch: String,
    profile_id: String,
    refresh_token: Option<String>,
}
impl Scope {
    fn matches(&self, epoch: &str, credential: &Credential) -> bool {
        // Incidental expiry/project/access metadata is not a new login or a
        // replacement for the known-obsolete refresh token. A changed epoch,
        // profile, or committed refresh token invalidates this recovery scope.
        self.epoch == epoch
            && self.profile_id == credential.profile_id
            && self.refresh_token == credential.refresh_token
    }
}
#[derive(Clone)]
struct Pending {
    scope: Scope,
    credential: Credential,
}
struct Flight {
    reply: watch::Receiver<Option<AccessResult>>,
    waiters: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}
#[derive(Default)]
struct State {
    flight: Option<Flight>,
    pending: Option<Pending>,
    uncertain: Option<Scope>,
    closed: bool,
}
struct Waiter(Arc<AtomicUsize>);
impl Drop for Waiter {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) struct RefreshManager {
    store: Store,
    client: reqwest::Client,
    token_url: url::Url,
    client_id: Option<String>,
    client_secret: Option<String>,
    lock_timeout: Duration,
    network_timeout: Duration,
    state: Mutex<State>,
}
impl RefreshManager {
    pub fn new(store: Store, client: reqwest::Client, config: &Config) -> Arc<Self> {
        Arc::new(Self {
            store,
            client,
            token_url: config.token_url.clone(),
            client_id: config.client_id.clone(),
            client_secret: config.client_secret.clone(),
            lock_timeout: Duration::from_secs(10),
            network_timeout: Duration::from_secs(30),
            state: Mutex::new(State::default()),
        })
    }
    #[cfg(test)]
    pub fn with_timeouts(mut self: Arc<Self>, lock: Duration, network: Duration) -> Arc<Self> {
        let manager = Arc::get_mut(&mut self).expect("unused test manager");
        manager.lock_timeout = lock;
        manager.network_timeout = network;
        self
    }

    pub async fn access(self: &Arc<Self>) -> AccessResult {
        let (mut reply, _waiter) = {
            let mut state = self.state.lock().map_err(|_| AccessError::Closed)?;
            if state.closed {
                return Err(AccessError::Closed);
            }
            // A reply is published only AFTER all filesystem work is complete.
            // Reap completed work; never reuse its credential across a new login.
            if state
                .flight
                .as_ref()
                .is_some_and(|flight| flight.reply.borrow().is_some())
            {
                state.flight.take();
            }
            if let Some(flight) = state.flight.as_ref() {
                flight.waiters.fetch_add(1, Ordering::AcqRel);
                (flight.reply.clone(), Waiter(flight.waiters.clone()))
            } else {
                let waiters = Arc::new(AtomicUsize::new(1));
                let (sender, reply) = watch::channel(None);
                let manager = self.clone();
                let subscribers = waiters.clone();
                let task = tokio::spawn(async move {
                    let result = manager.transaction(&subscribers).await;
                    let _ = sender.send(Some(result));
                });
                state.flight = Some(Flight {
                    reply: reply.clone(),
                    waiters: waiters.clone(),
                    task,
                });
                (reply, Waiter(waiters))
            }
        };
        loop {
            let completed = reply.borrow().clone();
            if let Some(result) = completed {
                return result;
            }
            reply.changed().await.map_err(|_| AccessError::Closed)?;
        }
    }

    fn clear_recovery(&self) -> std::result::Result<(), AccessError> {
        let mut state = self.state.lock().map_err(|_| AccessError::Closed)?;
        state.pending = None;
        state.uncertain = None;
        Ok(())
    }

    /// Called with the stable OS lock held. A pending replacement is never
    /// overwritten with, or retried against, the obsolete refresh credential.
    fn recover(&self, lock: &mut Lock) -> std::result::Result<Option<Credential>, AccessError> {
        let (pending, uncertain) = {
            let state = self.state.lock().map_err(|_| AccessError::Closed)?;
            (state.pending.clone(), state.uncertain.clone())
        };
        let epoch = lock.epoch().map_err(|error| {
            if pending.is_some() {
                AccessError::PendingPersistence
            } else {
                AccessError::Storage(error)
            }
        })?;
        let mut stored = self.store.load().map_err(|error| {
            if pending.is_some() {
                AccessError::PendingPersistence
            } else {
                AccessError::Storage(error)
            }
        })?;
        if let Some(pending) = pending {
            if stored
                .as_ref()
                .is_some_and(|credential| pending.scope.matches(&epoch, credential))
            {
                stored = Some(self.persist(lock, pending)?);
            } else {
                // Logout/new login/another completed refresh owns the new state.
                // Missing storage is not permission to resurrect the old profile.
                self.clear_recovery()?;
            }
        } else if let Some(scope) = uncertain {
            if stored
                .as_ref()
                .is_some_and(|credential| scope.matches(&epoch, credential))
            {
                return Err(AccessError::UncertainRotation);
            }
            self.clear_recovery()?;
        }
        Ok(stored)
    }

    /// No await/timer occurs after a validated replacement is retained. The
    /// filesystem calls are synchronous and are NOT claimed to be cancellable.
    fn persist(&self, lock: &mut Lock, pending: Pending) -> AccessResult {
        {
            let mut state = self.state.lock().map_err(|_| AccessError::Closed)?;
            state.pending = Some(pending.clone());
        }
        let epoch = lock.epoch().map_err(|_| AccessError::PendingPersistence)?;
        let stored = self
            .store
            .load()
            .map_err(|_| AccessError::PendingPersistence)?;
        if !stored
            .as_ref()
            .is_some_and(|credential| pending.scope.matches(&epoch, credential))
        {
            self.clear_recovery()?;
            return Err(AccessError::ProfileChanged);
        }
        self.store
            .save(lock, &pending.credential)
            .map_err(|_| AccessError::PendingPersistence)?;
        self.clear_recovery()?;
        Ok(pending.credential)
    }

    async fn transaction(&self, waiters: &AtomicUsize) -> AccessResult {
        // Lock acquisition has its own bound and precedes the network side effect.
        let mut lock = self
            .store
            .lock(self.lock_timeout)
            .await
            .map_err(AccessError::Lock)?;
        let credential = self.recover(&mut lock)?.ok_or(AccessError::Missing)?;
        if credential.expires_at > now().saturating_add(60) {
            return Ok(credential);
        }
        if waiters.load(Ordering::Acquire) == 0 {
            return Err(AccessError::Cancelled);
        }
        let refresh = credential
            .refresh_token
            .as_deref()
            .ok_or(AccessError::LoginRequired)?;
        let client_id = self
            .client_id
            .as_deref()
            .ok_or(AccessError::ClientConfiguration)?;
        let scope = Scope {
            epoch: lock.epoch().map_err(AccessError::Storage)?,
            profile_id: credential.profile_id.clone(),
            refresh_token: credential.refresh_token.clone(),
        };
        {
            let mut state = self.state.lock().map_err(|_| AccessError::Closed)?;
            state.uncertain = Some(scope.clone());
        }
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", client_id),
        ];
        if let Some(secret) = self.client_secret.as_deref() {
            form.push(("client_secret", secret));
        }
        let replacement = tokio::time::timeout(self.network_timeout, async {
            let response = self
                .client
                .post(self.token_url.clone())
                .timeout(self.network_timeout)
                .form(&form)
                .send()
                .await
                .map_err(|_| AccessError::Network)?;
            let token = bounded_json(response)
                .await
                .map_err(|_| AccessError::InvalidResponse)?;
            let mut replacement = credential.clone();
            update_tokens(&mut replacement, &token).map_err(|_| AccessError::InvalidResponse)?;
            Ok::<_, AccessError>(replacement)
        })
        .await
        .map_err(|_| AccessError::NetworkTimeout)??;
        // The manager's network deadline ended above. A disconnected waiter or
        // elapsed network timer cannot discard the now validated replacement.
        self.persist(
            &mut lock,
            Pending {
                scope,
                credential: replacement,
            },
        )
    }

    /// Quiesce new waiters, join owned maintenance, and retry ONLY a pending local
    /// write. No refresh or inference is initiated during graceful shutdown.
    pub async fn shutdown(&self) -> std::result::Result<(), AccessError> {
        let reply = {
            let mut state = self.state.lock().map_err(|_| AccessError::Closed)?;
            state.closed = true;
            state.flight.as_ref().map(|flight| flight.reply.clone())
        };
        // Keep the task handle owned by the manager if a shutdown waiter itself
        // is cancelled. Only reap after the transaction publishes completion.
        if let Some(mut reply) = reply {
            while reply.borrow().is_none() {
                if reply.changed().await.is_err() {
                    break;
                }
            }
        }
        let flight = self
            .state
            .lock()
            .map_err(|_| AccessError::Closed)?
            .flight
            .take();
        if let Some(flight) = flight {
            flight.task.await.map_err(|_| AccessError::Closed)?;
        }
        let recovery = {
            let state = self.state.lock().map_err(|_| AccessError::Closed)?;
            state.pending.is_some() || state.uncertain.is_some()
        };
        if recovery {
            let mut lock = self
                .store
                .lock(self.lock_timeout)
                .await
                .map_err(AccessError::Lock)?;
            self.recover(&mut lock)?;
        }
        Ok(())
    }
}
