> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Get latest box snapshot

> Return the most recent completed snapshot for this box, or `null` if it has none.



## OpenAPI

````yaml openapi/box-v1.yaml GET /boxes/{boxId}/snapshots/latest
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
  /boxes/{boxId}/snapshots/latest:
    get:
      tags:
        - Box
      summary: Get latest box snapshot
      description: >-
        Return the most recent completed snapshot for this box, or `null` if it
        has none.
      operationId: getLatestBoxSnapshot
      parameters:
        - $ref: '#/components/parameters/BoxId'
      responses:
        '200':
          description: Most recent completed snapshot, or `null`.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/SnapshotLatestResponse'
              examples:
                latest:
                  value:
                    ok: true
                    type: snapshot.latest
                    snapshot:
                      id: 7417be09-d419-4ae0-b3fc-7f04a5a71ef1
                      boxId: bx_23456789
                      status: completed
                      kind: incremental
                      generation: 3
                      chainId: 4ced5b04-d2cb-4ec3-b127-3b3ed836cab5
                      createdAt: '2026-06-24T06:24:00Z'
                      completedAt: '2026-06-24T06:24:50Z'
                      sizeBytes: 18874368
                      fileCount: 6781
                none:
                  value:
                    ok: true
                    type: snapshot.latest
                    snapshot: null
        '401':
          $ref: '#/components/responses/Unauthorized'
        '404':
          $ref: '#/components/responses/NotFound'
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
    SnapshotLatestResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - snapshot
          properties:
            type:
              type: string
              const: snapshot.latest
            snapshot:
              oneOf:
                - $ref: '#/components/schemas/SnapshotSummary'
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
    SnapshotSummary:
      type: object
      required:
        - id
        - boxId
        - status
        - generation
        - createdAt
        - sizeBytes
        - fileCount
      properties:
        id:
          type: string
          format: uuid
        boxId:
          type: string
          description: Public Box id this snapshot belongs to.
        status:
          type: string
          enum:
            - completed
        kind:
          type:
            - string
            - 'null'
          enum:
            - base
            - incremental
            - null
          description: >-
            `base` (full) or `incremental` (delta on a base). `null` for legacy
            snapshots.
        generation:
          type: integer
          description: Position in the incremental chain (0 = base).
        chainId:
          type:
            - string
            - 'null'
          format: uuid
        createdAt:
          type: string
          format: date-time
        completedAt:
          type:
            - string
            - 'null'
          format: date-time
        sizeBytes:
          type: integer
          description: Bytes this snapshot added (its delta), not the full restored size.
        fileCount:
          type: integer
          description: >-
            Inventory entries alive in the chain at this generation (includes
            base-image system entries).
        contentSizeBytes:
          type:
            - integer
            - 'null'
          description: >-
            Total bytes of your data restored by this snapshot (what
            resume/download returns; base image excluded). `null` on legacy
            snapshots.
        contentFileCount:
          type:
            - integer
            - 'null'
          description: >-
            Number of your files restored by this snapshot (base image
            excluded). `null` on legacy snapshots.
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