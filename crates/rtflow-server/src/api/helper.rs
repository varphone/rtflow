use super::*;

pub(super) fn expected_revision(req: &HttpRequest) -> Result<Option<u64>, (u16, String)> {
    match req.header("if-match") {
        None => Ok(None),
        Some(value) => value
            .trim()
            .trim_matches('"')
            .parse::<u64>()
            .map(Some)
            .map_err(|_| (400, "If-Match must be a revision number".into())),
    }
}
