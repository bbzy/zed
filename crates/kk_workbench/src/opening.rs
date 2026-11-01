use crate::remote::{Client, Request, ResourceId};
use anyhow::Result;
use smol::channel::{Receiver, Sender, bounded};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};

#[derive(Debug, Default)]
pub(crate) struct DiffOpenings(Mutex<HashMap<ResourceId, Weak<OpeningActivity>>>);

#[derive(Debug)]
struct OpeningActivity {
    owner: ResourceId,
    sender: Mutex<Option<Sender<()>>>,
    completed: Receiver<()>,
}

impl OpeningActivity {
    fn finish(&self) {
        self.sender
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }
}

pub struct DiffOpening {
    local: Option<Arc<OpeningActivity>>,
    remote: Option<(Client, ResourceId)>,
}

impl DiffOpening {
    pub(crate) fn remote(client: Client, id: ResourceId) -> Self {
        Self {
            local: None,
            remote: Some((client, id)),
        }
    }
}

impl Drop for DiffOpening {
    fn drop(&mut self) {
        if let Some(activity) = &self.local {
            activity.finish();
        }

        if let Some((client, id)) = self.remote.take() {
            drop(client.request(Request::EndDiffOpening { id }));
        }
    }
}

impl DiffOpenings {
    pub fn has_pending(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .filter_map(Weak::upgrade)
            .any(|activity| !activity.completed.is_closed())
    }

    pub fn begin(&self, owner: ResourceId) -> DiffOpening {
        let (sender, completed) = bounded(1);
        let activity = Arc::new(OpeningActivity {
            owner,
            sender: Mutex::new(Some(sender)),
            completed,
        });
        let mut activities = self.0.lock().unwrap_or_else(|error| error.into_inner());
        activities.retain(|_, activity| activity.strong_count() > 0);
        activities.insert(uuid::Uuid::new_v4().as_u128(), Arc::downgrade(&activity));
        DiffOpening {
            local: Some(activity),
            remote: None,
        }
    }

    pub fn cancel(&self, owner: ResourceId) {
        let mut activities = self.0.lock().unwrap_or_else(|error| error.into_inner());
        activities.retain(|_, activity| {
            let Some(activity) = activity.upgrade() else {
                return false;
            };
            if activity.owner == owner {
                activity.finish();
                return false;
            }
            true
        });
    }

    pub async fn wait(&self) -> Result<()> {
        loop {
            let pending = {
                let mut activities = self.0.lock().unwrap_or_else(|error| error.into_inner());
                activities.retain(|_, activity| {
                    activity
                        .upgrade()
                        .is_some_and(|activity| !activity.completed.is_closed())
                });
                activities
                    .values()
                    .filter_map(Weak::upgrade)
                    .map(|activity| activity.completed.clone())
                    .collect::<Vec<_>>()
            };
            if pending.is_empty() {
                return Ok(());
            }

            for completed in pending {
                // The sender is never used for data; closing it broadcasts completion.
                if completed.recv().await.is_ok() {
                    anyhow::bail!("Unexpected diff-opening notification");
                }
            }
        }
    }
}
