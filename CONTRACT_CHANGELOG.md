# Contract Changelog — Phase 1.1

## Project contract v1

- `source.type` is now required when `source` is present.
- `domain.url` is now required when `domain` is present.

## Gallery manifest contract v1

- `sequence`, when present, must be at least `1`.
- Duplicate `photo.id` values are rejected by the validator.

These changes are being made during the pre-1.0 stabilization phase. Once the contracts are released as stable public interfaces, breaking changes must use a new major contract version.
