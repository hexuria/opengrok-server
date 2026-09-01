# box.ascii.dev Public API — local copy

Fetched **1 Sep 2026** from [docs.ascii.dev/box/api/v1](https://docs.ascii.dev/box/api/v1)
(markdown via `https://docs.ascii.dev/box/api/v1.md` and the `/box/api/` entries in
[llms.txt](https://docs.ascii.dev/llms.txt)). These pages are **ASCII's**, not ours. Keep them
so a box bug can be checked against the vendor contract without leaving the repo. If a local
page and the live site disagree, the live site wins — re-fetch.

| File | What |
|---|---|
| [`api/v1.md`](api/v1.md) | Overview: base URL, auth, envelope, errors, lifecycle, recommended flow |
| [`openapi/box-v1.yaml`](openapi/box-v1.yaml) | OpenAPI 3.1 spec (includes `POST /boxes/{boxId}/host`, which has no reference `.md`) |
| [`llms.txt`](llms.txt) | The catalog used for this fetch (full Box docs index, not only API) |

Base URL: `https://ascii.dev/api/box/v1`. Auth: `Authorization: Bearer $BOX_API_KEY`.

Our research notes (verdict, gaps, how we consume this) stay in
[`docs/research/sandbox-box-ascii-dev.md`](../research/sandbox-box-ascii-dev.md). This folder is
the vendor pages. The typed Rust client is `opengrok_box::ascii::Client` in
`crates/opengrok-box/src/ascii/` — shapes transcribed from the OpenAPI spec here; `AsciiBoxes`
is the `Computer` adapter on top.

## Reference pages in this tree

### Account

- [Get current Box user](api/reference/account/get-current-box-user.md) — `GET /me`
- [List organizations](api/reference/account/list-organizations.md)
- [Get Box limits](api/reference/account/get-box-limits.md) — `GET /limits`
- [Get account data-retention policy](api/reference/account/get-account-data-retention-policy.md) — `GET /account/data-retention`
- [Update account data-retention policy](api/reference/account/update-account-data-retention-policy.md) — `PATCH /account/data-retention`
- [Get deletion operation](api/reference/account/get-deletion-operation.md) — `GET /deletion-operations/{operationId}`
- [List GitHub repositories available to Box](api/reference/account/list-github-repositories-available-to-box.md) — `GET /repos`
- [Select repository for Boxes](api/reference/account/select-repository-for-boxes.md) — `POST /repos`
- [List API keys](api/reference/account/list-api-keys.md) — `GET /api-keys`
- [Get API key usage](api/reference/account/get-api-key-usage.md) — `GET /api-keys/{apiKeyId}/usage`
- [Get Box secrets setup](api/reference/account/get-box-secrets-setup.md) — `GET /secrets`
- [Update Box secrets setup](api/reference/account/update-box-secrets-setup.md) — `POST /secrets`
- [List webhooks](api/reference/account/list-webhooks.md) — `GET /webhooks`
- [Create webhook](api/reference/account/create-webhook.md) — `POST /webhooks`
- [Get webhook](api/reference/account/get-webhook.md) — `GET /webhooks/{webhookId}`
- [Update webhook](api/reference/account/update-webhook.md) — `PATCH /webhooks/{webhookId}`
- [Delete webhook](api/reference/account/delete-webhook.md) — `DELETE /webhooks/{webhookId}`
- [Rotate webhook signing secret](api/reference/account/rotate-webhook-signing-secret.md) — `POST /webhooks/{webhookId}/rotate`

### Boxes (lifecycle)

- [List boxes](api/reference/boxes/list-boxes.md) — `GET /boxes`
- [Create box](api/reference/boxes/create-box.md) — `POST /boxes`
- [Get box](api/reference/boxes/get-box.md) — `GET /boxes/{boxId}`
- [Update box](api/reference/boxes/update-box.md) — `PATCH /boxes/{boxId}`
- [Stop and archive box](api/reference/boxes/stop-and-archive-box.md) — `POST /boxes/{boxId}/stop`
- [Resume box](api/reference/boxes/resume-box.md) — `POST /boxes/{boxId}/resume`
- [Fork box](api/reference/boxes/fork-box.md) — `POST /boxes/{boxId}/fork`
- [Permanently delete Box data](api/reference/boxes/permanently-delete-box-data.md) — `DELETE /boxes/{boxId}`

### Agent (exec, files, desktop)

What `opengrok-box` actually calls.

- [Execute Box command](api/reference/agent/execute-box-command.md) — `POST /boxes/{boxId}/commands`
- [Get command status](api/reference/agent/get-command-status.md) — `GET /boxes/{boxId}/commands/{processId}`
- [Read Box file](api/reference/agent/read-box-file.md) — `GET /boxes/{boxId}/files`
- [Write Box file](api/reference/agent/write-box-file.md) — `PUT /boxes/{boxId}/files`
- [Download Box artifact](api/reference/agent/download-box-artifact.md) — `GET /boxes/{boxId}/artifacts`
- [Interrupt running work](api/reference/agent/interrupt-running-agent-work.md) — `POST /boxes/{boxId}/interrupt`
- [Get desktop streaming URL](api/reference/agent/get-desktop-streaming-url.md) — `POST /boxes/{boxId}/desktop`
- [Configure box SSH key](api/reference/agent/configure-box-ssh-key.md) — `POST /boxes/{boxId}/sshkey`
- [List box events](api/reference/agent/list-box-events.md) — `GET /boxes/{boxId}/events`
- [Prompt Box](api/reference/agent/prompt-box-agent.md) — `POST /boxes/{boxId}/prompt` (ASCII's in-box agent, not our harness)
- [Get prompt run status](api/reference/agent/get-prompt-run-status.md) — `GET /boxes/{boxId}/prompts/{promptId}`

`POST /boxes/{boxId}/host` (preview URLs) is in the OpenAPI spec only; the prose is still
[docs.ascii.dev/box/hosting](https://docs.ascii.dev/box/hosting.md).

### Snapshots

- [List snapshots](api/reference/snapshots/list-snapshots.md) — `GET /snapshots`
- [List box snapshots](api/reference/snapshots/list-box-snapshots.md) — `GET /boxes/{boxId}/snapshots`
- [Get latest box snapshot](api/reference/snapshots/get-latest-box-snapshot.md) — `GET /boxes/{boxId}/snapshots/latest`
- [Get snapshot file tree](api/reference/snapshots/get-snapshot-tree.md) — `GET /snapshots/{snapshotId}/tree`
- [Download a file or folder from a snapshot](api/reference/snapshots/get-snapshot-file.md) — `GET /snapshots/{snapshotId}/files`
- [Get snapshot download](api/reference/snapshots/get-snapshot-download.md) — `GET /snapshots/{snapshotId}/download`
- [Permanently delete snapshot data](api/reference/snapshots/permanently-delete-snapshot-data.md) — `DELETE /snapshots/{snapshotId}`
- [List named snapshots](api/reference/snapshots/list-named-snapshots.md) — `GET /named-snapshots`
- [Save a named snapshot](api/reference/snapshots/save-named-snapshot.md) — `POST /named-snapshots`
- [Get a named snapshot](api/reference/snapshots/get-named-snapshot.md) — `GET /named-snapshots/{name}`
- [Delete a named snapshot](api/reference/snapshots/delete-named-snapshot.md) — `DELETE /named-snapshots/{name}`

### Environments

- [List Box environments](api/reference/environments/list-box-environments.md) — `GET /environments`
- [Create a Box environment](api/reference/environments/create-box-environment.md) — `POST /environments`
- [Update a Box environment](api/reference/environments/update-box-environment.md) — `PATCH /environments/{environmentId}`
- [Delete a Box environment](api/reference/environments/delete-box-environment.md) — `DELETE /environments/{environmentId}`
- [Upgrade boxes to an environment's latest version](api/reference/environments/upgrade-box-environment.md) — `POST /environments/{environmentId}/upgrade`
- [Set environment variable](api/reference/environments/set-environment-var.md)
- [Remove environment variable](api/reference/environments/delete-environment-var.md)
- [Write environment secret file](api/reference/environments/set-environment-secret-file.md)
- [Remove environment secret file](api/reference/environments/delete-environment-secret-file.md)
- [Add environment repository](api/reference/environments/add-environment-repo.md)
- [Remove environment repository](api/reference/environments/delete-environment-repo.md)

## Refresh

Pages are Mintlify markdown (some OpenAPI fences, `<Warning>` / `<Note>` tags). To refresh:

```sh
# from repo root — re-download every /box/api/*.md listed in llms.txt plus the OpenAPI spec
curl -fsS https://docs.ascii.dev/llms.txt -o docs/box/llms.txt
# then the same fetch loop that populated api/ and openapi/
```

Do not rewrite these pages to match our code. If our client and the spec disagree, fix the client
or record the gap in `docs/research/sandbox-box-ascii-dev.md`.
