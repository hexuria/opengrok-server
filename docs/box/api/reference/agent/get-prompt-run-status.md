> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Get prompt run status

> Returns first-class status for a queued/running/finished prompt so clients do not infer completion from box state and events.



## OpenAPI

````yaml openapi/box-v1.yaml GET /boxes/{boxId}/prompts/{promptId}
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
  /boxes/{boxId}/prompts/{promptId}:
    get:
      tags:
        - Box
      summary: Get prompt run status
      description: >-
        Returns first-class status for a queued/running/finished prompt so
        clients do not infer completion from box state and events.
      operationId: promptRunStatus
      parameters:
        - $ref: '#/components/parameters/BoxId'
        - name: promptId
          in: path
          required: true
          schema:
            type: string
      responses:
        '200':
          description: Prompt run status.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/PromptRunResponse'
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
    PromptRunResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - id
            - promptRun
          properties:
            type:
              type: string
              const: prompt.run
            id:
              type: string
            promptRun:
              $ref: '#/components/schemas/PromptRun'
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
    PromptRun:
      type: object
      required:
        - id
        - promptId
        - boxId
        - status
        - done
      properties:
        id:
          type: string
        promptId:
          type: string
        boxId:
          type: string
        status:
          type: string
          enum:
            - sending
            - queued
            - running
            - finished
            - failed
        done:
          type: boolean
        createdAt:
          type:
            - string
            - 'null'
          format: date-time
        model:
          type:
            - string
            - 'null'
        reasoningEffort:
          type:
            - string
            - 'null'
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