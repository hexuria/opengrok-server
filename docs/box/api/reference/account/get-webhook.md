> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Get webhook

> Get one Box lifecycle webhook endpoint without its signing secret.



## OpenAPI

````yaml openapi/box-v1.yaml GET /webhooks/{webhookId}
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
  /webhooks/{webhookId}:
    parameters:
      - name: webhookId
        in: path
        required: true
        schema:
          type: string
          pattern: ^wh_[a-f0-9]{24}$
    get:
      tags:
        - Box
      summary: Get webhook
      description: Returns webhook metadata without the signing secret.
      operationId: getWebhook
      responses:
        '200':
          description: Webhook metadata.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/WebhookResponse'
        '401':
          $ref: '#/components/responses/Unauthorized'
        '404':
          $ref: '#/components/responses/NotFound'
components:
  schemas:
    WebhookResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - webhook
          properties:
            webhook:
              $ref: '#/components/schemas/Webhook'
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
    WebhookEventType:
      type: string
      enum:
        - box.ready
        - box.error
        - box.archived
        - box.hydrated
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