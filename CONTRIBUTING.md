# Contributing

Thank you for your interest. This guide tells you how to build, test and submit a change.

## Before you start

If your change is large, open an issue first. Describe the problem and your plan.

Warning: never put real tokens, passwords, phone numbers, MAC addresses or device IDs in an issue, a test or a log that you share. Replace them with made-up values.

## Set up

Rust 1.89 or newer is necessary.

1. Clone the repository.
2. Run `cargo build --locked`.
3. Run `cargo test --locked`.

## Quality gates

Every pull request must pass these commands:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo test --locked
```

## Code rules

- Do not use `unwrap()` or `expect()` outside tests.
- Do not add `unsafe` code. The crate forbids it.
- Use `thiserror` errors in the library. Use `anyhow` only in `main`.
- Document every public item. Add `# Errors` for a function that returns a `Result`.
- Add an example to the documentation of a public function when it is practical.
- Fix a lint. If you must silence it, use `#[expect(...)]` and write the reason.

## Test rules

- Test one behavior in each test. Use one assertion when possible.
- Give a test a name that reads like a sentence, for example `refuses_a_speed_above_3`.
- Group the tests of one unit in a module named after the unit.
- Share setup in helper functions. Keep the action and the assertion inside the test.
- Do not make a test depend on a real device, a real account or the internet.
- If you change a command payload, update the exact-payload test. The device firmware is strict.

## Pull requests

1. Make a branch from `main`.
2. Keep each pull request to one topic.
3. Add or update tests and documentation.
4. Add a line to `CHANGELOG.md` under `[Unreleased]`.
5. Fill in the pull request template.

## Test on real devices

If you test on a real device, say which model and firmware you used. If a command can change the state of a device, restore the state after your test.

## License

By contributing, you agree that your work is licensed under the MIT license of this project.
