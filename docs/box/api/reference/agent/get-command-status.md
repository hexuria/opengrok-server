> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Get command status



## OpenAPI

````yaml openapi/box-v1.yaml GET /boxes/{boxId}/commands/{processId}
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
  /boxes/{boxId}/commands/{processId}:
    get:
      tags:
        - Box
      summary: Get detached command status and logs
      operationId: commandStatus
      parameters:
        - $ref: '#/components/parameters/BoxId'
        - $ref: '#/components/parameters/ProcessId'
        - name: tailBytes
          in: query
          schema:
            type: integer
            minimum: 1
            maximum: 524288
          description: >-
            Cap each returned log to its last N bytes. Defaults to 524288 (512
            KiB).
      responses:
        '200':
          description: Process status and log tails.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/CommandStatusResponse'
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
    ProcessId:
      name: processId
      in: path
      required: true
      schema:
        type: integer
        minimum: 1
      description: Process id returned by a detached command start.
  schemas:
    CommandStatusResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - success
            - processId
            - status
            - running
            - exitCode
            - stdout
            - stderr
          properties:
            type:
              type: string
              const: command.status
            success:
              type: boolean
            processId:
              type: integer
            pid:
              type: integer
            status:
              type: string
              enum:
                - running
                - exited
                - lost
              description: >-
                lost: the Box agent restarted and forgot the process;
                running/exitCode are then a best-effort probe and the logs come
                from the on-disk files.
            known:
              type: boolean
              description: Whether the process is still tracked by the Box agent.
            running:
              type: boolean
            exitCode:
              type:
                - integer
                - 'null'
            signal:
              type:
                - string
                - 'null'
            command:
              type:
                - string
                - 'null'
            cwd:
              type:
                - string
                - 'null'
            startedAt:
              type:
                - string
                - 'null'
              format: date-time
            finishedAt:
              type:
                - string
                - 'null'
              format: date-time
            stdout:
              type: string
              description: Tail of the stdout log file.
            stderr:
              type: string
              description: Tail of the stderr log file.
            stdoutTruncated:
              type: boolean
            stderrTruncated:
              type: boolean
            logPath:
              type: string
            errLogPath:
              type: string
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