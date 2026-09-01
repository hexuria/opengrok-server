> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Execute Box command

> Runs the command synchronously by default (timeout configurable via `timeoutSeconds`, 600s cap). With `detached: true` the command starts in the background and a process id is returned immediately; poll `/boxes/{boxId}/commands/{processId}` for status and logs. Returns 400 invalid_timeout when `timeoutSeconds` is not an integer in 1-600. Returns 409 box_starting (retryable) while the Box is still provisioning -- wait until the Box state is ready before running commands. Command execution is never retried automatically: a 502 box_direct_failed means the command may already be running on the Box.



## OpenAPI

````yaml openapi/box-v1.yaml POST /boxes/{boxId}/commands
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
  /boxes/{boxId}/commands:
    post:
      tags:
        - Box
      summary: Execute a command in a Box
      description: >-
        Runs the command synchronously by default (timeout configurable via
        `timeoutSeconds`, 600s cap). With `detached: true` the command starts in
        the background and a process id is returned immediately; poll
        `/boxes/{boxId}/commands/{processId}` for status and logs. Returns 400
        invalid_timeout when `timeoutSeconds` is not an integer in 1-600.
        Returns 409 box_starting (retryable) while the Box is still provisioning
        -- wait until the Box state is ready before running commands. Command
        execution is never retried automatically: a 502 box_direct_failed means
        the command may already be running on the Box.
      operationId: command
      parameters:
        - $ref: '#/components/parameters/BoxId'
      requestBody:
        required: true
        content:
          application/json:
            schema:
              $ref: '#/components/schemas/CommandRequest'
      responses:
        '200':
          description: >-
            Command result (synchronous) or process start confirmation
            (detached).
          content:
            application/json:
              schema:
                oneOf:
                  - $ref: '#/components/schemas/CommandResponse'
                  - $ref: '#/components/schemas/CommandStartedResponse'
        '400':
          $ref: '#/components/responses/BadRequest'
        '401':
          $ref: '#/components/responses/Unauthorized'
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
    CommandRequest:
      type: object
      required:
        - command
      properties:
        command:
          type: string
        cwd:
          type: string
          description: Relative working directory inside the Box work directory.
        timeoutSeconds:
          type: integer
          minimum: 1
          maximum: 600
          default: 30
          description: >-
            Command timeout in seconds. Values outside 1-600 are rejected with a
            400 invalid_timeout error.
        detached:
          type: boolean
          default: false
          description: >-
            Start the command in the background and return a process id
            immediately instead of waiting for it to finish. Output goes to a
            log file on the Box; poll the status endpoint for it.
    CommandResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - success
            - exitCode
            - stdout
            - stderr
            - timedOut
          properties:
            type:
              type: string
              const: command.finished
            success:
              type: boolean
            exitCode:
              type:
                - integer
                - 'null'
            signal:
              type:
                - string
                - 'null'
            stdout:
              type: string
            stderr:
              type: string
            stdoutTruncated:
              type: boolean
            stderrTruncated:
              type: boolean
            timedOut:
              type: boolean
            cwd:
              type: string
            startedAt:
              type: string
              format: date-time
            finishedAt:
              type: string
              format: date-time
    CommandStartedResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - success
            - processId
            - pid
            - command
            - startedAt
          properties:
            type:
              type: string
              const: command.started
            success:
              type: boolean
            processId:
              type: integer
              description: Process id to poll with the command status endpoint.
            pid:
              type: integer
            command:
              type: string
            cwd:
              type: string
            startedAt:
              type: string
              format: date-time
            logPath:
              type: string
              description: Stdout log file on the Box (~/.ascii/processes/<pid>.log).
            errLogPath:
              type: string
              description: Stderr log file on the Box.
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