> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Download a file or folder from a snapshot

> Stream a single file's bytes, or a folder subtree as a tar archive, directly out of a snapshot, the box can be stopped or archived; the machine is never contacted. Use `GET /snapshots/{snapshotId}/tree` to list paths, then pass one here. Paths are relative to the snapshot root (same space as `tree` entries; docker volumes under `__dockervol__/`). An empty or `/` path downloads the whole snapshot as a tar. Folder responses set `X-Snapshot-File-Count`, `X-Snapshot-Total-Size-Bytes` and `X-Snapshot-Skipped-Base-Image-Files` headers; both response shapes set `X-Snapshot-Entry-Kind` (`file` or `dir`).



## OpenAPI

````yaml openapi/box-v1.yaml GET /snapshots/{snapshotId}/files
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
  /snapshots/{snapshotId}/files:
    get:
      tags:
        - Box
      summary: Download a file or folder from a snapshot
      description: >-
        Stream a single file's bytes, or a folder subtree as a tar archive,
        directly out of a snapshot, the box can be stopped or archived; the
        machine is never contacted. Use `GET /snapshots/{snapshotId}/tree` to
        list paths, then pass one here. Paths are relative to the snapshot root
        (same space as `tree` entries; docker volumes under `__dockervol__/`).
        An empty or `/` path downloads the whole snapshot as a tar. Folder
        responses set `X-Snapshot-File-Count`, `X-Snapshot-Total-Size-Bytes` and
        `X-Snapshot-Skipped-Base-Image-Files` headers; both response shapes set
        `X-Snapshot-Entry-Kind` (`file` or `dir`).
      operationId: getSnapshotFile
      parameters:
        - $ref: '#/components/parameters/SnapshotId'
        - name: path
          in: query
          required: false
          schema:
            type: string
          description: >-
            File or folder path inside the snapshot. Empty for the whole
            snapshot.
      responses:
        '200':
          description: >-
            File bytes (`application/octet-stream`) or folder tar
            (`application/x-tar`).
          content:
            application/octet-stream:
              schema:
                type: string
                format: binary
            application/x-tar:
              schema:
                type: string
                format: binary
        '401':
          $ref: '#/components/responses/Unauthorized'
        '404':
          $ref: '#/components/responses/NotFound'
        '409':
          description: >-
            Path has no downloadable bytes, one of `legacy_snapshot`
            (pre-inventory snapshot), `snapshot_not_indexed` (content captured
            before the indexed snapshot format; take a new snapshot or use the
            download bundle), `base_image_file` (stock image file, not stored in
            snapshots), or `is_symlink` (request the symlink's target instead).
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/ErrorEnvelope'
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
  schemas:
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
  securitySchemes:
    BoxBearerAuth:
      type: http
      scheme: bearer
      bearerFormat: box_api_key
      description: >-
        Box bearer token in the form `box_...`. Service API keys authenticate
        Box operations.

````