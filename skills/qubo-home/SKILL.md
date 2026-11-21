---
name: qubo-home
description: Controls QUBO air purifiers (HPH) and smart sockets (HSP) through the qubo MCP tools or the qubo CLI, on the local network or the QUBO cloud. Use it when the user asks about air quality, PM2.5, the purifier fan, filter life, or socket power and energy.
---

# QUBO home control

## Tools

- MCP: `get_home_telemetry`, `refresh_sensors`, `control_purifier`, `control_socket`.
- CLI: `qubo status --json`, `qubo purifier power|speed|mode`, `qubo socket power|energy`. Always add `--json`.
- Each result has `path`: `local` (home network) or `cloud` (QUBO cloud). The tools work the same on both paths.

## Procedure

1. Read the state first. Use `refresh_sensors` or `qubo status --json`.
2. If a value is `null`, the device did not report it. Do not guess the value.
3. If a field name is in `cached`, the value comes from an earlier session and can be old. Tell the user this when you use the value.
4. Make one change at a time.
5. Read `confirmed` in the result. If it is `false`, read the state again before you try again. Try again only once.
6. Tell the user what you changed and the new value.

## Device behavior

A speed command also sets the mode to `manual`. To go back, set mode `auto`.

## Air quality defaults

PM2.5 is in µg/m³. These bands are defaults. The user can change them.

| PM2.5 | Action |
| :--- | :--- |
| 0 to 30 | Set mode `auto`. |
| 31 to 60 | Set mode `manual` and speed 2. |
| More than 60 | Set mode `manual` and speed 3. Tell the user the value. |

If `filter_hours` is less than 500, tell the user to get a new filter.

## Safety rules

- Never set `force` to true on `control_socket` without a clear "yes" from the user for that switch-off. First tell the user the load in watts.
- Do not switch off a device that the user did not name.
- There is no filter-reset action. Do not send raw MQTT messages.
- If a tool reports that the broker is not available, stop and tell the user. Do not try again in a loop.
- If `path` is `cloud` and a command fails with a login error, tell the user to run `qubo login`. Do not ask for the password.
- Never show or log the content of `session.json`.
