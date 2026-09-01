> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Save a named snapshot

> Freeze a box's current state under a name (`box snapshot <id> <name>` in the CLI). Answers `202` immediately with the snapshot in `saving`; a running box takes a fresh capture first, which can run minutes. Poll `GET /named-snapshots/{name}` until the status settles at `ready` or `failed`. Reusing one of your existing names replaces that snapshot's artifact once the new save is ready.



## OpenAPI

````yaml openapi/box-v1.yaml POST /named-snapshots
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
  /named-snapshots:
    post:
      tags:
        - Box
      summary: Save a named snapshot
      description: >-
        Freeze a box's current state under a name (`box snapshot <id> <name>` in
        the CLI). Answers `202` immediately with the snapshot in `saving`; a
        running box takes a fresh capture first, which can run minutes. Poll
        `GET /named-snapshots/{name}` until the status settles at `ready` or
        `failed`. Reusing one of your existing names replaces that snapshot's
        artifact once the new save is ready.
      operationId: saveNamedSnapshot
      requestBody:
        required: true
        content:
          application/json:
            schema:
              $ref: '#/components/schemas/NamedSnapshotSaveRequest'
      responses:
        '202':
          description: Save accepted and running in the background.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/NamedSnapshotSavingResponse'
        '400':
          $ref: '#/components/responses/BadRequest'
        '401':
          $ref: '#/components/responses/Unauthorized'
        '404':
          $ref: '#/components/responses/NotFound'
        '409':
          description: >-
            `named_snapshot_limit` when you already keep the maximum of 10 named
            snapshots; remove one first. `save_in_progress` when a save under
            this name is already running; wait for it to settle rather than
            retrying immediately.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/ErrorEnvelope'
components:
  schemas:
    NamedSnapshotSaveRequest:
      type: object
      required:
        - boxId
        - name
      properties:
        boxId:
          type: string
          description: The box whose current state to freeze.
        name:
          type: string
          pattern: ^[a-z0-9][a-z0-9-]{0,62}$
          description: >-
            Name to save under. 1-63 lowercase letters, digits, or dashes,
            starting with a letter or digit. Reusing one of your existing names
            replaces that snapshot's artifact. `latest`, `tree`, `pull`, `rm`,
            `save`, `current`, `self`, and `new` are reserved.
    NamedSnapshotSavingResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - status
            - snapshot
          properties:
            type:
              type: string
              const: snapshot.named.saving
            status:
              type: string
              const: saving
            snapshot:
              $ref: '#/components/schemas/NamedSnapshot'
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
    NamedSnapshot:
      type: object
      description: >-
        A named snapshot: a frozen copy of a box's disk at one moment, saved
        under a name you pick. Independent of the source box's later life — the
        box can change, stop, or be deleted and the named snapshot still
        deploys. Named snapshots never expire; re-saving a name replaces its
        artifact.
      required:
        - name
        - status
        - sourceBoxId
        - createdAt
      properties:
        name:
          type: string
          pattern: ^[a-z0-9][a-z0-9-]{0,62}$
          description: The user-chosen handle, unique within your account.
        status:
          type: string
          enum:
            - saving
            - ready
            - failed
          description: >-
            `saving` while the capture and pin are in flight (a live source box
            takes a fresh snapshot first, which can run minutes), `ready` when
            deployable, `failed` if the save did not complete (see `error`; save
            again to retry).
        error:
          type: string
          description: Failure reason. Only present when `status` is `failed`.
        sourceBoxId:
          type: string
          description: The box this snapshot was saved from (display only).
        snapshotId:
          type: string
          description: >-
            The frozen artifact behind the name. Present once `status` is
            `ready`. Accepted by `GET /api/box/snapshots/{snapshotId}/tree` to
            browse its files.
        type:
          type: string
          description: Box type the snapshot was saved from. Deploys default to it.
        sizeBytes:
          type: integer
          description: Restored content size of the frozen state, in bytes.
        createdAt:
          type: string
          format: date-time
  responses:
    BadRequest:
      description: Invalid request body or parameters.
      content:
        application/json:
          schema:
            $ref: '#/components/schemas/ErrorEnvelope'
          examples:
            invalid:
              value:
                ok: false
                type: box.error
                status: 400
                code: invalid_json
                message: Request body must be valid JSON.
                error:
                  code: invalid_json
                  message: Request body must be valid JSON.
                  status: 400
                requestId: req_01HX...
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
    NotFound:
      description: Resource not found.
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