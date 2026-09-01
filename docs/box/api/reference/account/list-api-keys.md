> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# List API keys

> Lists API key metadata only. Raw key secrets are not returned after creation/rotation. Results are scoped to the authenticated Box account.



## OpenAPI

````yaml openapi/box-v1.yaml GET /api-keys
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
  /api-keys:
    get:
      tags:
        - Box
      summary: List API keys
      description: >-
        Lists API key metadata only. Raw key secrets are not returned after
        creation/rotation. Results are scoped to the authenticated Box account.
      operationId: apiKeys
      responses:
        '200':
          description: API key metadata.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/ApiKeysResponse'
              examples:
                keys:
                  value:
                    ok: true
                    type: api_key.list
                    apiKeys:
                      - id: sak_123
                        name: Production worker
                        keyPrefix: box_live
                        keyLastFour: 9abc
                        sandboxId: null
                        createdAt: '2026-05-31T12:00:00Z'
                        lastUsedAt: null
                        usage:
                          requests: 1842
                          windowDays: 30
                        resources:
                          total: 3
                          boxes: 2
                          agents: 1
        '401':
          $ref: '#/components/responses/Unauthorized'
components:
  schemas:
    ApiKeysResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - apiKeys
          properties:
            apiKeys:
              type: array
              items:
                $ref: '#/components/schemas/ApiKey'
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
  securitySchemes:
    BoxBearerAuth:
      type: http
      scheme: bearer
      bearerFormat: box_api_key
      description: >-
        Box bearer token in the form `box_...`. Service API keys authenticate
        Box operations.

````