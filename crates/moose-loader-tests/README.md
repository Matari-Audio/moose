# moose-loader-tests

Integration-test host for `moose-loader`. Internal to the moose
workspace - `publish = false`, never goes to crates.io.

## Why this crate exists

The integration tests for `moose-loader` need
`#[derive(Params)]` / `#[derive(State)]`, which expand to
`::moose::params::*` / `::moose::core::*` paths - so the tests
need the umbrella `moose` crate in scope. But `moose` already
depends on `moose-loader` (for the `moose::plugin!` macro's
HotShell wiring), and adding `moose` as a `[dev-dependencies]`
entry on `moose-loader` would form a `moose <-> moose-loader`
cycle in cargo metadata.

Holding the tests in this separate crate breaks the loop: the
edges run `moose-loader-tests -> moose-loader` and
`moose-loader-tests -> moose`, with no back-edge into either.

Part of [moose](https://github.com/Matari-Audio/moose). [Docs](https://truce.audio/docs/).
