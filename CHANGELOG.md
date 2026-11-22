# Changelog

All notable changes to this project are in this file. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). The project follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.1] - 2026-11-22

### Changed

- Update dependencies: `rsa` 0.10, `rand` 0.10, `base64` 0.23 and `comfy-table` 8.
- Update the GitHub Actions in the CI and release workflows.

## [0.1.0] - 2026-11-21

### Added

- The `qubo` command to control QUBO purifiers (HPH) and smart sockets (HSP).
- Cloud path: login with e-mail, or with a phone number and an SMS code.
- Local path: embedded MQTT broker (`qubo serve`) with a generated certificate authority.
- `qubo doctor`, `qubo discover` and `qubo service` for setup, checks and start at boot.
- MCP server (`qubo mcp`) over `stdio` and streamable HTTP.
- Agent skill in `skills/qubo-home`.
- Safety check: a socket does not switch off above 15 W unless `--force` is set.
- Release archive for macOS on Apple Silicon (`macos-arm64`).
