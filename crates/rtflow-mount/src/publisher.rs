use super::*;

// ---------------------------------------------------------------------------
// LocalPublisher
// ---------------------------------------------------------------------------

pub struct LocalPublisher {
    pub(crate) inner: Arc<Mount>,
    pub(crate) active: bool,
}

impl LocalPublisher {
    /// Number of unique client sessions currently playing this mount.
    pub fn subscriber_count(&self) -> usize {
        self.inner.active_sessions.load(Ordering::Relaxed)
    }

    /// True after the mount is unregistered or its server begins shutdown.
    pub fn is_closed(&self) -> bool {
        self.inner.cancel.is_cancelled()
    }

    /// Subscribe to local-source keyframe requests (track id).
    pub fn keyframe_requests(&self) -> tokio::sync::broadcast::Receiver<u8> {
        self.inner.keyframe_requests.subscribe()
    }

    /// Request a keyframe on a track (used by RTCP feedback actions).
    pub fn request_keyframe(&self, track_id: u8) {
        self.inner.request_keyframe(track_id);
    }

    /// Push a frame without track validation (for protocol ingest like RTMP
    /// where the description may not yet be populated).
    pub fn push(&self, frame: AccessUnit) -> Result<PushResult> {
        if frame.data.len() > self.inner.max_queued_bytes_per_session {
            self.inner.stats.add_dropped(1);
            return Ok(PushResult::Dropped);
        }
        if self.inner.frames.receiver_count() == 0 {
            self.inner.stats.add_dropped(1);
            // No subscribers: still keep the startup keyframe cache fresh
            // (plus the per-track bootstrap for the RTP-Info rtptime base).
            // Otherwise the snapshot a new PLAY sees is frozen at the last
            // disconnect — replaying it jumps straight from stale frames
            // to the live edge, which stalls VLC for seconds.
            self.inner.auto_inject_fmtp(&frame);
            self.inner.update_bootstrap(&frame);
            #[cfg(feature = "server")]
            if let Ok(mut cache) = self.inner.startup_cache.try_lock() {
                cache.push(std::sync::Arc::new(frame));
            }
            return Ok(PushResult::DroppedNoSubscribers);
        }
        match self.inner.publish_cached(frame) {
            Ok(_) => {
                self.inner.stats.add_accepted(1);
                Ok(PushResult::Accepted)
            }
            Err(_) => {
                self.inner.stats.add_dropped(1);
                Ok(PushResult::Dropped)
            }
        }
    }

    pub fn try_push(&self, frame: AccessUnit) -> Result<PushResult> {
        // O(1) codec validation via the fixed lookup table; no per-frame
        // MediaDescription clone + linear track search.
        if frame.data.len() > self.inner.max_queued_bytes_per_session {
            self.inner.stats.add_dropped(1);
            return Ok(PushResult::Dropped);
        }
        let expected = self
            .inner
            .codec_by_track
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)[frame.track_id as usize];
        let Some(expected) = expected else {
            return Err(Error::Media(format!(
                "mount has no track {}",
                frame.track_id
            )));
        };
        if expected != frame.codec {
            return Err(Error::Media(format!(
                "track {} expects {:?}, got {:?}",
                frame.track_id, expected, frame.codec
            )));
        }
        if self.inner.frames.receiver_count() == 0 {
            self.inner.stats.add_dropped(1);
            // No subscribers: still keep the startup keyframe cache fresh
            // (plus the per-track bootstrap for the RTP-Info rtptime base).
            // Otherwise the snapshot a new PLAY sees is frozen at the last
            // disconnect — replaying it jumps straight from stale frames
            // to the live edge, which stalls VLC for seconds.
            self.inner.auto_inject_fmtp(&frame);
            self.inner.update_bootstrap(&frame);
            #[cfg(feature = "server")]
            if let Ok(mut cache) = self.inner.startup_cache.try_lock() {
                cache.push(std::sync::Arc::new(frame));
            }
            return Ok(PushResult::DroppedNoSubscribers);
        }
        match self.inner.publish_cached(frame) {
            Ok(_) => {
                self.inner.stats.add_accepted(1);
                Ok(PushResult::Accepted)
            }
            Err(_) => {
                self.inner.stats.add_dropped(1);
                Ok(PushResult::Dropped)
            }
        }
    }

    pub async fn close(mut self) -> Result<()> {
        self.release();
        Ok(())
    }

    fn release(&mut self) {
        if self.active {
            self.inner.publishers.fetch_sub(1, Ordering::AcqRel);
            self.active = false;
        }
    }
}

impl Drop for LocalPublisher {
    fn drop(&mut self) {
        self.release();
    }
}
