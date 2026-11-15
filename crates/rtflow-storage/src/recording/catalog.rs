use super::reader::SegmentReader;
use super::*;

pub(super) fn inspect_segment(
    path: &Path,
    max_sample_size: usize,
) -> Result<(MediaDescription, SegmentInfo)> {
    let (description, mut reader) = SegmentReader::open(path, max_sample_size)?;
    let mut start = None;
    let mut end = Duration::ZERO;
    let mut units = 0;
    while let Some(unit) = reader.next_unit()? {
        start.get_or_insert(unit.pts);
        end = end.max(unit.pts);
        units += 1;
    }
    Ok((
        description,
        SegmentInfo {
            path: path.to_owned(),
            start: start.unwrap_or_default(),
            end,
            bytes: fs::metadata(path)?.len(),
            units,
        },
    ))
}

pub(super) fn prune(config: &RecordingConfig) -> Result<()> {
    let catalog = match RecordingCatalog::scan(&config.directory, usize::MAX) {
        Ok(catalog) => catalog,
        Err(e) => {
            // A failed scan must not silently disable disk-limit enforcement.
            tracing::warn!("recording prune: catalog scan failed: {e}");
            return Ok(());
        }
    };
    let mut bytes = catalog
        .segments
        .iter()
        .map(|segment| segment.bytes)
        .sum::<u64>();
    let mut count = catalog.segments.len();
    for segment in catalog.segments {
        if bytes <= config.max_disk_bytes && count <= config.max_segments {
            break;
        }
        if let Err(e) = fs::remove_file(&segment.path) {
            tracing::warn!(
                "recording prune: failed to remove {}: {e}",
                segment.path.display()
            );
        }
        bytes = bytes.saturating_sub(segment.bytes);
        count -= 1;
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentInfo {
    pub path: PathBuf,
    pub start: Duration,
    pub end: Duration,
    pub bytes: u64,
    pub units: u64,
}

#[derive(Clone, Debug)]
pub struct RecordingCatalog {
    pub description: MediaDescription,
    pub segments: Vec<SegmentInfo>,
}

impl RecordingCatalog {
    pub fn recover(directory: impl AsRef<Path>) -> Result<usize> {
        let mut removed = 0;
        if !directory.as_ref().exists() {
            return Ok(0);
        }
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "partial")
            {
                fs::remove_file(path)?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    pub fn scan(directory: impl AsRef<Path>, max_sample_size: usize) -> Result<Self> {
        let mut paths = fs::read_dir(directory)?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "rtspseg")
            })
            .collect::<Vec<_>>();
        paths.sort();
        let mut description = None;
        let mut segments = Vec::new();
        for path in paths {
            // A corrupt segment must not make the whole directory VOD
            // unavailable; skip it and keep the rest.
            let (candidate, info) = match inspect_segment(&path, max_sample_size) {
                Ok(result) => result,
                Err(e) => {
                    tracing::warn!(
                        "recording scan: skipping corrupt segment {}: {e}",
                        path.display()
                    );
                    continue;
                }
            };
            if description
                .as_ref()
                .is_some_and(|current| current != &candidate)
            {
                return Err(Error::Media(
                    "recording segments have incompatible descriptions".into(),
                ));
            }
            description = Some(candidate);
            segments.push(info);
        }
        let description =
            description.ok_or_else(|| Error::NotFound("recording segments".into()))?;
        Ok(Self {
            description,
            segments,
        })
    }

    pub fn duration(&self) -> Duration {
        match (self.segments.first(), self.segments.last()) {
            (Some(first), Some(last)) => last.end.saturating_sub(first.start),
            _ => Duration::ZERO,
        }
    }
}
