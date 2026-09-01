> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Remove environment variable

> Removes a single variable. Mints a new immutable version.



## OpenAPI

````yaml openapi/box-v1.yaml DELETE /environments/{environmentId}/vars/{key}
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
  /environments/{environmentId}/vars/{key}:
    parameters:
      - name: environmentId
        in: path
        required: true
        schema:
          type: string
          format: uuid
      - name: key
        in: path
        required: true
        schema:
          type: string
          pattern: ^[A-Za-z_][A-Za-z0-9_]*$
        description: Environment variable name.
    delete:
      tags:
        - Box
      summary: Remove one environment variable
      description: Removes a single variable. Mints a new immutable version.
      operationId: deleteEnvironmentVar
      responses:
        '200':
          description: New version minted.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/EnvironmentItemChangeResponse'
        '401':
          $ref: '#/components/responses/Unauthorized'
        '404':
          $ref: '#/components/responses/NotFound'
components:
  schemas:
    EnvironmentItemChangeResponse:
      type: object
      description: >-
        Result of a granular environment change. Every change mints a new
        immutable version holding just that delta; existing boxes stay pinned
        until upgraded.
      required:
        - success
      properties:
        success:
          type: boolean
        versionId:
          type: string
          description: >-
            Id of the newly minted environment version. This is a version id,
            not the environment's own id: the environment id you passed in the
            path is unchanged.
        versionNumber:
          type: integer
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