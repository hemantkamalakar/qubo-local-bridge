# Security policy

## Supported versions

Only the latest release receives security fixes.

## Report a vulnerability

Do not open a public issue for a security problem.

Use the private vulnerability reporting of this repository. Open the Security tab and select "Report a vulnerability". You can also write to the author at hemant.kamalakar@gmail.com.

Include these items in your report:

- The version of `qubo` (`qubo --version`) and your operating system.
- The steps to reproduce the problem.
- The effect that you expect an attacker to have.

Remove real tokens, phone numbers and device IDs from your report.

We try to answer within 7 days. We ask you to keep the problem private until a fix is available.

## Known advisories

The dependency audit (`cargo deny check`) reports some advisories that we accept. Each one has a reason in `deny.toml`.

- The embedded broker library (`rumqttd`) uses `rustls` 0.22 and `rustls-webpki` 0.102. The advisories for these crates are about checking certificates and client sessions. The broker is a server. It does not check client certificates, and it does not use revocation lists.
- The `rsa` crate has a timing advisory with no fix. `qubo` uses it only to generate keys.
- A few crates are unmaintained and have no replacement yet.

We check these entries again at each release. Tell us if you see a way to reach one of these problems.

## Scope

These topics are in scope:

- Handling of cloud tokens and the session file.
- The local broker and its certificates.
- The MCP server, including its HTTP transport.
- Commands that can switch a device in an unsafe way.

These topics are out of scope:

- Vulnerabilities in the QUBO cloud, the QUBO app or the device firmware. Report those to the vendor.
- A local broker that you expose to an untrusted network. The device port has no login by design. See the README.
