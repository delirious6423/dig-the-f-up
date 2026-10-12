# Releasing Dig-the-f-up

Status: the workflow exists and has **not been run on GitHub** yet. Nothing here has been signed, notarized or
published. Items marked OWNER need your Apple Developer account or a GitHub action from you.

## What a release produces

- `.github/workflows/release.yml` runs on any tag `v*` (macOS 14 runner, Apple Silicon).
- It checks that the tag equals `version` in `app/tauri.conf.json` (for example tag `v0.1.0` and version `0.1.0`).
- It builds `Dig-the-f-up.app` with `cargo tauri build --bundles app`, zips it with `ditto`, writes a SHA-256 file, and
  attaches both to a **draft** GitHub release. Nothing is published automatically.
- Zip names:
  - `Dig-the-f-up-<version>-macos-arm64.zip` when signing secrets exist.
  - `Dig-the-f-up-<version>-macos-arm64-UNSIGNED.zip` when they do not. The release notes say so.

## Owner checklist

### 1. Before tagging

- [ ] `app/tauri.conf.json` `version` is the release number. Update `Cargo.toml` versions to match if you bump them.
- [ ] `README.md` status section and `app/README.md` are accurate for this release.
- [ ] `NOTICE` lists every bundled component (see `docs/THIRD-PARTY.md`). Regenerate with
      `scripts/gen-third-party-licenses.sh` when dependencies change.
- [ ] CI (`ci.yml`) is green on `main`.
- [ ] `git tag v<version>` on the commit you want, then `git push origin v<version>`. (OWNER: you push tags.)

### 2. Unsigned release (no secrets)

The workflow works with no secrets. The zip is **not** signed or notarized, so Gatekeeper blocks the first launch.
Tell testers: right-click `Dig-the-f-up.app` and choose Open, then confirm. Or remove the quarantine flag after unzipping:

    xattr -dr com.apple.quarantine /Applications/Dig-the-f-up.app

### 3. Signed and notarized release (OWNER)

Needs a paid Apple Developer Program membership and a "Developer ID Application" certificate.

1. Export the certificate and private key from Keychain Access as a `.p12` with a password.
2. Base64-encode it: `base64 -i DeveloperID.p12 | pbcopy`.
3. Add repository secrets (Settings > Secrets and variables > Actions):

| Secret | Value |
| --- | --- |
| `APPLE_CERTIFICATE` | the base64 `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | the `.p12` password |
| `APPLE_SIGNING_IDENTITY` | e.g. `Developer ID Application: Your Name (TEAMID)` |
| `APPLE_ID` | your Apple ID email |
| `APPLE_PASSWORD` | an app-specific password from appleid.apple.com (not your Apple ID password) |
| `APPLE_TEAM_ID` | the 10-character team ID |

When the first three exist, the job signs. When `APPLE_ID`, `APPLE_PASSWORD` and `APPLE_TEAM_ID` also exist, it
notarizes and staples. The signed zip has no `UNSIGNED` in its name. Any secret that is missing is simply skipped.

Secrets are never printed, and they are passed only to the build step that needs them.

### 4. Publish

1. Open the draft under Releases on GitHub.
2. Check the two files and the `.sha256` file. Download the zip once and run `shasum -a 256 -c` on it.
3. Launch the app from the unzipped bundle and check: the status bar, a search, Help > Show Logs.
4. Edit the notes if needed, then click **Publish release**.

## Not verified yet

- The workflow has not run on GitHub. Expect to fix small issues on the first run (cache, tool install time).
- `cargo install tauri-cli` takes several minutes on a cold cache.
- Signing and notarization have never run. The `codesign` and `stapler` checks are written but untested.
- No `minimumSystemVersion` is set: it could not be verified from this machine.
- Intel Macs are not built (owner decision: Apple Silicon only).
