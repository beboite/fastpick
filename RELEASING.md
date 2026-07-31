# Releasing

fastpick ships to two places, and they are not interchangeable. GitHub carries the signed
binaries that `fastpick --update` installs. crates.io carries the source that
`cargo install fastpick` builds. A version that exists on one and not the other is a version
somebody installs and cannot update, or reads release notes for and cannot download.

## The order

crates.io goes last, and only once the GitHub release is out of draft. Pushing a tag builds
the binaries and opens a **draft** release, which `releases/latest` skips, so nothing is
offered to anyone until a human has looked at the assets. Publishing to crates.io before
that point puts a version in the world with no binary behind it and no notes anybody has
read.

Nothing has to be remembered for this: `.github/workflows/publish.yml` runs on
`release: published`, so pressing Publish is what uploads the crate. The failure this guards
against is real and has happened: crates.io sat on 0.4.1 while GitHub still showed 0.4.0 as
the latest release, because the crate was pushed by hand and the release was left in draft.

## Steps

1. Bump `version` in `Cargo.toml`, then `cargo update -p fastpick` so the lockfile follows.
   `cargo publish --locked` refuses a lockfile that disagrees with the manifest.
2. Commit and push to `main`. CI has to be green: the release workflow does not re-run it.
3. Tag and push the tag:

   ```
   git tag -a v0.4.2 -m "v0.4.2"
   git push origin v0.4.2
   ```

   That triggers `.github/workflows/release.yml`, which builds one binary per platform,
   signs each with the project's minisign key, writes `SIGNATURES.json` and `SHA256SUMS.txt`,
   and opens the draft release with generated notes.
4. Read the draft. Check the assets are all there and the notes say what changed.
5. Press Publish. `publish.yml` then uploads to crates.io on its own.

## What each side needs

- GitHub: the `MINISIGN_SECRET_KEY` and `MINISIGN_PASSWORD` secrets. The release workflow
  fails up front when the key is missing rather than shipping binaries `--update` will
  refuse to install.
- crates.io: a trusted publisher, configured on the crate's Settings page under Trusted
  Publishing, naming this repository and `publish.yml`. That is what lets the workflow mint
  a short-lived token at run time, so no registry token has to live in the repository
  secrets. Without it the publish step fails and the crate has to go up by hand.

Publishing a release whose version crates.io already serves is a green skip, not a failure,
so re-publishing an old release is safe.
