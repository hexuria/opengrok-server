> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Upgrade boxes to an environment's latest version

> Repoints the caller's active boxes from an older version of this environment to its latest version, scrubbing any owner secrets the new version drops and hot-pushing the new config.



## OpenAPI

````yaml openapi/box-v1.yaml POST /environments/{environmentId}/upgrade
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
  /environments/{environmentId}/upgrade:
    post:
      tags:
        - Box
      summary: Upgrade boxes to an environment's latest version
      description: >-
        Repoints the caller's active boxes from an older version of this
        environment to its latest version, scrubbing any owner secrets the new
        version drops and hot-pushing the new config.
      operationId: upgradeEnvironment
      parameters:
        - name: environmentId
          in: path
          required: true
          schema:
            type: string
            format: uuid
      requestBody:
        required: false
        content:
          application/json:
            schema:
              $ref: '#/components/schemas/UpgradeBoxEnvironmentRequest'
      responses:
        '200':
          description: Upgrade result counts.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/UpgradeBoxEnvironmentResponse'
              examples:
                upgraded:
                  value:
                    success: true
                    upgraded: 2
                    failed: 0
        '401':
          $ref: '#/components/responses/Unauthorized'
        '404':
          $ref: '#/components/responses/NotFound'
components:
  schemas:
    UpgradeBoxEnvironmentRequest:
      type: object
      description: >-
        Repoint active boxes to the environment's latest version, scrubbing any
        owner secrets the new version drops and hot-pushing the new config.
      properties:
        agentIds:
          type: array
          items:
            type: string
          description: >-
            Restrict the upgrade to these box (agent) ids. Omit to upgrade all
            of the caller's active boxes that are on an older version of this
            environment.
    UpgradeBoxEnvironmentResponse:
      type: object
      required:
        - success
        - upgraded
        - failed
      properties:
        success:
          type: boolean
        upgraded:
          type: integer
        failed:
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