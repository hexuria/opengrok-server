> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Get Box secrets setup

> Returns the environment variables and secret files configured for Boxes.



## OpenAPI

````yaml openapi/box-v1.yaml GET /secrets
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
  /secrets:
    get:
      tags:
        - Box
      summary: Get Box secrets setup
      description: Returns the environment variables and secret files configured for Boxes.
      operationId: secrets
      responses:
        '200':
          description: Current secret setup metadata.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/SecretsResponse'
              examples:
                secrets:
                  value:
                    ok: true
                    type: secrets.info
                    environmentId: env_123
                    envContents: |
                      OPENAI_API_KEY=sk-...
                    secretFiles:
                      - path: .config/service-account.json
                        contents: '{"type":"service_account"}'
        '401':
          $ref: '#/components/responses/Unauthorized'
components:
  schemas:
    SecretsResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - environmentId
            - envContents
            - secretFiles
          properties:
            success:
              type: boolean
            environmentId:
              type: string
            envContents:
              type: string
            secretFiles:
              type: array
              items:
                $ref: '#/components/schemas/SecretFile'
            pushed:
              type: object
              additionalProperties: true
              description: >-
                Present on update; counts how many active Boxes received the new
                environment.
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
    SecretFile:
      type: object
      required:
        - path
        - contents
      properties:
        path:
          type: string
          examples:
            - .config/service-account.json
        contents:
          type: string
          description: Secret file contents. Treat as sensitive.
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
  securitySchemes:
    BoxBearerAuth:
      type: http
      scheme: bearer
      bearerFormat: box_api_key
      description: >-
        Box bearer token in the form `box_...`. Service API keys authenticate
        Box operations.

````