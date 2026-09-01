> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Create box

> Provision a new cloud computer. Store the returned `box.id` with your product job/session record. Send an `Idempotency-Key` header to make this call safe to retry after a lost response without creating a second billable box.



## OpenAPI

````yaml openapi/box-v1.yaml POST /boxes
openapi: 3.1.0
info:
  title: Box Public API v1
  version: 1.0.0
  description: >
    Public JSON API for creating, operating, prompting, observing, and exposing
    Box sandboxes from backend services, CI jobs, hosted workers, and Box
    automation products.


    The v1 reference intentionally documents the developer integration surface
    only. Dashboard billing actions are not part of v1.
servers:
  - url: https://ascii.dev/api/box/v1
security:
  - BoxBearerAuth: []
tags:
  - name: Box
    description: >-
      Unified Box account, setup, lifecycle, prompting, event history, desktop
      access, and SSH operations.
paths:
  /boxes:
    post:
      tags:
        - Box
      summary: Create box
      description: >-
        Provision a new cloud computer. Store the returned `box.id` with your
        product job/session record. Send an `Idempotency-Key` header to make
        this call safe to retry after a lost response without creating a second
        billable box.
      operationId: create
      parameters:
        - $ref: '#/components/parameters/IdempotencyKey'
        - $ref: '#/components/parameters/OrgId'
        - $ref: '#/components/parameters/OrgHeader'
      requestBody:
        required: false
        content:
          application/json:
            schema:
              $ref: '#/components/schemas/CreateBoxRequest'
      responses:
        '202':
          description: Box accepted for provisioning.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/CreateBoxResponse'
              examples:
                provisioning:
                  value:
                    ok: true
                    type: box.created
                    status: provisioning
                    ttlSeconds: 3600
                    box:
                      id: bx_23456789
                      name: Box 2026-05-31 12:00
                      state: provisioning
                      url: null
                      ip: null
                      createdAt: '2026-05-31T12:00:00Z'
                      updatedAt: '2026-05-31T12:00:00Z'
                      archiveAfter: '2026-05-31T13:00:00Z'
                      desktopAvailable: false
                      desktopUrl: null
                      snapshotAvailable: false
                      snapshotCompletedAt: null
        '401':
          $ref: '#/components/responses/Unauthorized'
        '402':
          $ref: '#/components/responses/PaymentRequired'
        '409':
          $ref: '#/components/responses/Conflict'
        '429':
          $ref: '#/components/responses/RateLimited'
components:
  parameters:
    IdempotencyKey:
      name: Idempotency-Key
      in: header
      required: false
      schema:
        type: string
        maxLength: 255
      description: >-
        Optional exactly-once key for creating a box. Send your own opaque,
        account-unique value (a UUID) to make `POST /boxes` safe to retry when
        the response is lost (network timeout, 5xx): the first request creates
        the box and binds it to the key; every later request with the **same
        account, key, and request body** returns that same box instead of
        creating a second, billable one. Behavior: keys are retained for **24
        hours**; a concurrent or early retry while the first box is still being
        minted returns `409` `idempotency_in_progress` (retry shortly, same
        key); reusing a key with a **different body** returns `409`
        `idempotency_key_reused`; timeouts and 5xx are safe to retry with the
        same key; a create that fails before the box exists releases the key
        within ~2 minutes so a retry can create the box. Omit the header to keep
        the default (non-idempotent) behavior.
    OrgId:
      name: org
      in: query
      required: false
      schema:
        type: string
      description: >-
        Billing wallet for this request. A team id you belong to reads that
        team's limits / bills a create to that team. Your own account id is
        personal. Boxes, snapshots, and environments stay creator-private.
    OrgHeader:
      name: X-Box-Org
      in: header
      required: false
      schema:
        type: string
      description: Same as the `org` query parameter. Query wins when both are set.
  schemas:
    CreateBoxRequest:
      type: object
      description: Options for provisioning a new cloud computer.
      properties:
        type:
          type: string
          enum:
            - small
            - default
            - large
          default: default
          description: >-
            Machine size. `small` is 2 vCPUs / 4 GB RAM and consumes machine
            time at half rate; `default` is 4 vCPUs / 8 GB RAM; `large` is 8
            vCPUs / 16 GB RAM and consumes machine time twice as fast (see the
            Billing guide). A box keeps its type for life: stopping, resuming
            and forking all preserve it, and forks inherit the source box's
            type.
        ttlSeconds:
          oneOf:
            - type: integer
              minimum: 1
              maximum: 2592000
            - type: 'null'
          default: 3600
          description: >-
            Number of seconds before automatic archival. `null` disables
            auto-stop. The backend also accepts the string `infinite` for legacy
            compatibility; new clients should send null.
        env:
          type: object
          additionalProperties:
            type: string
          description: >-
            Per-box environment variables injected into the box's tool
            environment, on top of the account environment's variables (per-box
            values win on conflicts). Keys must match
            `[A-Za-z_][A-Za-z0-9_]{0,127}`; at most 100 variables and 64KB
            total. Reserved names (`ASCII_TOKEN`, `ASCII_API_URL`, `AGENT_ID`,
            `PRODUCT_MODE`, `ENVIRONMENT_ID`, `BOX_ID`, `SERVICE_PREVIEW_TOKEN`,
            `BOX_CLI_TOKEN`) are rejected with `invalid_env`. Forked boxes
            inherit the source box's env unless the fork request supplies its
            own `env`.
        environment:
          type: string
          default: base
          description: >-
            Name of the Box environment to attach to this box. Environments are
            managed in the Box dashboard and bundle the repositories, secrets,
            and credential toggles a box gets. Omit to use your default
            environment (`base` unless you changed it). Unknown names are
            rejected with `unknown_environment`. An environment marked "safe for
            third parties" passes nothing to the box, exactly like `noEnv`.
          examples:
            - base
            - customer-demos
        noEnv:
          type: boolean
          default: false
          description: >-
            Create a box with none of the secrets attached to your account (no
            environment variables, secret files, or credentials), confined to
            itself so it cannot act on your account or other boxes. For boxes
            you give to your own users. SSH, SCP, desktop, snapshots, and public
            URLs still work; pass `env` to give the box a secret of its own. A
            fork of a no-env box is always no-env. Equivalent to attaching an
            environment marked "safe for third parties".
        setupScript:
          type: string
          maxLength: 65536
          description: >-
            Shell script that runs on the box after it is ready. Ready means
            "ready to accept the user", not "setup done": the script starts in
            the background once provisioning completes and never blocks the box
            becoming usable. It runs as the box user via `bash`, with the box's
            environment applied, and its output goes to a log file on the box.
            Observe the outcome as `setupStatus` (pending/running/done/failed)
            and `setupError` on the box. Rejected with a 400
            `invalid_setup_script` error when it is not a string or exceeds
            64KB.
        org:
          type: string
          description: >-
            Bill this box to a team you belong to (the team's shared wallet).
            Your own account id means personal billing. Listing, snapshots, and
            environments stay yours — the org is a wallet, not a shared
            workspace. Takes precedence over `teamId` and over the `X-Box-Org` /
            `?org=` request scope.
        teamId:
          type: string
          description: Legacy alias for `org`. Ignored when `org` is also set.
        from:
          type: string
          description: >-
            Create the box from a named snapshot (saved with `POST
            /named-snapshots`, or `box snapshot <id> <name>` in the CLI). The
            box starts from that exact frozen state. Omitting `type` inherits
            the type the snapshot was saved from; env and no-env inherit from
            the snapshot's source box unless the request passes its own, with
            the same rules as forking.
      examples:
        - ttlSeconds: 3600
        - ttlSeconds: null
        - type: large
          ttlSeconds: 3600
        - ttlSeconds: 3600
          env:
            DATABASE_URL: postgres://user:pass@host:5432/app
            FEATURE_FLAG: '1'
        - ttlSeconds: null
          noEnv: true
    CreateBoxResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - status
            - ttlSeconds
            - box
          properties:
            type:
              type: string
              const: box.created
            status:
              type: string
              enum:
                - provisioning
            ttlSeconds:
              type:
                - integer
                - 'null'
            box:
              $ref: '#/components/schemas/Box'
    SuccessBase:
      type: object
      required:
        - ok
        - type
      properties:
        ok:
          type: boolean
          examples:
            - true
        type:
          type: string
          description: Stable success envelope discriminator added by v1.
    Box:
      type: object
      required:
        - id
        - name
        - state
        - desktopAvailable
        - snapshotAvailable
      properties:
        id:
          type: string
          pattern: ^bx_[23456789abcdefghjkmnpqrstuvwxyz]{8}$
          examples:
            - bx_23456789
        name:
          type: string
          examples:
            - Box 2026-05-31 12:00
        state:
          type: string
          enum:
            - init
            - provisioning
            - provisioned
            - cloning
            - ready
            - idle
            - running
            - archiving
            - archived
            - error
        type:
          type: string
          enum:
            - small
            - default
            - large
            - bare-metal
          description: >-
            Machine size this box was created with. Fixed for the life of the
            box.
        vcpu:
          type: integer
          description: vCPUs guaranteed by this box's type.
          examples:
            - 4
        memoryGB:
          type: integer
          description: RAM in GB guaranteed by this box's type.
          examples:
            - 8
        billingMultiplier:
          type: number
          description: >-
            Rate at which this box consumes machine time. 0.5 for `small`, 1 for
            `default`, 2 for `large`.
          examples:
            - 1
        url:
          type:
            - string
            - 'null'
          format: uri
          description: Machine URL when assigned.
        ip:
          type:
            - string
            - 'null'
          description: Machine IPv6 or IPv4 address when assigned.
        createdAt:
          type:
            - string
            - 'null'
          format: date-time
        updatedAt:
          type:
            - string
            - 'null'
          format: date-time
        archiveAfter:
          type:
            - string
            - 'null'
          format: date-time
          description: Automatic archival time, or null when auto-stop is disabled.
        desktopAvailable:
          type: boolean
        desktopUrl:
          type:
            - string
            - 'null'
          format: uri
          description: Secret-bearing desktop stream URL when available. Redact from logs.
        snapshotAvailable:
          type: boolean
        snapshotCompletedAt:
          type:
            - string
            - 'null'
          format: date-time
          description: >-
            Timestamp of the most recent successfully completed snapshot, or
            null.
        subdomain:
          type:
            - string
            - 'null'
          description: >-
            The box's stable three-word subdomain slug (e.g.
            "frazil-pneuma-rallye"), or null before one is assigned.
        lastSnapshotAttemptAt:
          type:
            - string
            - 'null'
          format: date-time
          description: >-
            Timestamp of the most recent snapshot attempt of any status (queued,
            in_progress, completed, failed, cancelled), or null. Use with
            snapshotCompletedAt to detect snapshots that keep failing.
        lastSnapshotStatus:
          type:
            - string
            - 'null'
          enum:
            - queued
            - in_progress
            - completed
            - failed
            - cancelled
            - null
          description: >-
            Status of the most recent snapshot attempt, or null if none. A value
            other than completed while snapshotCompletedAt stays stale indicates
            failing snapshots.
        setupStatus:
          type:
            - string
            - 'null'
          enum:
            - pending
            - running
            - done
            - failed
            - null
          description: >-
            Outcome of the create-time `setupScript`: `pending` (stored, not yet
            started), `running` (executing on the box in the background), `done`
            (exit code 0) or `failed` (non-zero exit, or the box lost track of
            the process). Null when the box was created without a setup script.
        setupError:
          type:
            - string
            - 'null'
          description: >-
            Short failure detail (exit code plus a stderr tail) when
            `setupStatus` is `failed`; while `pending`, may carry the last
            start/upload error from a retry in progress. Otherwise null.
        environment:
          type:
            - string
            - 'null'
          description: >-
            Name of the Box environment this box is running, or null if it is
            attached to none (a `noEnv` box, or one whose environment was
            deleted). A box freezes onto one environment version when it starts
            and keeps it for life, so this is what the box actually holds, not
            what the environment says today.
          examples:
            - base
        environmentVersion:
          type:
            - integer
            - 'null'
          description: >-
            Version number of `environment` that this box is pinned to. Compare
            it against the environment's latest version to see whether an
            upgrade is pending: a box below the latest is still running the
            older configuration until someone calls `POST
            /environments/{environmentId}/upgrade`.
          examples:
            - 3
    ErrorEnvelope:
      type: object
      required:
        - ok
        - type
        - status
        - code
        - message
        - error
        - requestId
      properties:
        ok:
          type: boolean
          examples:
            - false
        type:
          type: string
          examples:
            - box.error
        status:
          type: integer
          examples:
            - 409
        code:
          type: string
          examples:
            - provider_not_configured
        message:
          type: string
          examples:
            - Prompting is locked until Codex is configured on the Agents page.
        requestId:
          type: string
          examples:
            - req_01HX...
        error:
          type: object
          required:
            - code
            - message
            - status
          properties:
            code:
              type: string
            message:
              type: string
            status:
              type: integer
            details:
              type: object
              additionalProperties: true
  responses:
    Unauthorized:
      description: Missing or invalid bearer token.
      content:
        application/json:
          schema:
            $ref: '#/components/schemas/ErrorEnvelope'
          examples:
            unauthorized:
              value:
                ok: false
                type: box.error
                status: 401
                code: unauthorized
                message: Unauthorized
                error:
                  code: unauthorized
                  message: Unauthorized
                  status: 401
                requestId: req_01HX...
    PaymentRequired:
      description: >-
        Account cannot currently create or operate Boxes. The error body may
        include a dashboard billing URL, but billing actions are not part of the
        v1 API.
      content:
        application/json:
          schema:
            $ref: '#/components/schemas/ErrorEnvelope'
    Conflict:
      description: Request conflicts with current account or box state.
      content:
        application/json:
          schema:
            $ref: '#/components/schemas/ErrorEnvelope'
    RateLimited:
      description: >-
        Machine start or concurrent-box limit reached. Create, fork and resume
        each count as one machine start against your plan's per-minute rate,
        five times that per hour and three times the hourly rate per day
        (`rate_limited`, naming the window you hit). A box that would exceed
        your plan's concurrent-box cap is refused with `limit_reached`, or
        `member_limit_reached` when an organization owner has capped you below
        the plan.
      content:
        application/json:
          schema:
            $ref: '#/components/schemas/ErrorEnvelope'
  securitySchemes:
    BoxBearerAuth:
      type: http
      scheme: bearer
      bearerFormat: box_api_key
      description: >-
        Box bearer token in the form `box_...`. Service API keys authenticate
        Box operations.

````