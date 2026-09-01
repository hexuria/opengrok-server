> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Get snapshot file tree

> List the files and folders captured in a snapshot, with sizes. Returns a flat list of entries you can render as a tree.



## OpenAPI

````yaml openapi/box-v1.yaml GET /snapshots/{snapshotId}/tree
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
  /snapshots/{snapshotId}/tree:
    get:
      tags:
        - Box
      summary: Get snapshot file tree
      description: >-
        List the files and folders captured in a snapshot, with sizes. Returns a
        flat list of entries you can render as a tree.
      operationId: getSnapshotTree
      parameters:
        - $ref: '#/components/parameters/SnapshotId'
      responses:
        '200':
          description: Flat file/folder listing for the snapshot.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/SnapshotTreeResponse'
              examples:
                tree:
                  value:
                    ok: true
                    type: snapshot.tree
                    snapshotId: 7417be09-d419-4ae0-b3fc-7f04a5a71ef1
                    boxId: bx_23456789
                    generation: 3
                    treeAvailable: true
                    truncated: false
                    fileCount: 6781
                    totalSizeBytes: 458291
                    entries:
                      - path: src
                        kind: dir
                      - path: src/main.ts
                        kind: file
                        size: 1024
                legacy:
                  value:
                    ok: true
                    type: snapshot.tree
                    snapshotId: 7417be09-d419-4ae0-b3fc-7f04a5a71ef1
                    boxId: bx_23456789
                    generation: 0
                    treeAvailable: false
                    truncated: false
                    fileCount: 0
                    totalSizeBytes: 0
                    entries: []
                    reason: legacy_snapshot
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
    SnapshotTreeResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - snapshotId
            - boxId
            - generation
            - treeAvailable
            - truncated
            - fileCount
            - totalSizeBytes
            - entries
          properties:
            type:
              type: string
              const: snapshot.tree
            snapshotId:
              type: string
              format: uuid
            boxId:
              type: string
            generation:
              type: integer
            treeAvailable:
              type: boolean
              description: >-
                `false` for legacy snapshots or inventories too large to expand;
                see `reason`.
            truncated:
              type: boolean
              description: >-
                `true` when the file list was capped; not every entry is
                returned.
            fileCount:
              type: integer
              description: >-
                Number of your files in this snapshot (base-image system files
                excluded).
            totalSizeBytes:
              type: integer
              description: >-
                Total bytes of your data in this snapshot (base-image system
                files excluded).
            entries:
              type: array
              description: >-
                Exactly the files/dirs a resume or download returns, your data
                only; base-image system entries are not listed.
              items:
                $ref: '#/components/schemas/SnapshotTreeEntry'
            reason:
              type: string
              description: Why the tree is unavailable, when `treeAvailable` is `false`.
              examples:
                - legacy_snapshot
                - inventory_too_large
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
    SnapshotTreeEntry:
      type: object
      required:
        - path
        - kind
      properties:
        path:
          type: string
          description: >-
            Path relative to the snapshot root. Docker named volumes appear
            under `__dockervol__/`.
        kind:
          type: string
          enum:
            - file
            - dir
            - symlink
        size:
          type: integer
          description: File size in bytes. Omitted for directories.
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