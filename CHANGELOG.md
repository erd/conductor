# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/erd/conductor/compare/v0.1.0...v0.2.0) - 2026-10-20

### Added

- Add the deck_icons setting ([#48](https://github.com/erd/conductor/pull/48))

## [0.1.0](https://github.com/erd/conductor/releases/tag/v0.1.0) - 2026-10-19

### Added

- Add conductor resume and an end-to-end smoke test ([#34](https://github.com/erd/conductor/pull/34))
- Add the conductor and status skills ([#33](https://github.com/erd/conductor/pull/33))
- Package conductor as a Claude Code plugin ([#29](https://github.com/erd/conductor/pull/29))
- Poll PR status in conductor watch ([#28](https://github.com/erd/conductor/pull/28))
- Add conductor watch to wake the conductor on worker events ([#27](https://github.com/erd/conductor/pull/27))
- Add conductor teardown for finished tasks ([#24](https://github.com/erd/conductor/pull/24))
- Add conductor spawn to launch workers ([#23](https://github.com/erd/conductor/pull/23))
- Add conductor task commands ([#22](https://github.com/erd/conductor/pull/22))
- Add worker report and decision commands ([#21](https://github.com/erd/conductor/pull/21))
- Add conductor hook to record hook events ([#20](https://github.com/erd/conductor/pull/20))
- Add CLI skeleton, config, and init/start/status ([#19](https://github.com/erd/conductor/pull/19))

### Fixed

- Wake on an unreported idle worker only once per user or conductor prompt ([#42](https://github.com/erd/conductor/pull/42))
- Show the whole pending command in permission-prompt wakes ([#41](https://github.com/erd/conductor/pull/41))
- Resume a task when the user answers its decision in the worker's pane ([#40](https://github.com/erd/conductor/pull/40))
- Keep PR state fresh in conductor status ([#36](https://github.com/erd/conductor/pull/36))
- Parse wt remove's JSON array in teardown ([#32](https://github.com/erd/conductor/pull/32))
- Tolerate a closed stdin pipe in hook and plugin tests ([#30](https://github.com/erd/conductor/pull/30))

### Other

- License the project under Apache 2.0 ([#43](https://github.com/erd/conductor/pull/43))
- Set up Cargo workspace and CI ([#16](https://github.com/erd/conductor/pull/16))
