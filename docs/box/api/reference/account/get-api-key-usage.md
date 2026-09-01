> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Get API key usage

> Get the 30-day request total and created Boxes and Agents for one API key.



## OpenAPI

````yaml openapi/box-v1.yaml GET /api-keys/{apiKeyId}/usage
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
  /api-keys/{apiKeyId}/usage:
    parameters:
      - name: apiKeyId
        in: path
        required: true
        description: API key ID returned by `GET /api-keys`.
        schema:
          type: string
    get:
      tags:
        - Box
      summary: Get API key usage
      description: >-
        Returns the key's request total for the 30-day UTC window and the Boxes
        and Agents it created that still exist, including Boxes that never got a
        Sandbox row. Works for revoked keys owned by the authenticated account.
      operationId: apiKeyUsage
      responses:
        '200':
          description: Usage and created resources for the key.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/ApiKeyUsageResponse'
              examples:
                usage:
                  value:
                    ok: true
                    type: api_key.usage
                    id: sak_123
                    name: Production worker
                    keyPrefix: box_live
                    keyLastFour: 9abc
                    sandboxId: null
                    createdAt: '2026-05-31T12:00:00Z'
                    lastUsedAt: '2026-08-25T09:30:00Z'
                    usage:
                      requests: 1842
                      windowDays: 30
                    resources:
                      total: 2
                      boxes: 1
                      agents: 1
                    createdResources:
                      - kind: box
                        id: bx_123
                        name: CI build
                        state: ready
                        createdAt: '2026-08-24T14:00:00Z'
                      - kind: agent
                        id: agent_456
                        name: Review
                        state: idle
                        createdAt: '2026-08-23T12:00:00Z'
        '401':
          $ref: '#/components/responses/Unauthorized'
        '404':
          $ref: '#/components/responses/NotFound'
components:
  schemas:
    ApiKeyUsageResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - $ref: '#/components/schemas/ApiKey'
        - type: object
          required:
            - createdResources
          properties:
            createdResources:
              type: array
              items:
                $ref: '#/components/schemas/ApiKeyCreatedResource'
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
    ApiKey:
      type: object
      required:
        - id
        - name
        - keyPrefix
        - keyLastFour
        - sandboxId
        - createdAt
        - lastUsedAt
        - usage
        - resources
      properties:
        id:
          type: string
          examples:
            - sak_123
        name:
          type: string
          examples:
            - Production worker
        keyPrefix:
          type: string
          examples:
            - box_live
        keyLastFour:
          type: string
          examples:
            - 9abc
        sandboxId:
          type:
            - string
            - 'null'
          description: >-
            Box ID for a platform-managed machine key, or null for a
            user-created key.
        createdAt:
          type: string
          format: date-time
        lastUsedAt:
          type:
            - string
            - 'null'
          format: date-time
        usage:
          $ref: '#/components/schemas/ApiKeyRequestUsage'
        resources:
          $ref: '#/components/schemas/ApiKeyResourceTotals'
    ApiKeyCreatedResource:
      type: object
      required:
        - kind
        - id
        - name
        - state
        - createdAt
      properties:
        kind:
          type: string
          enum:
            - box
            - agent
        id:
          type: string
        name:
          type: string
        state:
          type: string
        createdAt:
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
    ApiKeyRequestUsage:
      type: object
      required:
        - requests
        - windowDays
      properties:
        requests:
          type: integer
          minimum: 0
          description: >-
            Requests authenticated with this key during the current UTC day and
            the previous 29 UTC days.
          examples:
            - 1842
        windowDays:
          type: integer
          const: 30
    ApiKeyResourceTotals:
      type: object
      required:
        - total
        - boxes
        - agents
      properties:
        total:
          type: integer
          minimum: 0
        boxes:
          type: integer
          minimum: 0
        agents:
          type: integer
          minimum: 0
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