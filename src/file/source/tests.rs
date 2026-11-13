use super::*;

#[test]
fn extension_detection_and_timestamps() {
    assert_eq!(
        detect_format(Path::new("movie.MOV"), ContainerFormat::Auto).unwrap(),
        ContainerFormat::Mp4
    );
    assert_eq!(rtp_timestamp(Duration::from_millis(500), 90_000), 45_000);
    assert_eq!(
        duration_from_ticks(45_000, 90_000),
        Duration::from_millis(500)
    );
}

#[test]
fn rejects_invalid_options() {
    let options = FileSourceOptions {
        playback_rate: 0.0,
        ..Default::default()
    };
    assert!(validate_options(&options).is_err());
}
