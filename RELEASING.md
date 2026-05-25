# Releasing sift

How to cut a new release. The whole flow is driven by GitHub Actions
(`.github/workflows/release.yml`) — you tag, the CI builds, the CI publishes.

## One-time setup

- Workflow has `permissions: contents: write`, so the default `GITHUB_TOKEN`
  is enough to create the release. No PAT needed.
- ARM Linux runs on the free `ubuntu-24.04-arm` runner (public repos only).
- Intel macOS uses `macos-13`; Apple Silicon uses `macos-latest`.

## Cutting a release

1. Bump `version` in `Cargo.toml`.
2. Run `cargo build --release` locally once to refresh `Cargo.lock`.
3. Commit, push:
   ```bash
   git commit -am "chore: release v0.1.0"
   git push
   ```
4. Tag and push the tag:
   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```
5. Watch the **release** workflow on Actions. It will:
   - Build `sift` for 4 targets in parallel
   - Package each as `sift-<tag>-<target>.tar.gz` + `.sha256`
   - Create a GitHub Release with auto-generated notes and all artifacts

That's it. `scripts/install.sh` (and the `curl | bash` one-liner in the README)
will pick up the new release automatically because it resolves
`/releases/latest` by default.

## Manual re-run

If a build flaked, re-run individual jobs from the Actions tab. If the whole
release needs to be rebuilt against an existing tag:

- Actions → **release** → **Run workflow** → enter the existing tag (e.g. `v0.1.0`).

## Artifact layout

Each archive contains:

```
sift-v0.1.0-x86_64-unknown-linux-gnu/
├── sift          # the binary
├── README.md
├── README_cn.md
└── LICENSE       # if present
```

Checksum file (`sift-v0.1.0-<target>.tar.gz.sha256`) sits next to the archive
so the installer can verify it without a separate manifest.

## Targets currently built

| Target | Runner |
|---|---|
| `x86_64-unknown-linux-gnu` | `ubuntu-latest` |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` |
| `x86_64-apple-darwin` | `macos-13` |
| `aarch64-apple-darwin` | `macos-latest` |

To add Windows or `musl` Linux later: extend the matrix in
`.github/workflows/release.yml` and add the corresponding target to the
detection block in `scripts/install.sh`.
