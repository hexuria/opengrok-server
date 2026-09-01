> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Fork box

> Provision a new box from an existing one. Send an `Idempotency-Key` header to make this call safe to retry after a lost response without creating a second billable fork.



## OpenAPI

````yaml openapi/box-v1.yaml POST /boxes/{boxId}/fork
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
  /boxes/{boxId}/fork:
    post:
      tags:
        - Box
      summary: Fork box
      description: >-
        Provision a new box from an existing one. Send an `Idempotency-Key`
        header to make this call safe to retry after a lost response without
        creating a second billable fork.
      operationId: fork
      parameters:
        - $ref: '#/components/parameters/BoxId'
        - $ref: '#/components/parameters/IdempotencyKey'
      requestBody:
        required: false
        content:
          application/json:
            schema:
              type: object
              properties:
                env:
                  type: object
                  additionalProperties:
                    type: string
                  description: >-
                    Replaces the env the fork would otherwise inherit from the
                    source box. Same validation rules as `CreateBoxRequest.env`.
                environment:
                  type: string
                  description: >-
                    Optionally pin the fork to a different named Box
                    environment. Omit to inherit the source box's environment.
                    Unknown names are rejected with `unknown_environment`.
                  examples:
                    - base
                    - customer-demos
                noEnv:
                  type: boolean
                  description: >-
                    Make the fork no-env (see `CreateBoxRequest.noEnv`). A fork
                    of a no-env box is always no-env regardless of this field.
                type:
                  type: string
                  enum:
                    - small
                    - default
                    - large
                  description: >-
                    Machine size for the fork. Omit to inherit the source box's
                    type. The source box is never modified. Shrinking is
                    rejected with `type_too_small` when the source's data would
                    not fit the smaller disk.
                ttlSeconds:
                  oneOf:
                    - type: integer
                      minimum: 1
                      maximum: 2592000
                    - type: 'null'
                  default: 3600
                  description: >-
                    Auto-stop for the fork, in seconds. Omit for the 1 hour
                    default; the fork does NOT inherit the source box's TTL, so
                    forking a Box that has auto-stop disabled still gives you a
                    fork that stops itself. `null` disables auto-stop, which
                    means nothing will ever stop this Box for you.
      responses:
        '202':
          description: Fork started. The response `id` is the new forked box id.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/BoxActionResponse'
              examples:
                forking:
                  value:
                    ok: true
                    type: box.forking
                    id: bx_abcdef23
                    status: forking
                    box:
                      id: bx_abcdef23
                      name: Box fork
                      state: provisioning
                      desktopAvailable: false
                      snapshotAvailable: false
        '401':
          $ref: '#/components/responses/Unauthorized'
        '402':
          $ref: '#/components/responses/PaymentRequired'
        '404':
          $ref: '#/components/responses/NotFound'
        '409':
          $ref: '#/components/responses/Conflict'
        '429':
          $ref: '#/components/responses/RateLimited'
components:
  parameters:
    BoxId:
      name: boxId
      in: path
      required: true
      schema:
        type: string
        pattern: ^bx_[23456789abcdefghjkmnpqrstuvwxyz]{8}$
      description: Public Box id returned by create/list/get box calls.
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
  schemas:
    BoxActionResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - id
            - status
          properties:
            type:
              type: string
              examples:
                - box.stopping
            id:
              type: string
            status:
              type: string
              examples:
                - archiving
            box:
              oneOf:
                - $ref: '#/components/schemas/Box'
                - type: 'null'
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
    NotFound:
      description: Resource not found.
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