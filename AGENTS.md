# Repository Guidelines

These guidelines apply to the entire repository.

## Language

- Write all source code and documentation in English, including comments,
  identifiers, release notes, and commit messages.

## Branch Names

Follow Git Flow branch naming conventions:

- `main`: production history.
- `develop`: integration work, when used.
- `feature/<description>`: new features and routine improvements.
- `release/<version>`: release preparation, for example `release/0.5.0`.
- `hotfix/<description>`: urgent fixes to a production release.

Use concise English descriptions in lowercase with hyphens. Follow these names
when creating branches; do not create or restructure long-lived branches unless
the task requires it.

## Commit Messages

Follow Conventional Commits:

```text
<type>[optional scope][!]: <description>
```

Use types such as `feat`, `fix`, `docs`, `refactor`, `perf`, `test`, `build`,
`ci`, and `chore`. Write a concise English description. Mark breaking changes
with `!` or a `BREAKING CHANGE:` footer as appropriate.

Examples:

```text
feat(updates): add native update checks
fix(packaging): preserve Sparkle framework symlinks
docs(releases): add notes for version 0.5.0
```

## Release Notes

- Maintain release notes as UTF-8 English Markdown files in `docs/releases/`.
- Use one file per release, named after its Git tag, for example
  `docs/releases/v0.5.0.md` for tag `v0.5.0`.
- Describe user-visible changes, fixes, and any relevant compatibility or
  upgrade requirements. Include only changes actually delivered in that release.
- Use standard Markdown headings, paragraphs, lists, and links that Sparkle 2
  can render. Avoid GitHub-only markup, embedded HTML, and relative asset links
  that will not resolve when the notes are embedded in an appcast.
- Pass the release's file explicitly through `tools/publish-release.sh
  --notes-file`. The helper embeds its contents in the appcast's description
  with `sparkle:format="markdown"`.
- Use the same file with `gh release create --notes-file` or
  `gh release edit --notes-file` for the GitHub Release body. The publication
  helper's `--notes-file` option populates the appcast, not the GitHub Release
  body.

Example preparation from the repository root:

```sh
tools/publish-release.sh \
  --dmg target/dmg/CrossPuck-0.5.0.dmg \
  --tag v0.5.0 \
  --notes-file docs/releases/v0.5.0.md \
  --dry-run
```

Preparation is the default and does not upload assets. Use `--publish` to upload
to an existing non-prerelease GitHub Release and mark it latest. Finish Developer
ID signing, notarization, and stapling before preparing release assets.

## Release Versions

- Use the root `Cargo.toml` workspace package version as the app's display
  version. `tools/build-app.sh` writes it to `CFBundleShortVersionString`.
- `CFBundleVersion` is Sparkle's update comparison value. It defaults to the
  Cargo version; use `CROSSPUCK_BUILD_VERSION` for a separate numeric build
  sequence, and keep that sequence increasing for new update builds.
- `tools/build-dmg.sh` derives the default DMG filename from the packaged app's
  `CFBundleShortVersionString`, including when packaging a prebuilt app.
- Match `--tag` to the app's display version, with an optional `v` prefix.
  Create and push the Git tag and create its GitHub Release separately; the
  publication helper does not perform those steps or change the app version.
- Build release binaries from the source revision identified by the release
  tag. The publication helper checks version strings, not the source revision.
- Publish `appcast.xml` alongside every stable DMG release so the embedded
  `releases/latest/download/appcast.xml` feed URL remains available.
- Do not replace a published DMG with different bytes under the same asset
  name or reuse a published build version for a different update artifact.
