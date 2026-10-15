# Releasing

Tulma is distributed outside the Mac App Store. It runs servers through `npx`, `uvx` and `docker`, which the App Store sandbox does not allow. Releases are signed with a Developer ID certificate, notarized by Apple and published as a DMG on GitHub Releases. Gatekeeper then opens them without a warning.

`scripts/release.sh` does the whole job on your Mac. `.github/workflows/release.yml` runs the same script in CI when you push a tag.

## One-time setup

### 1. Developer ID certificate

You need a paid Apple Developer account. In **Xcode → Settings → Accounts → Manage Certificates**, add a **Developer ID Application** certificate. Check that it is in your keychain:

```sh
security find-identity -v -p codesigning | grep "Developer ID Application"
```

### 2. Notarization key

Create an App Store Connect API key at [App Store Connect → Users and Access → Integrations → Team Keys](https://appstoreconnect.apple.com/access/integrations/api) with the **Developer** role. Download `AuthKey_XXXXXXXXXX.p8`; Apple lets you download it only once. Note the **Key ID** and the **Issuer ID**.

Save the key in your keychain for local releases:

```sh
xcrun notarytool store-credentials tulma-notary \
  --key ~/Downloads/AuthKey_XXXXXXXXXX.p8 --key-id XXXXXXXXXX --issuer xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
```

An Apple ID with an [app-specific password](https://support.apple.com/102654) works too:

```sh
xcrun notarytool store-credentials tulma-notary --apple-id you@example.com --team-id LT347RVFTF
```

### 3. GitHub secrets (CI only)

Export the certificate with its private key: in **Keychain Access**, find "Developer ID Application: …", select the certificate together with its key, choose **File → Export Items…** and save it as `.p12` with a password. Then add the repository secrets (**Settings → Secrets and variables → Actions**):

```sh
gh secret set DEVELOPER_ID_P12 < <(base64 -i developer-id.p12)
gh secret set DEVELOPER_ID_P12_PASSWORD            # the .p12 password
gh secret set NOTARY_KEY_P8 < <(base64 -i AuthKey_XXXXXXXXXX.p8)
gh secret set NOTARY_KEY_ID                        # Key ID
gh secret set NOTARY_ISSUER_ID                     # Issuer ID
```

Delete the exported `.p12` afterwards.

## Releasing

CI publishes a release whenever `main` gets a version that has no tag yet:

```sh
scripts/bump-version.sh 0.2.0       # sets project.yml and core/Cargo.toml, updates Cargo.lock
git commit -am "chore: release 0.2.0"
git push                            # or merge a PR that contains the bump
```

The workflow then builds, signs and notarizes the DMG, creates the `v0.2.0` tag on that commit and publishes the GitHub release with generated notes. Pushes to `main` that keep the version only run a ten-second check on Linux and skip the macOS build.

Other ways to run it:

- **Push a tag** (`git tag v0.2.0 && git push origin v0.2.0`): releases that tag. It must match `MARKETING_VERSION`.
- **Manual run** (**Actions → release → Run workflow**): builds the DMG and keeps it as a workflow artifact without publishing.
- **Locally**: `scripts/release.sh` builds a signed, notarized DMG in `build/release`; `NOTARIZE=0 scripts/release.sh` only signs it.

The build number (`CFBundleVersion`) is the commit count, so every release gets a higher one.

## Troubleshooting

- **Notarization rejected.** The script prints Apple's log. A usual cause is a binary without the hardened runtime or a timestamp; the script checks both before it uploads.
- **"The signature of the binary is invalid"** after editing `Tulma.app` by hand: run the script again; do not modify a signed bundle.
- **Keychain prompts after the first Developer ID build.** Keychain items are tied to the code signature. Items a development build created ask for access once when a Developer ID build first reads them. Users who install a release never see this.
