//! RTSP Normal Play Time range parsing and rendering.
use crate::{Error, Result};
use std::time::Duration;

/// How a live RTSP presentation maps media time to Normal Play Time.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum TimelineMode {
    /// Per-session live range (`npt=now-`/`npt=0-`); VLC starts at zero.
    #[default]
    Relative,
    /// NPT starts at seconds since the server process started; reconnects
    /// continue on the same clock.
    ServerUptime,
    /// NPT starts at Unix epoch seconds.
    Unix,
    /// NPT starts at `offset + seconds since server start`.
    Absolute { offset: Duration },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NptPosition {
    Now,
    Time(Duration),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NptRange {
    pub start: NptPosition,
    pub end: Option<Duration>,
}

impl NptRange {
    pub fn parse(value: &str) -> Result<Self> {
        let value = value
            .trim()
            .strip_prefix("npt=")
            .ok_or_else(|| Error::Rtsp("unsupported RTSP Range unit".into()))?;
        let (start, end) = value
            .split_once('-')
            .ok_or_else(|| Error::Rtsp("invalid NPT range".into()))?;
        let start = if start.eq_ignore_ascii_case("now") {
            NptPosition::Now
        } else {
            NptPosition::Time(parse_seconds(start)?)
        };
        let end = (!end.trim().is_empty())
            .then(|| parse_seconds(end))
            .transpose()?;
        if let (NptPosition::Time(start), Some(end)) = (start, end)
            && end < start
        {
            return Err(Error::Rtsp("NPT range end precedes start".into()));
        }
        Ok(Self { start, end })
    }

    pub fn render(self) -> String {
        let start = match self.start {
            NptPosition::Now => "now".into(),
            NptPosition::Time(value) => render_seconds(value),
        };
        let end = self.end.map(render_seconds).unwrap_or_default();
        format!("npt={start}-{end}")
    }
}

fn parse_seconds(value: &str) -> Result<Duration> {
    let value = value
        .trim()
        .parse::<f64>()
        .map_err(|_| Error::Rtsp("invalid NPT timestamp".into()))?;
    if !value.is_finite() || value < 0.0 {
        return Err(Error::Rtsp("invalid NPT timestamp".into()));
    }
    Duration::try_from_secs_f64(value).map_err(|_| Error::Limit("NPT timestamp"))
}

fn render_seconds(value: Duration) -> String {
    let mut rendered = format!("{:.6}", value.as_secs_f64());
    while rendered.ends_with('0') {
        rendered.pop();
    }
    if rendered.ends_with('.') {
        rendered.push('0');
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npt_range_round_trips_and_rejects_reverse_ranges() {
        let range = NptRange::parse("npt=12.5-20").unwrap();
        assert_eq!(
            range.start,
            NptPosition::Time(Duration::from_millis(12_500))
        );
        assert_eq!(range.render(), "npt=12.5-20.0");
        assert!(NptRange::parse("npt=20-10").is_err());
        assert_eq!(NptRange::parse("npt=now-").unwrap().start, NptPosition::Now);
    }
}
