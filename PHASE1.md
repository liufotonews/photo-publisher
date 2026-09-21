# Photo Publisher — Phase 1.1

Phase 1.1 hardens the public contract layer established in Phase 1.

## Changes

- Require `source.type` whenever `source` is present.
- Require `domain.url` whenever `domain` is present.
- Standardize gallery `sequence` as one-based (`minimum: 1`).
- Reject duplicate photo IDs in the gallery validator.
- Add regression fixtures and tests for all of the above.
- Add GitHub Actions CI to run the Rust test suite on pushes to `main` and pull requests.

## Scope

- JSON Schema Draft 2020-12 contracts
- Project contract v1
- Gallery manifest contract v1
- Rust validator
- Valid/invalid fixtures
- Automated tests
- GitHub Actions CI

## Out of scope

GUI, GitHub/Vercel/R2 providers, Lightroom integration, AI adapters, publishing engine, and cloud provisioning remain out of scope for this phase.

## Contract note

The repository is still pre-1.0. The refinements in Phase 1.1 are intentionally applied before the contracts are treated as a stable public release. Future breaking contract changes after stabilization must use a new major contract version.
