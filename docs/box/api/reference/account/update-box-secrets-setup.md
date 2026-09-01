> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Update Box secrets setup



## OpenAPI

````yaml openapi/box-v1.yaml POST /secrets
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
    post:
      tags:
        - Box
      summary: Update Box secrets setup
      operationId: updateSecrets
      requestBody:
        required: true
        content:
          application/json:
            schema:
              $ref: '#/components/schemas/SecretsUpdateRequest'
      responses:
        '200':
          description: Updated secret setup metadata.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/SecretsResponse'
              examples:
                updated:
                  value:
                    ok: true
                    type: secrets.updated
                    success: true
                    environmentId: env_123
                    envContents: |
                      OPENAI_API_KEY=sk-...
                    secretFiles: []
                    pushed:
                      updated: 2
                      failed: 0
        '401':
          $ref: '#/components/responses/Unauthorized'
components:
  schemas:
    SecretsUpdateRequest:
      type: object
      description: >-
        Full replacement for the Box secret setup. Omitted `envContents` or
        `secretFiles` are treated as empty values, and successful updates are
        pushed to active Boxes.
      properties:
        envContents:
          type: string
          description: >-
            Full .env-style content to sync into Boxes. Send the complete
            desired file contents, not a patch.
        secretFiles:
          type: array
          description: >-
            Full list of secret files to keep configured. Send existing files
            again if they should remain.
          items:
            $ref: '#/components/schemas/SecretFile'
      examples:
        - envContents: |-
            OPENAI_API_KEY=sk-...
            DATABASE_URL=postgres://...
          secretFiles:
            - path: .config/service-account.json
              contents: '{"type":"service_account"}'
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
  securitySchemes:
    BoxBearerAuth:
      type: http
      scheme: bearer
      bearerFormat: box_api_key
      description: >-
        Box bearer token in the form `box_...`. Service API keys authenticate
        Box operations.

````