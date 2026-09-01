> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Delete a Box environment

> Soft-deletes an environment. The default environment cannot be deleted, and at least one environment is always kept.



## OpenAPI

````yaml openapi/box-v1.yaml DELETE /environments/{environmentId}
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
  /environments/{environmentId}:
    parameters:
      - name: environmentId
        in: path
        required: true
        schema:
          type: string
          format: uuid
        description: Environment id returned by `GET /environments`.
    delete:
      tags:
        - Box
      summary: Delete a Box environment
      description: >-
        Soft-deletes an environment. The default environment cannot be deleted,
        and at least one environment is always kept.
      operationId: deleteEnvironment
      responses:
        '200':
          description: Environment soft-deleted.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/BoxEnvironmentResponse'
        '401':
          $ref: '#/components/responses/Unauthorized'
        '404':
          $ref: '#/components/responses/NotFound'
        '409':
          $ref: '#/components/responses/Conflict'
components:
  schemas:
    BoxEnvironmentResponse:
      type: object
      required:
        - success
      properties:
        success:
          type: boolean
        environment:
          oneOf:
            - $ref: '#/components/schemas/BoxEnvironment'
            - type: 'null'
    BoxEnvironment:
      type: object
      required:
        - id
        - name
        - isDefault
        - latestVersionId
        - safeForThirdParties
        - passGithub
        - passSecrets
        - passBoxCredentials
        - passAgentsCredentials
        - envContents
        - secretFiles
        - versions
      description: A named Box environment. All flags/contents reflect the latest version.
      properties:
        id:
          type: string
          format: uuid
        name:
          type: string
          examples:
            - base
            - customer-demos
        isDefault:
          type: boolean
          description: >-
            Exactly one environment is the default; boxes created without an
            `environment` name use it.
        latestVersionId:
          type:
            - string
            - 'null'
          format: uuid
        safeForThirdParties:
          type: boolean
          description: >-
            When true the environment passes nothing to a box (repos, secrets,
            and all credentials withheld), overriding the fine-grained flags
            below. Use for boxes handed to third parties.
        passGithub:
          type: boolean
          description: >-
            Attach the environment's GitHub repositories and the GitHub token
            (so `gh` and pushes work). Ignored when `safeForThirdParties` is
            true.
        passSecrets:
          type: boolean
          description: >-
            Attach the environment's env variables and secret files. Ignored
            when `safeForThirdParties` is true.
        passBoxCredentials:
          type: boolean
          description: >-
            Attach the box's own service/preview credentials. Ignored when
            `safeForThirdParties` is true.
        passAgentsCredentials:
          type: boolean
          description: >-
            Attach agent-provider credentials configured on the Agents page.
            Ignored when `safeForThirdParties` is true.
        envContents:
          type: string
          description: The latest version's .env-style content. Treat as sensitive.
        secretFiles:
          type: array
          items:
            $ref: '#/components/schemas/SecretFile'
        selectedRepositories:
          type: array
          description: >-
            Repositories attached to the latest version, with base branch and
            setup script.
          items:
            $ref: '#/components/schemas/SelectedRepository'
        versions:
          type: array
          items:
            $ref: '#/components/schemas/BoxEnvironmentVersionSummary'
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
    SelectedRepository:
      allOf:
        - $ref: '#/components/schemas/Repository'
        - type: object
          properties:
            baseBranch:
              type: string
              examples:
                - main
            setupRoutineId:
              type:
                - string
                - 'null'
            setupScript:
              type: string
            setupBlocking:
              type: boolean
    BoxEnvironmentVersionSummary:
      type: object
      required:
        - id
        - versionNumber
        - boxCount
        - createdAt
      description: >-
        An immutable snapshot of an environment's config. Editing an environment
        mints a new version; boxes stay pinned to the version they were created
        on until upgraded.
      properties:
        id:
          type: string
          format: uuid
        versionNumber:
          type: integer
          description: Monotonically increasing per environment; version 1 is the first.
        boxCount:
          type: integer
          description: >-
            Number of the caller's active boxes currently pinned to this
            version.
        createdAt:
          type: string
          format: date-time
    Repository:
      type: object
      additionalProperties: true
      properties:
        id:
          type: integer
          description: GitHub repository id.
        databaseId:
          type: string
          description: Internal repository id used when selecting repositories.
        name:
          type: string
        fullName:
          type: string
          examples:
            - acme/web
        private:
          type: boolean
        permissions:
          type: string
        pushedAt:
          type:
            - string
            - 'null'
          format: date-time
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