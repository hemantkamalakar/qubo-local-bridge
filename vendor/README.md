# Patched crates

`rumqttc` 0.25.1 and `rumqttd` 0.20.0 pin versions of `rustls` and `rustls-webpki` that have security advisories.
Upstream has no release with a fix. The license is Apache-2.0.

These copies come from crates.io. Only these items are different:

- `rumqttc/Cargo.toml`: `rustls-webpki` is 0.103.
- `rumqttd/Cargo.toml`: `rustls-webpki` is 0.103 and `tokio-rustls` is 0.26 with the `ring` provider.
- Examples, tests and extra documents are removed, together with their manifest entries.

The Rust source has no changes.

When upstream releases a fix, delete this directory and the `[patch.crates-io]` table in `Cargo.toml`.
Dependabot does not update these copies.
