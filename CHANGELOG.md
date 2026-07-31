# Changelog

User-visible changes to mintPod are recorded here. This project follows [Semantic Versioning](https://semver.org/) while it is pre-`1.0`.

## Unreleased

### Changed

- Live GPU availability is now an obvious refreshable action that lists every acceptable GPU with its priority, VRAM, stock level, and current RunPod USD rate.
- Rejected GPU candidates explain whether the blocker is stock, VRAM, Secure Cloud availability, missing price data, or the selected maximum rate.
- Manual session termination now shows cleanup attempts instead of leaving the running screen looking stalled.

### Fixed

- Setup and recovery errors now stay on the screen that can act on them.
- A malformed API-key profile index or session-history file opens a recoverable startup screen instead of a dead-end setup state.
- Resetting a malformed local file no longer overwrites an earlier `.broken` backup.
- Long streaming generations remain active for idle-timeout purposes.
- Startup failures, price currency, stale availability checks, and close-time cleanup reporting are more robust.

## 0.1.1 - 2026-07-30

### Fixed

- Corrected RunPod runtime proxy forwarding while preserving remote authentication.

## 0.1.0 - 2026-07-30

- First pre-release of the focused RunPod-to-coding-agent workflow.
