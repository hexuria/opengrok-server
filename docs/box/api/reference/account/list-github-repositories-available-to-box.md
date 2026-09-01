> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# List GitHub repositories available to Box

> Returns GitHub repositories grouped by installation plus the current selected repositories for new Boxes.



## OpenAPI

````yaml openapi/box-v1.yaml GET /repos
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
    get:
      tags:
        - Box
      summary: List GitHub repositories available to Box
      description: >-
        Returns GitHub repositories grouped by installation plus the current
        selected repositories for new Boxes.
      operationId: repos
      parameters:
        - name: sync
          in: query
          schema:
            type: boolean
          description: When true, sync from GitHub before returning repository groups.
        - $ref: '#/components/parameters/Limit'
        - $ref: '#/components/parameters/Cursor'
        - $ref: '#/components/parameters/Sort'
        - name: q
          in: query
          schema:
            type: string
          description: Case-insensitive repository name/fullName filter.
        - name: selected
          in: query
          schema:
            type: boolean
          description: Filter to selected or unselected repositories.
      responses:
        '200':
          description: Repository groups and current Box repository selection.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/ReposResponse'
              examples:
                repos:
                  value:
                    ok: true
                    type: repos.list
                    environmentId: env_123
                    installations:
                      - type: Organization
                        accountLogin: acme
                        accountAvatarUrl: https://github.com/acme.png
                        repositories:
                          - id: 123456
                            databaseId: repo_org_123456
                            name: web
                            fullName: acme/web
                            description: Marketing site
                            url: https://github.com/acme/web
                            private: true
                            permissions: admin
                            pushedAt: '2026-05-31T12:00:00Z'
                    selectedRepositories:
                      - id: 123456
                        databaseId: repo_org_123456
                        name: web
                        fullName: acme/web
                        private: true
                        permissions: admin
                        pushedAt: '2026-05-31T12:00:00Z'
                        baseBranch: dev
                        setupRoutineId: null
                        setupScript: ''
                        setupBlocking: false
        '401':
          $ref: '#/components/responses/Unauthorized'
        '409':
          $ref: '#/components/responses/Conflict'
components:
  parameters:
    Limit:
      name: limit
      in: query
      schema:
        type: integer
        minimum: 1
        maximum: 200
        default: 100
      description: Maximum items to return.
    Cursor:
      name: cursor
      in: query
      schema:
        type:
          - string
          - 'null'
      description: Opaque pagination cursor returned as `pageInfo.nextCursor`.
    Sort:
      name: sort
      in: query
      schema:
        type: string
        enum:
          - asc
          - desc
        default: desc
      description: Sort direction for cursor pagination.
  schemas:
    ReposResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - installations
            - environmentId
            - selectedRepositories
          properties:
            installations:
              type: array
              items:
                $ref: '#/components/schemas/RepositoryInstallation'
            environmentId:
              type: string
            selectedRepositories:
              type: array
              items:
                $ref: '#/components/schemas/SelectedRepository'
            pageInfo:
              $ref: '#/components/schemas/PageInfo'
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
    RepositoryInstallation:
      type: object
      properties:
        type:
          type: string
          examples:
            - Organization
        accountLogin:
          type: string
          examples:
            - acme
        accountAvatarUrl:
          type:
            - string
            - 'null'
          format: uri
        repositories:
          type: array
          items:
            $ref: '#/components/schemas/Repository'
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
    PageInfo:
      type: object
      required:
        - nextCursor
        - hasMore
        - limit
      properties:
        nextCursor:
          type:
            - string
            - 'null'
        hasMore:
          type: boolean
        limit:
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