---
name: publish-release
description: Publish a new nudge release end-to-end — version bump PR, v-tag, GitHub release with notes, and the CI pipeline that builds binaries/APK and updates Homebrew + AUR. Use when asked to release, publish, ship, or tag a new version. Encodes the exact merge-gate and notes-before-artifacts ordering, so no step needs rediscovering.
---
# Publish a nudge release

The release is driven by an annotated `v*` tag on `main`: pushing it triggers
`.github/workflows/release.yml`, which builds the relay (linux x86_64), the agent
(linux x86_64 + macOS aarch64), and the signed Android APK, checksums everything,
attaches it all to the tag's GitHub release, then updates the Homebrew tap
(`nuudge/homebrew-tap`) and the AUR package. Everything below is runnable
without manual intervention; steps are ordered — don't reorder 5 and 6
(notes before artifacts, see why in step 6).

## 0. Decide the version

Semver, pre-1.0 conventions: patch for fixes, minor for features or anything
touching stored data / schemas / wire shapes. Check the current version:

```sh
grep -m1 '^version' Cargo.toml
```

## 1. Bump on a branch

```sh
git checkout main && git pull --ff-only
git checkout -b release-X.Y.Z
```

Edit the `[package]` `version` in the root `Cargo.toml` (the ONLY version to
bump — `relay` is workspace-internal and Android versions derive from the tag).
Then refresh the lockfile and verify:

```sh
cargo check          # updates Cargo.lock's own version entry
cargo test           # full suite must be green before a release commit
```

Commit both files, matching repo style:

```sh
git add Cargo.toml Cargo.lock
git commit -m "chore: bump to X.Y.Z"   # add a short body naming the headline changes
```

## 2. PR and merge (the one wait in the flow)

```sh
git push -u origin release-X.Y.Z
gh pr create --title "chore: bump to X.Y.Z" --body "<one-line summary>"
```

Branch protection requires the CI checks; the repo has **auto-merge disabled**,
so `gh pr merge --auto` fails — poll instead. Merge state goes `BLOCKED` →
`CLEAN` when the `test` check finishes (~2–4 min):

```sh
gh pr view <N> --json mergeStateStatus -q .mergeStateStatus   # wait for CLEAN
gh pr merge <N> --squash --delete-branch
git checkout main && git pull --ff-only
```

## 3. Draft the release notes FIRST

Write them to a temp file before tagging (the tag push starts the build; having
notes ready lets step 6 happen inside the workflow's window). Structure used by
0.2.0: a one-line summary, `## Highlights` bullets, and `## Upgrade notes` for
anything behavioral — **always include an upgrade caveat when storage, schema,
or config changes** (e.g. 0.2.0's "quit running sessions before first launch").

## 4. Tag main

```sh
git tag -a vX.Y.Z -m "vX.Y.Z" <merge-commit>
git push origin vX.Y.Z        # THIS is the release trigger
```

## 5. Pre-create the release with the notes

```sh
gh release create vX.Y.Z --verify-tag --title "nudge X.Y.Z" --notes-file <notes.md>
```

Do this immediately after the tag push, before the workflow's `release` job
runs (it needs all builds first, so there are many minutes of slack).

## 6. Why notes-before-artifacts

The workflow's `softprops/action-gh-release` step ATTACHES files to an existing
release for the tag if one exists, but if it creates the release itself the
body is empty — and past releases shipped bodyless that way. Pre-creating with
`--verify-tag` claims the release with the notes; the workflow then only adds
artifacts. (Editing notes in afterwards with `gh release edit` also works, if
the ordering was missed.)

## 7. Verify (builds take ~10–20 min; don't block on them)

```sh
gh run list --workflow=release.yml --limit 1        # should be in_progress on vX.Y.Z
# later:
gh run view <run-id>                                # all jobs green
gh release view vX.Y.Z                              # binaries + .sha256 files attached
```

Confirm the downstream publishes: the Homebrew formula version
(`nuudge/homebrew-tap` repo, or `brew info nuudge/tap/nudge` after an update)
and the AUR package version. If a build job failed, fix and re-run the failed
jobs from the run page — the tag and release stay in place; artifacts attach on
re-run.

## Failure modes seen in practice

- `gh pr merge` right after `gh pr create` → refused with "requirements not
  met": that's the CI gate, not an error — wait for `CLEAN` (step 2).
- `gh pr merge --auto` → "Auto merge is not allowed for this repository".
- `gh release create` without `--verify-tag` can silently create a tag from
  the wrong ref if the tag push was forgotten — always `--verify-tag`.
- Tagging before the bump PR is merged releases the OLD version number baked
  into the binary; the tag must point at (or after) the bump commit on main.
