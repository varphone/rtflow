use super::*;

impl ProxyService {
    pub(super) fn cache_path(&self, path: &str) -> Option<std::path::PathBuf> {
        let directory = self.cache_dir.as_ref()?;
        let name = path.trim_matches('/').replace('/', "_");
        Some(directory.join(format!("{name}.sdp")))
    }

    pub(super) fn load_cached(&self, path: &str) -> Option<rtflow_media::MediaDescription> {
        let contents = std::fs::read(self.cache_path(path)?).ok()?;
        rtflow_rtsp::sdp::parse(&contents).ok()
    }

    pub(super) fn store_cached(&self, path: &str, description: &rtflow_media::MediaDescription) {
        let Some(path) = self.cache_path(path) else {
            return;
        };
        let Some(directory) = path.parent() else {
            return;
        };
        if std::fs::create_dir_all(directory).is_err() {
            return;
        }
        let text = rtflow_rtsp::sdp::encode(description);
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        if let Ok(mut file) = options.open(&path) {
            use std::io::Write as _;
            let _ = file.write_all(text.as_bytes());
        }
    }
}
