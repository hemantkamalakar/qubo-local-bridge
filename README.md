# qubo-local-bridge

[![M8ven Score](https://m8ven.ai/badge/mcp/hemantkamalakar-qubo-local-bridge-4vyqk4?v=ce732e247d7b7b77510a96ca9f5aaa88)](https://m8ven.ai/mcp/hemantkamalakar-qubo-local-bridge-4vyqk4?s=readme)

One command, `qubo`, controls QUBO smart air purifiers and smart sockets. It works through the QUBO cloud, or only on your home network. It also has an MCP server for AI assistants.

`qubo` is one static Rust binary. It has no runtime and no external broker.

This is an unofficial project. See [Disclaimer](#disclaimer).

## Features

- Read PM2.5, filter life, fan state and socket energy.
- Switch devices and set the purifier fan speed and mode.
- Use the cloud path with no network change, or the local path with no internet.
- Run an embedded MQTT broker with `qubo serve`. No Docker is necessary.
- Use `--json` on every command for scripts and agents.
- Refuse to switch off a socket that carries a load of more than 15 W, unless you use `--force`.
- Connect AI assistants through the MCP server (`qubo mcp`) and the agent skill in `skills/qubo-home`.

## Supported devices

| Device | Family | Status |
| :--- | :--- | :--- |
| QUBO Smart Air Purifier | `HPH` | Tested on a Q400, cloud path. |
| QUBO Smart Plug (socket) | `HSP` | Implemented. Not yet tested on hardware. |

QUBO does not publish a protocol description. The protocol comes from community reverse engineering. Other devices and firmware versions can behave differently.

## Install

### Download a binary

Download the archive for your system from the Releases page. The archives are:

- `windows-x64`
- `linux-x64`
- `linux-arm64` (Raspberry Pi)
- `macos-arm64` (Apple Silicon)

Each archive has a SHA-256 checksum file. Unpack the archive and put `qubo` in your `PATH`.

### Build from source

Rust 1.89 or newer is necessary.

```sh
cargo install --locked --path .
```

To build without installing, run `cargo build --release`. The binary is `target/release/qubo`.

## Quick start: cloud path

The cloud path needs no network change. The QUBO app continues to work.

1. Log in with your QUBO account: `qubo login --phone <number>` or `qubo login --email <address>`.
2. Read the state of your devices: `qubo status`.
3. Control a device: `qubo purifier speed 2` or `qubo socket energy`.

If your account has no password, for example when you use "Continue with Google", try `--phone`. `qubo` asks for the SMS code and does not store it.

## Local path: no internet

The local path moves all QUBO devices of your network to your own broker.

Caution: while the DNS record exists, the QUBO app cannot control the devices.

Caution: the device port (TCP 8883) has no login. Allow it from your local network only. On Windows, `qubo service install` adds a firewall rule for this. On Linux, add one yourself, for example `sudo ufw allow from 192.168.0.0/16 to any port 8883 proto tcp`.

1. Copy `qubo` to a computer that is always on.
2. Start the local broker at boot: `qubo service install`. On Windows, accept the administrator prompt.
3. In your router, Pi-hole or AdGuard Home, add the DNS record `mqtt.platform.quboworld.com` -> IP address of this computer.
4. Disconnect each QUBO device from power for 10 s, then connect it again.
5. Check the setup: `qubo doctor`.
6. Read the state: `qubo status`.

To go back to the cloud path, delete the DNS record. Then disconnect the devices from power for 10 s and run `qubo service uninstall`.

If a firmware update makes the devices check the certificate more strictly, the local path can stop working. The cloud path is not affected.

## Commands

| Command | Action |
| :--- | :--- |
| `qubo login`, `qubo logout` | Save or delete the cloud login. |
| `qubo serve` | Run the local broker in the foreground. |
| `qubo service install`, `uninstall`, `status` | Manage the start at boot. |
| `qubo doctor` | Check the login, the broker, the DNS record and the devices. |
| `qubo discover` | Find the devices and save them. |
| `qubo status` | Print the state of all devices. |
| `qubo purifier power\|speed\|mode` | Control a purifier. |
| `qubo socket power\|energy` | Control a socket. |
| `qubo mcp` | Run the MCP server (`stdio` or `streamable-http`). |

Use `--mode cloud` or `--mode local` to pick a path. The default is `auto`: local if the broker of `qubo serve` answers, else cloud.

Use `--json` for machine-readable output. If a command fails, it prints `{"error": "..."}` and exits with code 1.

## AI assistants

`qubo mcp` is an MCP server. It has 4 tools: `get_home_telemetry`, `refresh_sensors`, `control_purifier` and `control_socket`.

To add it to Claude Code, run:

```sh
claude mcp add qubo -- qubo mcp
```

For other MCP clients, add a server with the command `qubo` and the argument `mcp`:

```json
{ "mcpServers": { "qubo": { "command": "qubo", "args": ["mcp"] } } }
```

To serve over HTTP, run `qubo mcp --transport streamable-http --port 8000`. The endpoint is `/mcp` on `127.0.0.1`.

Warning: the HTTP transport has no login. Bind it to a loopback or trusted address only.

The folder `skills/qubo-home` has an agent skill. It tells an assistant how to read the state and which safety rules to follow.

`qubo mcp` also serves this skill as `skill://qubo-home/SKILL.md`, through the [MCP Skills extension](https://modelcontextprotocol.io/extensions/skills/overview) and as an MCP resource.

Claude Code supports MCP skills as a public beta, in version 2.1.331 or later. It shows the skill as `qubo:qubo-home`. `claude -p` does not support MCP skills. For all requirements, see the [Claude Code documentation](https://code.claude.com/docs/en/mcp#use-skills-from-mcp-servers).

Support in other clients differs. If your client does not load the skill, copy `skills/qubo-home` to its skills folder.

## Files and settings

| Item | Use |
| :--- | :--- |
| `~/.config/qubo/config.json` | Mode, broker address and known devices. |
| `~/.config/qubo/session.json` | Cloud tokens. Only the owner can read it on Unix. |
| `~/.config/qubo/state.json` | Cache of the last device values. |
| `~/.config/qubo/broker/` | Certificates of the local broker. |
| `QUBO_CONFIG_PATH` | Environment variable. Moves all files next to the path that you give. |
| `RUST_LOG` | Environment variable. Sets the log level, for example `info`. |

Never share `session.json` or the files in `broker/`. Logs go to stderr.

## Security

- The cloud tokens are stored in `session.json`. `qubo logout` deletes it.
- The local broker accepts devices without a login. Use a firewall rule, as described above.
- The broker listens for local tools on `127.0.0.1:1883` only.
- `qubo` makes a private certificate authority for the local path. Its key stays on your computer.

To report a vulnerability, read [SECURITY.md](SECURITY.md).

## Development

```sh
cargo test --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo fmt --check
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

The test suite has unit tests, doc tests and end-to-end tests. Each test checks one behavior. The end-to-end tests run `qubo serve`, connect a simulated purifier over TLS, and control it with the `qubo` binary.

| Module | Use |
| :--- | :--- |
| `protocol` | Topics, command payloads and report parsing. No I/O. |
| `client` | MQTT client that tracks the state of every device. |
| `broker`, `certs`, `serve` | Embedded MQTT broker, certificates and `qubo serve`. |
| `cloud`, `connect` | QUBO account login, device list, and path selection. |
| `doctor`, `service` | Diagnostics and start at boot. |
| `cli`, `mcp_server` | Command line and MCP server. |

Read [CONTRIBUTING.md](CONTRIBUTING.md) before you open a pull request.

## Acknowledgements

The protocol layer (`src/protocol.rs`) follows the design and the test cases of the MIT-licensed project "qubo-local-control" by Ankur Pandey. The community research on the QUBO protocol made this project possible. The license text of that project is in [NOTICE](NOTICE).

## License

MIT. See [LICENSE](LICENSE). Third-party notices are in [NOTICE](NOTICE).

## Author

Hemant Kamalakar (hemant.kamalakar@gmail.com).

## Disclaimer

This project is not affiliated with, endorsed by or supported by QUBO or its owners. QUBO is a trademark of its owner. The protocol was learned by observation and community work. The software comes with no warranty. A firmware or cloud change can break it. Use it at your own risk.
