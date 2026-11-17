use super::*;

impl ProxyService {
    /// Restart a proxy now, or defer until its current subscribers leave.
    /// Returns `true` when the restart was deferred.
    pub fn restart(self: &Arc<Self>, path: &str) -> rtflow_core::error::Result<bool> {
        let (mut upstream, options, grace, always, refresh, mount) = {
            let entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = entries
                .get(path)
                .ok_or_else(|| rtflow_core::error::Error::NotFound(path.into()))?;
            let entry = entry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if entry.state.restart_pending {
                return Ok(true);
            }
            (
                entry.upstream.clone(),
                entry.options.clone(),
                entry.grace,
                entry.always,
                entry.refresh,
                entry.mount.clone(),
            )
        };
        upstream.reset_state();
        let subscribers = mount
            .as_ref()
            .map(|mount| {
                let snapshot = mount.snapshot();
                snapshot.subscriber_count.max(snapshot.broadcast_receivers)
            })
            .unwrap_or(0);
        if subscribers == 0 {
            self.remove(path);
            self.ensure(
                Arc::from(path),
                upstream,
                options,
                Some(grace),
                always,
                refresh,
            )?;
            return Ok(false);
        }
        {
            let entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entry) = entries.get(path) {
                entry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .state
                    .restart_pending = true;
            }
        }
        let service = self.clone();
        let path: Arc<str> = Arc::from(path);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if service.cancel.is_cancelled() {
                    return;
                }
                let entry = {
                    let entries = service
                        .entries
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    entries.get(&path).cloned()
                };
                let Some(entry) = entry else {
                    return;
                };
                let (mount, cancel) = {
                    let entry = entry
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    (entry.mount.clone(), entry.cancel.clone())
                };
                if cancel.is_cancelled() {
                    return;
                }
                let Some(mount) = mount else {
                    return;
                };
                let snapshot = mount.snapshot();
                if snapshot.subscriber_count.max(snapshot.broadcast_receivers) == 0 {
                    service.remove(&path);
                    let _ = service.ensure(path, upstream, options, Some(grace), always, refresh);
                    return;
                }
            }
        });
        Ok(true)
    }
}
