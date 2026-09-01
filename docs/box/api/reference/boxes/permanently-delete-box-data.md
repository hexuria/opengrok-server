> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Permanently delete Box data

> Confirm and accept irreversible background deletion of a Box.



## OpenAPI

````yaml openapi/box-v1.yaml DELETE /boxes/{boxId}
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
  /boxes/{boxId}:
    parameters:
      - $ref: '#/components/parameters/BoxId'
    delete:
      tags:
        - Box
      summary: Permanently delete Box data
      description: >-
        Accept irreversible background deletion of the Box machine, snapshots,
        and content-bearing records. This is not archive: the Box cannot be
        resumed. Named snapshots and other shared artifacts remain independent.
        Poll the returned operation until `completed`.
      operationId: deleteBox
      parameters:
        - $ref: '#/components/parameters/ConfirmDelete'
      responses:
        '202':
          description: Deletion confirmed and accepted for background processing.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/DeletionOperationResponse'
        '401':
          $ref: '#/components/responses/Unauthorized'
        '403':
          $ref: '#/components/responses/Forbidden'
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
    ConfirmDelete:
      name: X-Ascii-Confirm-Delete
      in: header
      required: true
      schema:
        type: string
      description: >-
        Must exactly equal the target `boxId` or `snapshotId`. A missing or
        mismatched value returns `409` without accepting deletion.
  schemas:
    DeletionOperationResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - operation
          properties:
            type:
              type: string
              enum:
                - deletion.operation
                - box.deleting
                - snapshot.deleting
            operation:
              $ref: '#/components/schemas/DeletionOperation'
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
    DeletionOperation:
      type: object
      required:
        - id
        - kind
        - targetId
        - reason
        - status
        - attemptCount
        - requestedAt
        - completedAt
      properties:
        id:
          type: string
          pattern: ^bdop_[a-f0-9]{32}$
        kind:
          type: string
          enum:
            - box
            - snapshot
        targetId:
          type: string
        reason:
          type: string
          enum:
            - explicit
            - zdr
            - account
        status:
          type: string
          enum:
            - pending
            - processing
            - blocked
            - completed
        attemptCount:
          type: integer
          minimum: 0
        requestedAt:
          type: string
          format: date-time
        completedAt:
          type:
            - string
            - 'null'
          format: date-time
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
    Forbidden:
      description: Authenticated token is not allowed to perform this action.
      content:
        application/json:
          schema:
            $ref: '#/components/schemas/ErrorEnvelope'
          examples:
            forbidden:
              value:
                ok: false
                type: box.error
                status: 403
                code: forbidden
                message: Forbidden
                error:
                  code: forbidden
                  message: Forbidden
                  status: 403
                requestId: req_01HX...
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