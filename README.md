# Photo Publisher

Photo Publisher is a local-first photo publishing workspace. The Phase 4 CLI is a thin application layer over `publisher-core`, `publisher-pipeline`, and the existing contract validator.

## CLI v1

Build and run the executable with:

```text
cargo run -p photo-publisher-cli -- version
```

The Windows artifact is `photo-publisher.exe`.

For distribution, place the contract resource beside the executable:

```text
Photo Publisher/
├── photo-publisher.exe
└── schemas/
    └── project.schema.json
```

The CLI resolves this resource relative to the executable, not relative to
the current working directory or the compilation workspace. In development,
the resolver may find the same `schemas` directory in an executable's parent
directory ancestry; a missing resource is an explicit error.

```text
photo-publisher validate <project.json>
photo-publisher publish <project.json>
photo-publisher recover <project.json>
photo-publisher inspect <project.json>
photo-publisher version
```

The current public project schema does not contain an output field. Therefore CLI v1 derives the local output directory as `<project.json parent>/output`. Relative `source.path` values are resolved relative to the project file; absolute Windows paths are preserved.

### Flags

- `--silent` suppresses normal success output while preserving the exit code.
- `--verbose` emits detailed result information on stderr.
- `--dry-run` is supported by `publish` and calculates the existing core sync plan without changing publication files.
- `--json` emits only the stable result envelope on stdout.
- `--help` prints usage information.

The JSON envelope is:

```json
{"ok":true,"command":"inspect","data":{}}
```

Errors use:

```json
{"ok":false,"error":{"code":3,"category":"project_invalid","message":"..."}}
```

Diagnostics and verbose details use stderr. No credentials or secrets are printed.

### Exit codes

| Code | Meaning |
|---:|---|
| 0 | Success |
| 1 | Generic error (reserved for a future unclassified condition) |
| 2 | Invalid command or arguments |
| 3 | Invalid `project.json` or contract |
| 4 | Required resource does not exist |
| 5 | Validation error |
| 6 | Publication error |
| 7 | Recovery required or recovery failed |
| 8 | Publication blocked (reserved until a concrete v1 blocked condition exists) |
| 9 | Internal error |

`validate` is read-only. `inspect` is read-only and reports project, source, derived output, journal presence, generation, publication phase, recovery state, and photo count when available. Corrupt journal or state files are returned as controlled errors rather than being treated as absent. `recover` runs only the existing Phase 3 recovery operation; it may create the `.publisher` infrastructure, but it never scans, processes photos, or creates a new gallery/state publication. `publish` delegates scanning, hashing, sync planning, image processing, manifest generation, state, journal, backup, replacement, and recovery to the existing crates.

Example Windows invocation:

```text
photo-publisher publish "D:\Fotos\Projeto\project.json" --silent
```

Phase 3 journal, staging, backup, recovery, Windows-safe replacement, and public schemas remain unchanged by the CLI layer.
