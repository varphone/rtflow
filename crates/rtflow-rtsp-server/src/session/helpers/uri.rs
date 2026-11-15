use super::*;

pub(crate) fn track_for_uri(
    description: &MediaDescription,
    uri: &str,
) -> Result<rtflow_media::TrackDescription> {
    let requested = url::Url::parse(uri).ok();
    description
        .tracks
        .iter()
        .find(|track| {
            if let Ok(control) = url::Url::parse(&track.control) {
                return requested.as_ref() == Some(&control);
            }
            let requested_path = requested.as_ref().map(url::Url::path).unwrap_or(uri);
            let control = track.control.trim_start_matches('/');
            requested_path == track.control.as_ref()
                || requested_path.ends_with(&format!("/{control}"))
        })
        .cloned()
        .ok_or_else(|| Error::Rtsp(format!("no track matches SETUP URI {uri}")))
}

pub(crate) async fn registered_mount_for_uri(
    state: &State,
    uri: &str,
) -> Option<(String, Arc<Mount>)> {
    let requested = uri_path(uri);
    state
        .mounts
        .find_longest_prefix(&requested)
        .map(|(path, mount)| (path.to_string(), mount))
}
pub(crate) fn uri_path(uri: &str) -> String {
    url::Url::parse(uri)
        .map(|u| u.path().to_owned())
        .unwrap_or_else(|_| uri.to_owned())
}
