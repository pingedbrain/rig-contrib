//! Push notifications: the webhook configs clients set, and the checks on them.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::Duration;

use a2a::{A2AError, ListTasksRequest, ListTasksResponse, Task, TaskPushNotificationConfig};
use a2a_server::{
    HttpPushSender, HttpPushSenderConfig, InMemoryPushConfigStore, PushConfigStore, TaskStore,
};
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
    /// Drop the `https` requirement, the literal-range refusal and the SDK's
    /// address guard, and use system proxies. For local tests only.
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
    /// the caps are enforced whatever store is underneath. `tasks` is the
    /// task store, read to tell live tasks from purged ones.
    pub(crate) fn checked_store(&self, tasks: Arc<dyn TaskStore>) -> CheckedPushStore {
        let inner = self
            .store
            .clone()
            .unwrap_or_else(|| Arc::new(InMemoryPushConfigStore::new()));
        CheckedPushStore::new(
            inner,
            tasks,
            self.allowed_hosts.clone(),
            self.allow_unguarded,
        )
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

/// The task store as the handler and the push wrapper share it.
pub(crate) struct SharedTasks(pub(crate) Arc<dyn TaskStore>);

#[async_trait]
impl TaskStore for SharedTasks {
    async fn create(&self, task: Task) -> Result<u64, A2AError> {
        self.0.create(task).await
    }

    async fn update(&self, task: Task) -> Result<u64, A2AError> {
        self.0.update(task).await
    }

    async fn get(&self, task_id: &str) -> Result<Option<Task>, A2AError> {
        self.0.get(task_id).await
    }

    async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        self.0.list(req).await
    }

    async fn begin_cancel(&self, task_id: &str) -> Result<Task, A2AError> {
        self.0.begin_cancel(task_id).await
    }
}

/// How many configs each task holds, and in all, as this store saved them.
#[derive(Default)]
struct Held {
    per_task: HashMap<String, usize>,
    total: usize,
}

/// A push config store that checks each config before saving it and caps
/// how many it holds.
///
/// The checks: the URL parses; its scheme is `https` (or `http` when
/// unguarded URLs are allowed); its host is on the allowlist, if one is set;
/// and a literal IP host is not in a range the SDK guard misses (see
/// [`literal_range_refused_v4`] and [`literal_range_refused_v6`]).
///
/// Caps: [`MAX_PUSH_CONFIGS_PER_TASK`] per task, and [`MAX_PUSH_CONFIGS_TOTAL`]
/// overall. The total counts configs this store has saved since it started.
/// When the total is reached, configs of tasks the task store no longer
/// holds (purged by `terminal_task_limit`) are dropped first, so the cap
/// counts live tasks. Reads pass straight through.
pub(crate) struct CheckedPushStore {
    inner: Arc<dyn PushConfigStore>,
    tasks: Arc<dyn TaskStore>,
    allowed_hosts: Option<Vec<String>>,
    allow_unguarded: bool,
    max_per_task: usize,
    max_total: usize,
    /// Configs held, serialized so the cap check and the write are one step.
    held: Mutex<Held>,
}

fn too_many(what: &str, max: usize) -> A2AError {
    A2AError::invalid_params(format!(
        "too many push notification configs {what} (max {max})"
    ))
}

impl CheckedPushStore {
    fn new(
        inner: Arc<dyn PushConfigStore>,
        tasks: Arc<dyn TaskStore>,
        allowed_hosts: Option<Vec<String>>,
        allow_unguarded: bool,
    ) -> Self {
        Self {
            inner,
            tasks,
            allowed_hosts,
            allow_unguarded,
            max_per_task: MAX_PUSH_CONFIGS_PER_TASK,
            max_total: MAX_PUSH_CONFIGS_TOTAL,
            held: Mutex::new(Held::default()),
        }
    }

    /// The same store with other caps, for tests.
    #[cfg(test)]
    pub(crate) fn with_limits(mut self, per_task: usize, total: usize) -> Self {
        self.max_per_task = per_task;
        self.max_total = total;
        self
    }

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

    /// Drop the configs of tasks the task store no longer holds.
    async fn drop_finished(&self, held: &mut Held) -> Result<(), A2AError> {
        let mut gone = Vec::new();
        for task_id in held.per_task.keys() {
            if matches!(self.tasks.get(task_id).await, Ok(None)) {
                gone.push(task_id.clone());
            }
        }
        for task_id in gone {
            self.inner.delete_all(&task_id).await?;
            if let Some(n) = held.per_task.remove(&task_id) {
                held.total -= n;
            }
        }
        Ok(())
    }

    /// Account for `n` configs of `task_id` removed from the store.
    fn forget(held: &mut Held, task_id: &str, n: usize) {
        if let Some(count) = held.per_task.get_mut(task_id) {
            let removed = n.min(*count);
            *count -= removed;
            held.total -= removed;
            if *count == 0 {
                held.per_task.remove(task_id);
            }
        }
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
    let nat64 = s[0] == 0x0064 && s[1] == 0xff9b && s[2..6].iter().all(|&seg| seg == 0);
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
        let mut held = self.held.lock().await;
        // Re-saving an existing config replaces it and does not count again.
        let replacing = match &config.id {
            Some(id) => self.inner.get(&config.task_id, id).await.is_ok(),
            None => false,
        };
        if !replacing {
            if held.per_task.get(&config.task_id).copied().unwrap_or(0) >= self.max_per_task {
                return Err(too_many("for this task", self.max_per_task));
            }
            if held.total >= self.max_total {
                self.drop_finished(&mut held).await?;
            }
            if held.total >= self.max_total {
                return Err(too_many("on this server", self.max_total));
            }
        }
        let task_id = config.task_id.clone();
        let saved = self.inner.save(config).await?;
        if !replacing {
            *held.per_task.entry(task_id).or_insert(0) += 1;
            held.total += 1;
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
        let mut held = self.held.lock().await;
        let existed = self.inner.get(task_id, config_id).await.is_ok();
        self.inner.delete(task_id, config_id).await?;
        if existed {
            Self::forget(&mut held, task_id, 1);
        }
        Ok(())
    }

    async fn delete_all(&self, task_id: &str) -> Result<(), A2AError> {
        let mut held = self.held.lock().await;
        self.inner.delete_all(task_id).await?;
        if let Some(n) = held.per_task.remove(task_id) {
            held.total -= n;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use a2a::{TaskState, TaskStatus};
    use a2a_server::InMemoryTaskStore;

    use super::*;

    fn config(task_id: &str) -> TaskPushNotificationConfig {
        TaskPushNotificationConfig {
            url: "https://hooks.example.com/a".to_owned(),
            id: None,
            task_id: task_id.to_owned(),
            token: None,
            authentication: None,
            tenant: None,
        }
    }

    fn task(id: &str, state: TaskState) -> Task {
        Task {
            id: id.to_owned(),
            context_id: "c".to_owned(),
            status: TaskStatus {
                state,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        }
    }

    fn wrapper(tasks: Arc<InMemoryTaskStore>) -> CheckedPushStore {
        CheckedPushStore::new(
            Arc::new(InMemoryPushConfigStore::new()),
            Arc::new(SharedTasks(tasks)),
            None,
            false,
        )
    }

    /// The total cap refuses a config past the limit while the tasks are live.
    #[tokio::test]
    async fn the_total_cap_refuses_past_the_limit() {
        let tasks = Arc::new(InMemoryTaskStore::new());
        let store = wrapper(Arc::clone(&tasks)).with_limits(10, 5);
        for n in 0..5 {
            let id = format!("task-{n}");
            tasks.create(task(&id, TaskState::Working)).await.unwrap();
            store.save(config(&id)).await.unwrap();
        }
        let refused = store.save(config("task-5")).await;
        assert!(refused.is_err(), "live tasks fill the total cap");
    }

    /// Configs of purged tasks are released when the total is reached, so
    /// saves keep being accepted as many tasks finish.
    #[tokio::test]
    async fn configs_of_purged_tasks_stop_counting() {
        let tasks = Arc::new(InMemoryTaskStore::new().with_terminal_task_limit(3));
        let store = wrapper(Arc::clone(&tasks)).with_limits(10, 5);
        tasks
            .create(task("long-running", TaskState::Working))
            .await
            .unwrap();
        store.save(config("long-running")).await.unwrap();
        for n in 0..40 {
            let id = format!("task-{n}");
            tasks.create(task(&id, TaskState::Working)).await.unwrap();
            store.save(config(&id)).await.unwrap();
            tasks.update(task(&id, TaskState::Completed)).await.unwrap();
        }
        // The long-running task keeps its config; the purged ones were dropped.
        let held = store.list("long-running").await.unwrap();
        assert_eq!(held.len(), 1);
        assert!(
            tasks.get("task-0").await.unwrap().is_none(),
            "task-0 was purged"
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
