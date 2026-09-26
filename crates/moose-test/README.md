# moose-test

Test harness for moose audio plugins.

## Overview

Helpers for offline rendering, assertion, and validation of moose plugins,
all in-process with no DAW or host simulation needed. Use this crate in
your plugin's integration tests to verify audio output, parameter
behavior, state persistence, and editor lifecycle.

## Entry points

- **`driver!(MyPlugin)`** macro returns a `PluginDriver` builder. Configure
  it (sample rate, block size, MIDI events, parameter overrides, script
  callbacks) and call `.run()` to get back a `DriverResult` with the
  rendered audio, output events, and meter readings.
- **`screenshot!(MyPlugin)`** macro returns a `ScreenshotTest` builder
  for golden-image regression tests on the plugin editor.

## Assertions

Audio-shape assertions live in `moose_test::assertions` and take a
`&DriverResult<P>`:

- `assert_nonzero`, `assert_silence`
- `assert_no_nans`
- `assert_peak_below`
- `assert_silence_after`, `assert_nonzero_after`
- `assert_silence_between`, `assert_nonzero_between`
- `assert_meter_above`, `assert_meter_below`
- `assert_output_event_count`

Whole-plugin contract assertions live at the crate root and take only a
type parameter (the driver is set up internally):

- `assert_state_round_trip`, `assert_corrupt_state_no_crash`,
  `assert_empty_state_no_crash`
- `assert_has_editor`, `assert_editor_lifecycle`,
  `assert_editor_size_consistent`
- `assert_param_defaults_match`, `assert_param_normalized_clamped`,
  `assert_param_normalized_roundtrip`, `assert_param_count_matches`,
  `assert_no_duplicate_param_ids`
- `assert_valid_info`
- `assert_bus_config_effect`, `assert_bus_config_instrument`

## Usage

```rust
use moose_test::{driver, assertions::*};

#[test]
fn effect_renders_nonzero() {
    let result = driver!(MyEffect).block_size(1024).sample_rate(44_100.0).run();
    assert_no_nans(&result);
    assert_nonzero(&result);
    assert_peak_below(&result, 1.0);
}

#[test]
fn state_round_trips() {
    moose_test::assert_state_round_trip::<MyEffect>();
}
```

Part of [moose](https://github.com/Matari-Audio/moose). [Docs](https://truce.audio/docs/).
