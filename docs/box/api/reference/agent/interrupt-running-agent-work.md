> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Interrupt running work



## OpenAPI

````yaml openapi/box-v1.yaml POST /boxes/{boxId}/interrupt
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
  /boxes/{boxId}/interrupt:
    post:
      tags:
        - Box
      summary: Interrupt running work
      operationId: interrupt
      parameters:
        - $ref: '#/components/parameters/BoxId'
      responses:
        '200':
          description: Interrupt requested.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/BoxActionResponse'
              examples:
                interrupted:
                  value:
                    ok: true
                    type: box.interrupted
                    id: bx_23456789
                    status: interrupted
        '401':
          $ref: '#/components/responses/Unauthorized'
        '402':
          $ref: '#/components/responses/PaymentRequired'
        '404':
          $ref: '#/components/responses/NotFound'
        '409':
          $ref: '#/components/responses/Conflict'
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
  securitySchemes:
    BoxBearerAuth:
      type: http
      scheme: bearer
      bearerFormat: box_api_key
      description: >-
        Box bearer token in the form `box_...`. Service API keys authenticate
        Box operations.

````