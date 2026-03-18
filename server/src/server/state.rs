use std::{collections::HashMap, sync::Arc};

use eyre::Result;
use tokio::sync::{watch, Mutex};

use crate::{
    config::Config,
    store::{GitStore, MAIN_BRANCH},
    sync_state::{SyncStateStore, VapidKeys},
};

use super::push::{PushDelivery, ReqwestPushDelivery};

/// Shared server state. All fields behind cheap-to-clone handles.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Mutex<GitStore>>,
    pub config: Arc<Config>,
    pub(super) vapid_keys: Arc<VapidKeys>,
    pub(super) push_state: Arc<Mutex<SyncStateStore>>,
    pub(super) push_delivery: Arc<dyn PushDelivery>,
    /// Per-branch notification channels. Includes an entry for `MAIN_BRANCH`
    /// so sync-local clients can be notified when main advances.
    branch_notifications: Arc<HashMap<String, watch::Sender<u64>>>,
}

impl AppState {
    pub fn new(config: Config, store: GitStore) -> Result<Self> {
        Self::new_with_push_delivery(config, store, Arc::new(ReqwestPushDelivery::new()))
    }

    pub(super) fn new_with_push_delivery(
        config: Config,
        store: GitStore,
        push_delivery: Arc<dyn PushDelivery>,
    ) -> Result<Self> {
        let sync_state_store = SyncStateStore::for_git_store(&config.git_store);
        let vapid_keys = sync_state_store.ensure_vapid_keys()?;
        let mut branch_notifications: HashMap<String, watch::Sender<u64>> = config
            .clients
            .iter()
            .map(|client| {
                let (tx, _rx) = watch::channel(0_u64);
                (client.id.clone(), tx)
            })
            .collect();

        let (main_tx, _) = watch::channel(0_u64);
        branch_notifications.insert(MAIN_BRANCH.to_string(), main_tx);

        Ok(Self {
            store: Arc::new(Mutex::new(store)),
            config: Arc::new(config),
            vapid_keys: Arc::new(vapid_keys),
            push_state: Arc::new(Mutex::new(sync_state_store)),
            push_delivery,
            branch_notifications: Arc::new(branch_notifications),
        })
    }

    pub(crate) fn subscribe_branch_notifications(
        &self,
        branch_id: &str,
    ) -> Option<watch::Receiver<u64>> {
        self.branch_notifications
            .get(branch_id)
            .map(watch::Sender::subscribe)
    }

    pub(crate) fn notify_branches<'a>(&self, branch_ids: impl IntoIterator<Item = &'a String>) {
        for branch_id in branch_ids {
            if let Some(tx) = self.branch_notifications.get(branch_id) {
                tx.send_modify(|version| *version += 1);
            }
        }
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState").finish_non_exhaustive()
    }
}
