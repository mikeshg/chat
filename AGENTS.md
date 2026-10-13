## Agent skills

### Issue tracker

Issues live in the self-hosted Gitea repo `xiefan/chat`, driven via `curl` + REST API. See `docs/agents/issue-tracker.md`.

### Triage labels

Default five-role vocabulary (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: root `CONTEXT.md` + `docs/adr/`. See `docs/agents/domain.md`.

### Releasing a new client version

Clients update in-app from the latest GitHub Release of `xie-fan/chat` (see `docs/release-runners.md`).

Any change under `app/` must ship as a new client release; pushing `main` alone does not reach desktop users. `app/src-tauri/tauri.conf.json` is the source of truth for the version. Version bump: bug fixes and small tweaks bump the patch (`0.3.3` → `0.3.4`); new features bump the minor (`0.3.3` → `0.4.0`).

1. Bump `version` in `app/src-tauri/tauri.conf.json` (e.g. `0.2.1`), commit, merge to `main`.
2. `git tag -a v0.2.1 -m "<release notes>"` and push the tag. The tag must equal `v` + that version, or the draft job fails; the tag message becomes the release notes users see.
3. `.github/workflows/release.yml` reuses the ci test job, then builds on free GitHub runners (windows-latest: NSIS+sig; ubuntu-latest: Android APK; ubuntu-22.04: deb+AppImage+sigs), publishes the draft once all 7 assets are present; then the 「我」 page shows the new version. CI for PRs/main is `.github/workflows/ci.yml`.
