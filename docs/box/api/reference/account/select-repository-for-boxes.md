> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Select repository for Boxes

> Idempotently selects one repository for the Box environment. Use `databaseId` from `GET /repos` as `repositoryId`; selecting an already-selected repository updates its `baseBranch`.



## OpenAPI

````yaml openapi/box-v1.yaml POST /repos
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
  /repos:
    post:
      tags:
        - Box
      summary: Select repository for Boxes
      description: >-
        Idempotently selects one repository for the Box environment. Use
        `databaseId` from `GET /repos` as `repositoryId`; selecting an
        already-selected repository updates its `baseBranch`.
      operationId: selectRepo
      requestBody:
        required: true
        content:
          application/json:
            schema:
              $ref: '#/components/schemas/RepoSelectionRequest'
      responses:
        '200':
          description: Updated repository selection.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/RepoSelectionResponse'
              examples:
                selected:
                  value:
                    ok: true
                    type: repos.updated
                    success: true
                    environmentId: env_123
                    selectedRepositories:
                      - databaseId: repo_org_123456
                        name: web
                        fullName: acme/web
                        baseBranch: dev
                        setupRoutineId: null
                        setupScript: ''
                        setupBlocking: false
        '400':
          $ref: '#/components/responses/BadRequest'
        '401':
          $ref: '#/components/responses/Unauthorized'
        '409':
          $ref: '#/components/responses/Conflict'
components:
  schemas:
    RepoSelectionRequest:
      type: object
      required:
        - repositoryId
      description: >-
        Idempotently selects a repository for future Boxes. If the repository is
        already selected, the API updates its base branch instead of returning a
        conflict.
      properties:
        repositoryId:
          type: string
          description: Internal repository `databaseId` returned by `GET /repos`.
        baseBranch:
          type: string
          default: main
      examples:
        - repositoryId: repo_org_123
          baseBranch: dev
    RepoSelectionResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - success
            - environmentId
            - selectedRepositories
          properties:
            success:
              type: boolean
            environmentId:
              type: string
            selectedRepositories:
              type: array
              items:
                $ref: '#/components/schemas/SelectedRepository'
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