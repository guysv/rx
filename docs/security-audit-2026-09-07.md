# Dependency security audit — 2026-09-07

Audited `guysv/rxx` default branch `master` at `1b7aea2` using
`cargo-audit 0.22.2` and RustSec database commit
`5a0ebedfe8bdd2e295b171f4162f8c977bcad9a5`.
GitHub's Dependabot and code-scanning alert APIs returned HTTP 403 with the
available credentials. These results are an independent dependency audit,
not confirmation of the repository's private alert state or a source-code audit.

## Fixed vulnerability matches

| Package | Advisory | Lockfile fix |
| --- | --- | --- |
| chrono 0.4.19 | [RUSTSEC-2020-0159](https://rustsec.org/advisories/RUSTSEC-2020-0159) | Update to 0.4.45 |
| time 0.1.44 | [RUSTSEC-2020-0071](https://rustsec.org/advisories/RUSTSEC-2020-0071) | Removed by the chrono update |
| generic-array 0.12.3 | [RUSTSEC-2020-0146](https://rustsec.org/advisories/RUSTSEC-2020-0146) | Update to 0.12.4 |
| remove_dir_all 0.5.2 | [RUSTSEC-2023-0018](https://rustsec.org/advisories/RUSTSEC-2023-0018) | Removed by updating tempfile from 3.2.0 to 3.27.0 |
| crossbeam-epoch 0.9.18 | [RUSTSEC-2026-0204](https://rustsec.org/advisories/RUSTSEC-2026-0204) | Update to 0.9.21 |

## Other resolved warnings

- [RUSTSEC-2026-0190](https://rustsec.org/advisories/RUSTSEC-2026-0190): update anyhow from 1.0.100 to 1.0.104.
- [RUSTSEC-2022-0041](https://rustsec.org/advisories/RUSTSEC-2022-0041): update redox_users from 0.3.4 to 0.3.5, replacing crossbeam-utils 0.7.2 with the existing 0.8.21 dependency through rust-argon2.
- [RUSTSEC-2026-0097](https://rustsec.org/advisories/RUSTSEC-2026-0097): remove rand 0.8.4 through the tempfile update and update rand 0.9.2 to 0.9.5.
- [RUSTSEC-2026-0105](https://rustsec.org/advisories/RUSTSEC-2026-0105): update bitstream-io from 4.9.0 to 4.10.0, replacing unmaintained core2 with no_std_io2.

These are compatible updates within the existing manifest constraints. Application
code and dependency declarations are unchanged. Matches include transitive,
platform-specific, and development dependencies; a match does not establish an
exploitable execution path in RXX.

## Remaining maintenance warnings

- `lzw 0.10.0`, through `gif 0.10.3`: [RUSTSEC-2020-0144](https://rustsec.org/advisories/RUSTSEC-2020-0144).
- `paste 1.0.15`, through `metal` and `rav1e`: [RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436).

These advisories identify unmaintained crates. Neither has a patched release
listed. Removing them requires dependency migrations beyond these compatible
security updates. No advisories were ignored or suppressed.

## Validation

- Before: 5 vulnerability matches, 4 soundness warnings, 3 maintenance warnings.
- After: 0 vulnerability matches, 0 soundness warnings, 2 maintenance warnings.
- `cargo test --locked --no-default-features`: passed on macOS, with 100 unit
  tests, 38 integration/replay tests, and 18 doc tests passing; 1 test ignored.
- `RUSTFLAGS='-D warnings' cargo check --locked --all-targets`: failed on unused
  methods in unchanged source (`wgpu_texture`, `compile_str`, `wait_changed`).
- `cargo check --locked --all-targets`: passed with the default desktop features;
  existing unused-method warnings remain. Other operating systems were not tested.
- `git diff --check`: passed.
- Reproduce the dependency check with `cargo audit`; future database updates may
  change its findings. Builds and tests should use `--locked` to retain the
  reviewed dependency versions.
