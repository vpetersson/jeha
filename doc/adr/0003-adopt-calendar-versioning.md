# 3. Adopt calendar versioning

Date: 2026-09-20

## Status

Accepted

## Decision

JEHA is versioned with CalVer using the scheme `YY.MM.MICRO`:

- `YY` — year, two digits, no padding (`26` for 2026)
- `MM` — month, no zero padding (`9` for September, `10` for October)
- `MICRO` — release counter within that month, starting at `0`

The first CalVer release is `26.9.0`, superseding `0.7.0`. Git tags keep the
existing `v` prefix (`v26.9.0`), which is what the release workflow triggers on
and what the published Docker tag is named after.

Zero padding is omitted because Cargo requires a valid SemVer version string,
and SemVer rejects leading zeros in numeric identifiers. `26.09.0` would not
parse; `26.9.0` does, and still orders correctly because each component is
compared numerically.

## Consequences

- A version number says when a build was cut, not what it promises about
  compatibility. Compatibility is instead carried by `schema_version` in the
  config file, which is validated at startup and has its own migration path.
- SemVer ordering still holds, so Cargo, Docker, and `gh release` continue to
  treat newer versions as newer. The jump from `0.7.0` to `26.9.0` is a normal
  version increase for every one of those tools.
- Users cannot infer "this upgrade is breaking" from the version alone. Breaking
  changes have to be called out in release notes.
- JEHA is a standalone daemon, not a library published to crates.io, so nothing
  downstream depends on its version range.
