> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Get account data-retention policy

> Read the account zero-data-retention setting.



## OpenAPI

````yaml openapi/box-v1.yaml GET /account/data-retention
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
  /account/data-retention:
    get:
      tags:
        - Box
      summary: Get account data-retention policy
      description: >-
        Returns the current zero-data-retention policy. Per-Box API keys cannot
        access this account-wide setting. Responses are never cached.
      operationId: getDataRetention
      responses:
        '200':
          description: Current retention policy.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/DataRetentionPolicyResponse'
        '401':
          $ref: '#/components/responses/Unauthorized'
        '403':
          $ref: '#/components/responses/Forbidden'
components:
  schemas:
    DataRetentionPolicyResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - enabled
            - enabledAt
          properties:
            type:
              type: string
              enum:
                - data_retention.info
                - data_retention.updated
            enabled:
              type: boolean
            enabledAt:
              type:
                - string
                - 'null'
              format: date-time
            queuedBoxes:
              type: integer
              minimum: 0
              description: Archived Boxes newly queued for deletion by this policy update.
            acceptedDeletionOperationsIrreversible:
              type: boolean
              description: >-
                Always true on updates. Disabling the policy does not cancel
                accepted deletion operations.
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
  securitySchemes:
    BoxBearerAuth:
      type: http
      scheme: bearer
      bearerFormat: box_api_key
      description: >-
        Box bearer token in the form `box_...`. Service API keys authenticate
        Box operations.

````