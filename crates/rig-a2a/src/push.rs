//! Push notifications: the webhook configs clients set, and the checks on them.

use std::sync::Arc;

use a2a::{A2AError, TaskPushNotificationConfig};
use a2a_server::{HttpPushSender, HttpPushSenderConfig, InMemoryPushConfigStore, PushConfigStore};
use async_trait::async_trait;

/// What the server does with push notifications when they are on.
///
/// Set through [`A2aServer::push_notifications`](crate::A2aServer::push_notifications)
/// and its companions. Webhook URLs come from clients, so every URL is
/// checked before it is saved, and the SDK checks it again before each POST.
#[derive(Clone)]
pub(crate) struct PushSettings {
    /// Whether the server takes push configs at all. Off by default.
    pub(crate) enabled: bool,
    /// Where configs are kept. `None` means the in-memory store.
    pub(crate) store: Option<Arc<dyn PushConfigStore>>,
    /// Hosts a webhook URL may name. `None` allows every host.
    pub(crate) allowed_hosts: Option<Vec<String>>,
    /// Accept `http://` URLs and turn off the SDK's address guard.
    pub(crate) allow_insecure: bool,
}

impl PushSettings {
    pub(crate) fn new() -> Self {
        Self {
            enabled: false,
            store: None,
            allowed_hosts: None,
            allow_insecure: false,
        }
    }

    /// The store the handler uses, wrapped so every saved URL is checked.
    pub(crate) fn checked_store(&self) -> CheckedPushStore {
        let inner = self
            .store
            .clone()
            .unwrap_or_else(|| Arc::new(InMemoryPushConfigStore::new()));
        CheckedPushStore {
            inner,
            allowed_hosts: self.allowed_hosts.clone(),
            allow_insecure: self.allow_insecure,
        }
    }

    /// The SDK sender. With `allow_insecure` off it keeps the SDK's guard:
    /// private, loopback and link-local targets are refused, including
    /// after DNS and across redirects.
    pub(crate) fn sender(&self) -> HttpPushSender {
        HttpPushSender::new(Some(HttpPushSenderConfig {
            validate_urls: !self.allow_insecure,
            ..HttpPushSenderConfig::default()
        }))
    }
}

/// A push config store that refuses a webhook URL before saving it.
///
/// The checks: the URL parses; its scheme is `https` (or `http` when
/// insecure URLs are allowed); and its host is on the allowlist, if one is
/// set. Reads, lists and deletes pass straight through.
pub(crate) struct CheckedPushStore {
    inner: Arc<dyn PushConfigStore>,
    allowed_hosts: Option<Vec<String>>,
    allow_insecure: bool,
}

impl CheckedPushStore {
    fn check_url(&self, url: &str) -> Result<(), A2AError> {
        let parsed = reqwest::Url::parse(url)
            .map_err(|_| A2AError::invalid_params("push notification URL is not a valid URL"))?;
        let scheme_ok = match parsed.scheme() {
            "https" => true,
            "http" => self.allow_insecure,
            _ => false,
        };
        if !scheme_ok {
            return Err(A2AError::invalid_params(
                "push notification URL must use https",
            ));
        }
        if let Some(allowed) = &self.allowed_hosts {
            let host = parsed.host_str().unwrap_or_default();
            if !allowed.iter().any(|a| a.eq_ignore_ascii_case(host)) {
                return Err(A2AError::invalid_params(
                    "push notification host is not on this server's allowlist",
                ));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl PushConfigStore for CheckedPushStore {
    async fn save(
        &self,
        config: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.check_url(&config.url)?;
        self.inner.save(config).await
    }

    async fn get(
        &self,
        task_id: &str,
        config_id: &str,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.inner.get(task_id, config_id).await
    }

    async fn list(&self, task_id: &str) -> Result<Vec<TaskPushNotificationConfig>, A2AError> {
        self.inner.list(task_id).await
    }

    async fn delete(&self, task_id: &str, config_id: &str) -> Result<(), A2AError> {
        self.inner.delete(task_id, config_id).await
    }

    async fn delete_all(&self, task_id: &str) -> Result<(), A2AError> {
        self.inner.delete_all(task_id).await
    }
}
