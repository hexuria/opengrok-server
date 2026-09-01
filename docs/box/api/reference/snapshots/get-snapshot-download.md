> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Get snapshot download

> Return time-limited signed URLs for every chunk needed to reconstruct the box filesystem at this snapshot (the full chain, base through this generation), plus the inventory. Download the chunks and reassemble them as described by `reconstruct`. The `box snapshot pull` CLI command does this for you.




## OpenAPI

````yaml openapi/box-v1.yaml GET /snapshots/{snapshotId}/download
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
  /snapshots/{snapshotId}/download:
    get:
      tags:
        - Box
      summary: Get snapshot download
      description: >
        Return time-limited signed URLs for every chunk needed to reconstruct
        the box filesystem at this snapshot (the full chain, base through this
        generation), plus the inventory. Download the chunks and reassemble them
        as described by `reconstruct`. The `box snapshot pull` CLI command does
        this for you.
      operationId: getSnapshotDownload
      parameters:
        - $ref: '#/components/parameters/SnapshotId'
      responses:
        '200':
          description: Signed chunk URLs for the snapshot chain.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/SnapshotDownloadResponse'
              examples:
                download:
                  value:
                    ok: true
                    type: snapshot.download
                    snapshotId: 7417be09-d419-4ae0-b3fc-7f04a5a71ef1
                    boxId: bx_23456789
                    kind: incremental
                    generation: 3
                    expiresInSeconds: 3600
                    reconstruct: >-
                      Download every chunk. For each snapshot in ascending
                      generation, concat its chunks by chunkIndex and pipe
                      through `zstd -d | tar -x`.
                    inventory:
                      r2Key: chains/4ced5b04/inventory-3.json.zst
                      signedUrl: >-
                        https://r2.example/chains/4ced5b04/inventory-3.json.zst?sig=redacted
                    chunks:
                      - snapshotId: 2db50582-716c-424c-817e-9495484f88dd
                        generation: 0
                        chunkIndex: 0
                        r2Key: chains/4ced5b04/snapshots/2db50582/chunks/00.tar.zst
                        sizeBytes: 209715200
                        sha256: >-
                          9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
                        signedUrl: >-
                          https://r2.example/chains/4ced5b04/snapshots/2db50582/chunks/00.tar.zst?sig=redacted
        '401':
          $ref: '#/components/responses/Unauthorized'
        '404':
          $ref: '#/components/responses/NotFound'
components:
  parameters:
    SnapshotId:
      name: snapshotId
      in: path
      required: true
      schema:
        type: string
        format: uuid
      description: Snapshot id returned by the snapshot list/latest calls.
  schemas:
    SnapshotDownloadResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - snapshotId
            - boxId
            - kind
            - generation
            - expiresInSeconds
            - reconstruct
            - chunks
          properties:
            type:
              type: string
              const: snapshot.download
            snapshotId:
              type: string
              format: uuid
            boxId:
              type: string
            kind:
              type: string
              enum:
                - base
                - incremental
                - legacy
            generation:
              type: integer
            expiresInSeconds:
              type: integer
              description: Lifetime of every `signedUrl` in this response.
            reconstruct:
              type: string
              description: >-
                Human-readable note on how to reassemble the chunks into a
                filesystem.
            inventory:
              oneOf:
                - type: object
                  required:
                    - r2Key
                    - signedUrl
                  properties:
                    r2Key:
                      type: string
                    signedUrl:
                      type: string
                      format: uri
                - type: 'null'
            chunks:
              type: array
              description: >-
                Every chunk across the chain (all generations up to and
                including this snapshot), ordered by `(generation, chunkIndex)`.
              items:
                $ref: '#/components/schemas/SnapshotChunk'
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
    SnapshotChunk:
      type: object
      required:
        - snapshotId
        - generation
        - chunkIndex
        - r2Key
        - sizeBytes
        - sha256
        - signedUrl
      properties:
        snapshotId:
          type: string
          format: uuid
        generation:
          type: integer
        chunkIndex:
          type: integer
        r2Key:
          type: string
        sizeBytes:
          type: integer
        sha256:
          type: string
        signedUrl:
          type: string
          format: uri
          description: Time-limited download URL (see `expiresInSeconds`).
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