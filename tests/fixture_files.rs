use rtflow::{Codec, FileSource, FileSourceOptions};
use std::{collections::HashSet, path::PathBuf};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn inspect(name: &str, expected: &[Codec]) {
    let source = FileSource::open(fixture(name), FileSourceOptions::default())
        .unwrap_or_else(|error| panic!("failed to open {name}: {error}"));
    let described = source
        .description()
        .tracks
        .iter()
        .map(|track| track.codec)
        .collect::<HashSet<_>>();
    for codec in expected {
        assert!(
            described.contains(codec),
            "{name} did not describe {codec:?}"
        );
    }

    let mut demuxer = source.into_demuxer();
    let mut seen = HashSet::new();
    for _ in 0..256 {
        let Some(sample) = demuxer.next_sample().unwrap() else {
            break;
        };
        assert!(
            !sample.unit.data.is_empty(),
            "{name} emitted an empty sample"
        );
        seen.insert(sample.unit.codec);
    }
    for codec in expected {
        assert!(seen.contains(codec), "{name} did not emit {codec:?}");
    }
}

#[test]
fn committed_fixtures_cover_every_supported_container_and_codec() {
    for name in [
        "h264-aac.mp4",
        "h264-aac.mov",
        "h264-aac-fragmented.mp4",
        "h264-aac.mkv",
        "h264-aac.ts",
        "h264-aac.m2ts",
    ] {
        inspect(name, &[Codec::H264, Codec::Aac]);
    }
    for name in ["h265.mp4", "h265.mkv", "h265.ts", "video.h265"] {
        inspect(name, &[Codec::H265]);
    }
    inspect("video.h264", &[Codec::H264]);
    inspect("audio.aac", &[Codec::Aac]);
    inspect("pcma.wav", &[Codec::Pcma]);
    inspect("pcmu.wav", &[Codec::Pcmu]);
    inspect("audio.pcma", &[Codec::Pcma]);
    inspect("audio.pcmu", &[Codec::Pcmu]);
}
