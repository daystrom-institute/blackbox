# Release Process

Blackbox uses `Cargo.toml` as the version source, annotated git tags as immutable
release markers, and GitHub Releases as the public release surface.

## Versioning

- Use Semantic Versioning tags in the form `vX.Y.Z`.
- Keep the crate version in `Cargo.toml` synchronized with the release tag.
- For `0.y.z` releases, breaking changes are allowed but must be called out in
  `CHANGELOG.md`.

## Changelog

- `CHANGELOG.md` is the source of truth because this repo does not rely on
  GitHub PRs for release note generation.
- Add user-visible changes under `Unreleased` as work lands.
- At release time, move those entries into a dated `X.Y.Z - YYYY-MM-DD` section.

## Release Checklist

```bash
cargo test
git status --short
```

Then:

1. Update `Cargo.toml` to the target version.
2. Update `Cargo.lock` if Cargo rewrites the root package version.
3. Move `CHANGELOG.md` entries from `Unreleased` to `X.Y.Z - YYYY-MM-DD`.
4. Commit the release metadata.
5. Create an annotated tag:

```bash
git tag -a vX.Y.Z -m "vX.Y.Z"
```

6. Push the commit and tag.
7. Create a GitHub Release from the tag, using the matching changelog section as
   the release body.
