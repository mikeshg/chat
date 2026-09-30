## Agent skills

### Issue tracker

Issues live in the self-hosted Gitea repo `xiefan/chat`, driven via `curl` + REST API. See `docs/agents/issue-tracker.md`.

### Triage labels

Default five-role vocabulary (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: root `CONTEXT.md` + `docs/adr/`. See `docs/agents/domain.md`.

### Releasing a new client version

Clients update in-app from the latest Gitea Release (see `docs/release-runners.md`).

1. Bump `version` in `app/src-tauri/tauri.conf.json` (e.g. `0.2.1`), commit, push `main`.
2. `git tag -a v0.2.1 -m "<release notes>"` and push the tag. The tag must equal `v` + that version, or the workflow fails; the tag message becomes the release notes users see.
3. `.gitea/workflows/release.yml` builds on the `linux` (Ubuntu 192.168.6.212) and `windows` (192.168.6.206) runners, ~20 min; then the 「我」 page shows the new version.
