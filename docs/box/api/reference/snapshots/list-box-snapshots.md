> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# List box snapshots

> List visible completed snapshots for one Box with uncapped cursor pagination.



## OpenAPI

````yaml openapi/box-v1.yaml GET /boxes/{boxId}/snapshots
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
  /boxes/{boxId}/snapshots:
    get:
      tags:
        - Box
      summary: List box snapshots
      description: >-
        List visible completed snapshots for one Box with uncapped cursor
        pagination.
      operationId: listBoxSnapshots
      parameters:
        - $ref: '#/components/parameters/BoxId'
        - $ref: '#/components/parameters/Limit'
        - $ref: '#/components/parameters/Cursor'
        - $ref: '#/components/parameters/Sort'
      responses:
        '200':
          description: Completed snapshots for this box.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/SnapshotListResponse'
        '400':
          $ref: '#/components/responses/BadRequest'
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
    Limit:
      name: limit
      in: query
      schema:
        type: integer
        minimum: 1
        maximum: 200
        default: 100
      description: Maximum items to return.
    Cursor:
      name: cursor
      in: query
      schema:
        type:
          - string
          - 'null'
      description: Opaque pagination cursor returned as `pageInfo.nextCursor`.
    Sort:
      name: sort
      in: query
      schema:
        type: string
        enum:
          - asc
          - desc
        default: desc
      description: Sort direction for cursor pagination.
  schemas:
    SnapshotListResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - snapshots
          properties:
            type:
              type: string
              const: snapshot.list
            snapshots:
              type: array
              items:
                $ref: '#/components/schemas/SnapshotSummary'
            pageInfo:
              $ref: '#/components/schemas/PageInfo'
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
    PageInfo:
      type: object
      required:
        - nextCursor
        - hasMore
        - limit
      properties:
        nextCursor:
          type:
            - string
            - 'null'
        hasMore:
          type: boolean
        limit:
          type: integer
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