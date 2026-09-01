> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Create webhook

> Create an account-wide Box lifecycle webhook endpoint.



## OpenAPI

````yaml openapi/box-v1.yaml POST /webhooks
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
  /webhooks:
    post:
      tags:
        - Box
      summary: Create webhook
      description: >
        Registers an account-wide endpoint for Box lifecycle events. The
        endpoint must use HTTPS on port 443 and resolve only to public
        addresses. Redirects are not followed during delivery.


        The signing secret is returned only in this response. An account can
        register at most 10 unique endpoint URLs.
      operationId: createWebhook
      requestBody:
        required: true
        content:
          application/json:
            schema:
              $ref: '#/components/schemas/WebhookCreateRequest'
            examples:
              readyAndError:
                value:
                  name: Production automation
                  url: https://example.com/hooks/box
                  events:
                    - box.ready
                    - box.error
      responses:
        '201':
          description: Webhook created. Store the one-time signing secret now.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/WebhookSecretResponse'
              examples:
                created:
                  value:
                    ok: true
                    type: webhook.created
                    webhook:
                      id: wh_0123456789abcdef01234567
                      name: Production automation
                      url: https://example.com/hooks/box
                      events:
                        - box.ready
                        - box.error
                      createdAt: '2026-08-11T12:00:00Z'
                      updatedAt: '2026-08-11T12:00:00Z'
                    secret: >-
                      whsec_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
        '400':
          $ref: '#/components/responses/BadRequest'
        '401':
          $ref: '#/components/responses/Unauthorized'
        '409':
          $ref: '#/components/responses/Conflict'
components:
  schemas:
    WebhookCreateRequest:
      type: object
      required:
        - url
        - events
      properties:
        name:
          type:
            - string
            - 'null'
          minLength: 1
          maxLength: 100
        url:
          type: string
          format: uri
        events:
          type: array
          minItems: 1
          uniqueItems: true
          items:
            $ref: '#/components/schemas/WebhookEventType'
    WebhookSecretResponse:
      allOf:
        - $ref: '#/components/schemas/WebhookResponse'
        - type: object
          required:
            - secret
          properties:
            secret:
              type: string
              pattern: ^whsec_[a-f0-9]{64}$
              description: Signing secret returned only when created or rotated.
    WebhookEventType:
      type: string
      enum:
        - box.ready
        - box.error
        - box.archived
        - box.hydrated
    WebhookResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - webhook
          properties:
            webhook:
              $ref: '#/components/schemas/Webhook'
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
    Webhook:
      type: object
      required:
        - id
        - name
        - url
        - events
        - createdAt
        - updatedAt
      properties:
        id:
          type: string
          pattern: ^wh_[a-f0-9]{24}$
          examples:
            - wh_0123456789abcdef01234567
        name:
          type:
            - string
            - 'null'
          maxLength: 100
          examples:
            - Production automation
        url:
          type: string
          format: uri
          description: Public HTTPS endpoint on port 443.
          examples:
            - https://example.com/hooks/box
        events:
          type: array
          minItems: 1
          uniqueItems: true
          items:
            $ref: '#/components/schemas/WebhookEventType'
        createdAt:
          type: string
          format: date-time
        updatedAt:
          type: string
          format: date-time
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