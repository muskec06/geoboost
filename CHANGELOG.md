# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses [Semantic Versioning](https://semver.org/).

## [0.2.0] - 2026-11-12

First public release under the name **GeoBoost**.

### Added
- English user interface: panel, tray menu, log and error messages.
- Automatic port fallback: if a local port is busy, the next free port is used and saved.
- Restores leftover system settings from pre-rename builds (`%APPDATA%\satmux`) on start.
- Satellite-link emulator and reproducible latency benchmark (`tools/`).
- GitHub Actions workflow that builds and tests every push and publishes `v*` tags as releases.

### Changed
- Renamed from "satmux for Windows" to GeoBoost (`geoboost.exe`, `%APPDATA%\GeoBoost`).
- Default local ports moved away from common dev-server ports: SOCKS5 `11080`, HTTP `18080`, gate `18081`, panel `18090`.
- Server installer installs a `geoboost` systemd service (`/etc/geoboost.env`, `/opt/geoboost`).

## [0.1.1] - 2026-11-12 (pre-release)

### Fixed
- The panel failed to open when port 8090 was taken by another application.

## [0.1.0] - 2026-11-12 (pre-release)

### Added
- Initial Windows client: satmux v3 protocol (12 warm carriers, SOCKS5, HTTP/CONNECT, 1-RTT gate).
- Tray icon, local control panel, built-in speed test.
- Windows integration (system proxy, git, env vars) with snapshot/restore, fallback and crash recovery.
- DPAPI-protected PSK and a hardened server installer.
