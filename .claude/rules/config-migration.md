---
paths:
  - "src/main.rs"
  - "src/config.rs"
---

# Adding a config key — migration safety

`load_config()` reads a hardcoded system path, so the migration it performs is **not directly
testable**. Put the logic in `migrate_config_json(&mut serde_json::Value) -> bool` (already
extracted, in `src/main.rs`) and test THAT — the untestable wrapper is why a panicking migration
reached the fleet's clock master once already (#68).

## Never index a nested value mutably without proving it is an object

`serde_json`'s `IndexMut<&str>` **panics** on any value that is neither an object nor null. The
top-level `json["key"] = …` shape used by the older migrations is safe (the root is an object by
construction), but the nested form is not:

```rust
// PANICS on `"ntp_server_mode": true` — a very plausible hand-edit to enable it
} else if json["ntp_server_mode"].get("max_step_us").is_none() {
    json["ntp_server_mode"]["max_step_us"] = serde_json::json!(100_000);

// Correct: prove it is an object, and leave anything else alone
} else if let Some(obj) = json["ntp_server_mode"].as_object_mut() {
    if !obj.contains_key("max_step_us") {
        obj.insert("max_step_us".to_string(), serde_json::json!(100_000));
    }
}
```

A malformed value must fall through to `load_config()`'s existing log-loudly-then-default path.
On a daemon under systemd / Windows Service, a startup panic is a restart loop, not a degraded
start — and on the NTP master that takes the whole fleet's time source with it. Always add a test
that feeds the migration a bool, a string, a number and an array.

## Adding a field to a config struct

- Give it `#[serde(default = "…")]` and update the struct's `Default` impl. A config file written by
  an older version must still parse.
- Add a test that a config WITHOUT the key parses and gets the intended default — especially when
  the default is the safe/bounded value and `0` would mean "unbounded" or "always" (see
  `test_ntp_server_config_without_max_step_us_defaults_to_bounded_68`).
- Guard nonsense values where a literal reading would latch: `ntp_stale_secs: 0` would make every
  node permanently stale, so it is floored at one query cadence at read time
  (`effective_stale_window`), and `max_step_us <= 0` is treated as unbounded rather than as "never
  correct anything".
- Only migrate a key into an EXISTING object when the operator is meant to tune it. As of
  camera-box issue 1073, `SystemConfig`'s `servo`/`filters` DO carry `#[serde(default)]` (with
  `Default` impls holding the former inline values), and `gm_allowlist` is `#[serde(default)]`
  empty — so a partial `"system": {"gm_allowlist": [...]}}` block now parses cleanly (servo/filters
  fall back to their defaults). A `system.gm_allowlist` key MAY therefore be safely migrated into an
  existing file, or added by an operator, without breaking the parse. (Note the whole-FILE caveat:
  `Config.ntp_server` also gained a `#[serde(default)]` for the same rollout, so even a file that
  contains ONLY `{"system": {"gm_allowlist": [...]}}` parses instead of tripping `load_config`'s
  overwrite-with-defaults path — which would silently delete the operator's allowlist.)
