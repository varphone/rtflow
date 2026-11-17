# rtflow-onvif

Standalone ONVIF discovery/client support for rtflow:

- WS-Discovery on `239.255.255.250:3702` (`NetworkVideoTransmitter`)
- `GetDeviceInformation`, `GetCapabilities(Media)`, `GetProfiles`, `GetStreamUri`
- HTTP Basic and Digest authentication

```rust
let devices = rtflow_onvif::discover(std::time::Duration::from_secs(3)).await?;
let details = rtflow_onvif::inspect(
    &devices[0].xaddr,
    Some("admin".into()),
    Some("secret".into()),
).await?;
for profile in details.profiles {
    println!("{}: {:?}", profile.name, profile.uri);
}
```

Limitations: ONVIF ver10 media service, no WS-Security UsernameToken, HTTPS uses
the system trust store (self-signed devices need HTTP or a trusted CA).

The `rtflow` facade re-exports this crate as `rtflow::onvif` when built with the
`onvif` feature; `rtflow-server` enables it with `--features onvif`.
