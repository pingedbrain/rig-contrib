//! Push notifications: the webhook configs clients set, and the checks on them.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::Duration;

use a2a::{A2AError, TaskPushNotificationConfig};
use a2a_server::{HttpPushSender, HttpPushSenderConfig, InMemoryPushConfigStore, PushConfigStore};
use async_trait::async_trait;
use tokio::sync::Mutex;

/// Most push configs one task may hold. A config costs one POST per update.
pub(crate) const MAX_PUSH_CONFIGS_PER_TASK: usize = 10;
/// Most push configs the server holds across all tasks.
pub(crate) const MAX_PUSH_CONFIGS_TOTAL: usize = 10_000;
/// How long one webhook POST may take before it counts as failed.
pub(crate) const PUSH_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// Drop the `https` requirement and the SDK's address guard, and use
    /// system proxies. For local tests only.
    pub(crate) allow_unguarded: bool,
}

impl PushSettings {
    pub(crate) fn new() -> Self {
        Self {
            enabled: false,
            store: None,
            allowed_hosts: None,
            allow_unguarded: false,
        }
    }

    /// The store the handler uses, wrapped so every saved URL is checked and
    /// the caps are enforced whatever store is underneath.
    pub(crate) fn checked_store(&self) -> CheckedPushStore {
        let inner = self
            .store
            .clone()
            .unwrap_or_else(|| Arc::new(InMemoryPushConfigStore::new()));
        CheckedPushStore {
            inner,
            allowed_hosts: self.allowed_hosts.clone(),
            allow_unguarded: self.allow_unguarded,
            total: Mutex::new(0),
        }
    }

    /// The SDK sender, with a short timeout. Unless `allow_unguarded` is set,
    /// it keeps the SDK's address guard, which also covers DNS and redirects.
    pub(crate) fn sender(&self) -> HttpPushSender {
        HttpPushSender::new(Some(HttpPushSenderConfig {
            timeout: PUSH_TIMEOUT,
            validate_urls: !self.allow_unguarded,
            ..HttpPushSenderConfig::default()
        }))
    }
}

/// A push config store that checks each config before saving it and caps
/// how many it holds.
///
/// The checks: the URL parses; its scheme is `https` (or `http` when
/// unguarded URLs are allowed); its host is on the allowlist, if one is set;
/// and a literal IP host is not in a range the SDK guard misses (see
/// [`literal_range_refused`]). Caps: [`MAX_PUSH_CONFIGS_PER_TASK`] per task
/// and [`MAX_PUSH_CONFIGS_TOTAL`] overall. The total counts configs this
/// store has saved, so it starts at zero over a store that already holds some.
/// Reads pass straight through.
pub(crate) struct CheckedPushStore {
    inner: Arc<dyn PushConfigStore>,
    allowed_hosts: Option<Vec<String>>,
    allow_unguarded: bool,
    /// Configs held, serialized so the cap check and the write are one step.
    total: Mutex<usize>,
}

fn too_many(what: &str, max: usize) -> A2AError {
    A2AError::invalid_params(format!(
        "too many push notification configs {what} (max {max})"
    ))
}

impl CheckedPushStore {
    fn check_url(&self, url: &str) -> Result<(), A2AError> {
        let parsed = reqwest::Url::parse(url)
            .map_err(|_| A2AError::invalid_params("push notification URL is not a valid URL"))?;
        let scheme_ok = match parsed.scheme() {
            "https" => true,
            "http" => self.allow_unguarded,
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
        // IPv6 host strings keep their brackets.
        let host = parsed
            .host_str()
            .unwrap_or_default()
            .trim_start_matches('[')
            .trim_end_matches(']');
        let refused = match host.parse::<IpAddr>() {
            Ok(IpAddr::V4(ip)) => literal_range_refused_v4(ip),
            Ok(IpAddr::V6(ip)) => literal_range_refused_v6(ip),
            Err(_) => false,
        };
        if refused && !self.allow_unguarded {
            return Err(A2AError::invalid_params(
                "push notification URL targets a blocked address range",
            ));
        }
        Ok(())
    }
}

/// IPv4 literals outside the SDK guard's ranges that this crate refuses:
/// carrier-grade NAT, 100.64.0.0/10.
fn literal_range_refused_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (o[1] & 0xc0) == 64
}

/// IPv6 literals outside the SDK guard's ranges that this crate refuses:
/// NAT64 64:ff9b::/96, 6to4 2002::/16, and IPv4-compatible ::a.b.c.d/96.
fn literal_range_refused_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    let nat64 =
        s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0;
    let six_to_four = s[0] == 0x2002;
    let v4_compatible = s[0..6].iter().all(|&seg| seg == 0);
    nat64 || six_to_four || v4_compatible
}

#[async_trait]
impl PushConfigStore for CheckedPushStore {
    async fn save(
        &self,
        config: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.check_url(&config.url)?;
        let mut total = self.total.lock().await;
        // Re-saving an existing config replaces it and does not count again.
        let replacing = match &config.id {
            Some(id) => self.inner.get(&config.task_id, id).await.is_ok(),
            None => false,
        };
        if !replacing {
            if self.inner.list(&config.task_id).await?.len() >= MAX_PUSH_CONFIGS_PER_TASK {
                return Err(too_many("for this task", MAX_PUSH_CONFIGS_PER_TASK));
            }
            if *total >= MAX_PUSH_CONFIGS_TOTAL {
                return Err(too_many("on this server", MAX_PUSH_CONFIGS_TOTAL));
            }
        }
        let saved = self.inner.save(config).await?;
        if !replacing {
            *total += 1;
        }
        Ok(saved)
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
        let mut total = self.total.lock().await;
        let existed = self.inner.get(task_id, config_id).await.is_ok();
        self.inner.delete(task_id, config_id).await?;
        if existed {
            *total = total.saturating_sub(1);
        }
        Ok(())
    }

    async fn delete_all(&self, task_id: &str) -> Result<(), A2AError> {
        let mut total = self.total.lock().await;
        let held = self.inner.list(task_id).await?.len();
        self.inner.delete_all(task_id).await?;
        *total = total.saturating_sub(held);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(task_id: &str, id: Option<&str>) -> TaskPushNotificationConfig {
        TaskPushNotificationConfig {
            url: "https://hooks.example.com/a".to_owned(),
            id: id.map(str::to_owned),
            task_id: task_id.to_owned(),
            token: None,
            authentication: None,
            tenant: None,
        }
    }

    /// The total cap is enforced by the wrapper, whatever the store holds,
    /// so it is checked here against the wrapper directly.
    #[tokio::test]
    async fn the_total_cap_holds_across_tasks() {
        let store = CheckedPushStore {
            inner: Arc::new(InMemoryPushConfigStore::new()),
            allowed_hosts: None,
            allow_unguarded: false,
            total: Mutex::new(0),
        };
        for n in 0..MAX_PUSH_CONFIGS_TOTAL {
            store
                .save(config(&format!("task-{n}"), None))
                .await
                .unwrap();
        }
        let refused = store.save(config("one-more", None)).await;
        assert!(
            refused.is_err(),
            "the total cap refuses a config past the limit"
        );
    }

    #[test]
    fn literal_ranges_outside_the_sdk_guard_are_refused() {
        assert!(literal_range_refused_v4(Ipv4Addr::new(100, 64, 0, 1)));
        assert!(!literal_range_refused_v4(Ipv4Addr::new(100, 128, 0, 1)));
        assert!(literal_range_refused_v6(
            "64:ff9b::808:808".parse().unwrap()
        ));
        assert!(literal_range_refused_v6(
            "2002:c000:204::1".parse().unwrap()
        ));
        assert!(literal_range_refused_v6("::1.2.3.4".parse().unwrap()));
        assert!(!literal_range_refused_v6(
            "2606:4700::1111".parse().unwrap()
        ));
    }
}
