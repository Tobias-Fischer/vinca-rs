## Tooling

- PREFER `pixi run` tasks over executables from your system.

## Compatibility with vinca

- Generated recipes MUST stay equivalent to vinca's. Document deliberate differences under "Comparison with vinca" in [README.md](README.md); `tools/compare.py` compares two recipe directories by meaning.

## Testing

- PREFER inline `insta` snapshots over substring assertions when testing rendered output.
- New tests MUST follow local test conventions. Check existing coverage before adding tests.
- PREFER running specific tests during development; run `pixi run lint` and `pixi run test` before finishing.

## Dependencies

- You MUST NOT work around limitations or bugs in rattler, rattler-build or pixi-build-ros. Fix them upstream instead.

## Linting

- `pixi run typos` MUST pass. Suppress deliberate misspellings with `// spellchecker:off` / `// spellchecker:on` or their `#` equivalents.
- TOML files MUST be formatted with `pixi run toml-format` and pass `pixi run toml-lint`.

## Rust

- Use enums with variant data to represent mutually exclusive states instead of contradictory flags or dependent `Option` fields.
- Each module that can fail has its own `thiserror` + `miette::Diagnostic` error enum.

## Error messages

- Authored diagnostic messages and warnings MUST start with a lowercase letter and end without a trailing dot. This does not apply to quoted upstream errors.

## Releases

- A release bumps the version in `Cargo.toml`, `pixi.toml` and `Cargo.lock` together.

## Pull requests

- Pull requests MUST follow the [pull request template](.github/pull_request_template.md), including a human-written description outside the AI-generated details.
