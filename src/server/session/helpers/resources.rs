use super::*;

#[derive(Default)]
pub(crate) struct ConnectionResources {
    pub(crate) senders: Vec<tokio::task::JoinHandle<()>>,
    pub(crate) recording_mounts: Vec<Arc<Mount>>,
    pub(crate) claims: Vec<Arc<Mount>>,
    /// Publisher mounts created by this session's ANNOUNCE (dynamic paths).
    pub(crate) dynamic_mounts: Vec<crate::mount::MountHandle>,
    pub(crate) vod: Option<VodPlayback>,
    /// Mounts with an active PLAY in this session (deduplicated), for the
    /// unique-client counter.
    pub(crate) played_mounts: Vec<Arc<Mount>>,
}
impl ConnectionResources {
    pub(crate) fn abort_senders(&mut self) {
        for task in self.senders.drain(..) {
            task.abort();
        }
    }

    pub(crate) fn stop_vod(&mut self) -> Option<std::time::Duration> {
        self.vod.take().map(VodPlayback::stop)
    }

    /// Register each distinct mount as having one active play session.
    /// Idempotent across repeated PLAY requests.
    pub(crate) fn register_play<'a>(&mut self, mounts: impl Iterator<Item = &'a Arc<Mount>>) {
        for mount in mounts {
            if !self
                .played_mounts
                .iter()
                .any(|seen| Arc::ptr_eq(seen, mount))
            {
                mount.active_sessions.fetch_add(1, Ordering::Relaxed);
                self.played_mounts.push(mount.clone());
            }
        }
    }

    pub(crate) fn cleanup(&mut self) {
        self.abort_senders();
        self.stop_vod();
        release_recording_mounts(&mut self.recording_mounts);
        for mount in self.played_mounts.drain(..) {
            mount.active_sessions.fetch_sub(1, Ordering::Relaxed);
        }
        // Always release publisher claims when the session ends: the flag
        // guards concurrent take-over while a session is alive, it must not
        // outlive the connection (otherwise a Publisher mount with
        // allow_replace_after_disconnect=false is locked forever).
        release_remote_publishers(&mut self.claims, true);
    }
}
impl Drop for ConnectionResources {
    fn drop(&mut self) {
        self.cleanup();
    }
}

pub(crate) fn claim_remote_publishers(candidates: Vec<Arc<Mount>>) -> Result<Vec<Arc<Mount>>> {
    let mut claims: Vec<Arc<Mount>> = Vec::new();
    for mount in candidates {
        if !matches!(mount.config, MountConfig::Publisher { .. })
            || claims.iter().any(|current| Arc::ptr_eq(current, &mount))
        {
            continue;
        }
        // A non-RTSP publisher (RTMP/SRT via MountHandle::publisher) already
        // holds the mount: refuse to claim on top of it.
        if mount.publishers.load(Ordering::Acquire) > 0 {
            release_remote_publishers(&mut claims, true);
            return Err(Error::Conflict(
                "publisher is already active on this mount".into(),
            ));
        }
        if mount
            .publisher_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            release_remote_publishers(&mut claims, true);
            return Err(Error::Conflict("publisher is already claimed".into()));
        }
        claims.push(mount);
    }
    Ok(claims)
}

pub(crate) fn release_remote_publishers(mounts: &mut Vec<Arc<Mount>>, force: bool) {
    for mount in mounts.drain(..) {
        let replace = matches!(
            mount.config,
            MountConfig::Publisher {
                allow_replace_after_disconnect: true,
                ..
            }
        );
        if force || replace {
            mount.publisher_claimed.store(false, Ordering::Release);
        }
    }
}

pub(crate) fn release_recording_mounts(mounts: &mut Vec<Arc<Mount>>) {
    for mount in mounts.drain(..) {
        mount.publishers.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) fn live_sender_input(
    mount: &Arc<Mount>,
    track_id: u8,
    sequence: Arc<AtomicU16>,
) -> SenderInput {
    let (receiver, startup) = mount.subscribe_with_startup(track_id);
    SenderInput {
        receiver,
        startup,
        sequence,
        crypto: crate::server::tasks::MediaCrypto::default(),
    }
}
