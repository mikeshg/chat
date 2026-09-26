# Issue tracker: Gitea

Issues and specs for this repo live as issues on the self-hosted Gitea at `http://192.168.6.210:3000/xiefan/chat` (Gitea 1.25, API docs at `http://192.168.6.210:3000/api/swagger`). No Gitea CLI and no `jq` are installed: call the REST API with `curl`, and build JSON bodies / parse responses with `python`.

## Setup

```bash
API=http://192.168.6.210:3000/api/v1/repos/xiefan/chat
AUTH="Authorization: token $GITEA_TOKEN"
```

The repo is public, so reads work without a token. Writes need `GITEA_TOKEN` (a Gitea access token with `write:issue` scope). If it is unset, stop and ask the user to export it; never ask them to paste the token into chat.

All write calls take `-H "$AUTH" -H 'Content-Type: application/json'`.

## Conventions

- **Create an issue**:
  ```bash
  python -c 'import json,sys; print(json.dumps({"title": sys.argv[1], "body": sys.stdin.read()}))' "Title" <<'EOF' | curl -s -X POST "$API/issues" -H "$AUTH" -H 'Content-Type: application/json' -d @-
  Multi-line body here.
  EOF
  ```
- **Read an issue**: `curl -s "$API/issues/<n>"` for title, body, labels and state, plus `curl -s "$API/issues/<n>/comments"` for the thread.
- **List issues**: `curl -s "$API/issues?state=open&type=issues&limit=50"`, adding `&labels=<name>` to filter. Reduce with `python` to `{number, title, body, labels: [l.name], state}`.
- **Comment on an issue**: `POST $API/issues/<n>/comments` with `{"body": "..."}`.
- **Apply labels**: `POST $API/issues/<n>/labels` with `{"labels": ["needs-triage"]}` (label names are accepted). A label must already exist in the repo: list with `GET $API/labels`, create a missing one with `POST $API/labels` and `{"name": "...", "color": "#ededed"}`.
- **Remove a label**: `DELETE $API/issues/<n>/labels/<label-id>`, taking the numeric id from `GET $API/labels`.
- **Close**: comment first, then `PATCH $API/issues/<n>` with `{"state": "closed"}`.

## Pull requests as a triage surface

**PRs as a request surface: no.** _(Set to `yes` if this repo treats external PRs as feature requests; `/triage` reads this flag.)_

When set to `yes`, PRs run through the same labels and states as issues:

- **Read a PR**: `GET $API/pulls/<n>`, comments via `GET $API/issues/<n>/comments`, diff via `GET $API/pulls/<n>.diff`.
- **List external PRs for triage**: `GET $API/pulls?state=open`, then drop PRs whose `user.login` is the owner `xiefan` or a collaborator (`GET $API/collaborators`).
- **Comment / label / close**: the issue endpoints above work on PR numbers too.

Gitea shares one number space across issues and PRs: resolve a bare `#42` with `GET $API/issues/42` and check `pull_request` (non-null means it is a PR).

## When a skill says "publish to the issue tracker"

Create a Gitea issue.

## When a skill says "fetch the relevant ticket"

Read the issue and its comments (see above).

## Wayfinding operations

Used by `/wayfinder`. The **map** is a single issue with **child** issues as tickets.

- **Map**: an issue labelled `wayfinder:map`, holding the Notes / Decisions-so-far / Fog body.
- **Child ticket**: Gitea has no sub-issues. Add the child to a task list in the map body and put `Part of #<map>` at the top of the child body. Labels: `wayfinder:<type>` (`research`/`prototype`/`grilling`/`task`). Once claimed, the ticket is assigned to the driving dev.
- **Blocking**: Gitea's native issue dependencies (enabled on this repo). Add an edge with `POST $API/issues/<child>/dependencies` and `{"owner": "xiefan", "repo": "chat", "index": <blocker>}`. `GET $API/issues/<child>/dependencies` lists the blockers; a ticket is unblocked when every blocker is closed.
- **Frontier query**: list the map's open children (from its task list), drop any with an open blocker or an assignee; first in map order wins.
- **Claim**: `PATCH $API/issues/<n>` with `{"assignees": ["xiefan"]}`, the session's first write.
- **Resolve**: comment the answer, close the issue, then append a context pointer (gist + link) to the map's Decisions-so-far by `PATCH $API/issues/<map>` with the updated `body`.
