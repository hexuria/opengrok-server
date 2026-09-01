> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Get desktop streaming URL

> Create or fetch a secret-bearing desktop/noVNC URL for live computer-use visibility. Use `?vnc=1` for noVNC; a `provisioning: true` response means poll again.



## OpenAPI

````yaml openapi/box-v1.yaml POST /boxes/{boxId}/desktop
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
  /boxes/{boxId}/desktop:
    post:
      tags:
        - Box
      summary: Get desktop streaming URL
      description: >-
        Create or fetch a secret-bearing desktop/noVNC URL for live computer-use
        visibility. Use `?vnc=1` for noVNC; a `provisioning: true` response
        means poll again.
      operationId: desktop
      parameters:
        - $ref: '#/components/parameters/BoxId'
        - name: vnc
          in: query
          schema:
            type: integer
            enum:
              - 1
          description: Use VNC/noVNC streaming mode.
        - name: theme
          in: query
          schema:
            type: string
            enum:
              - light
              - dark
          description: Desktop streaming theme for non-VNC mode.
      requestBody:
        required: false
        content:
          application/json:
            schema:
              $ref: '#/components/schemas/DesktopRequest'
      responses:
        '200':
          description: Desktop streaming URL, or provisioning state for VNC setup.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/DesktopResponse'
              examples:
                ready:
                  value:
                    ok: true
                    type: desktop.url
                    success: true
                    desktopUrl: https://box-preview.example/vnc.html?_token=redacted
                    ip: 203.0.113.10
                    mode: vnc
                provisioning:
                  value:
                    ok: true
                    type: desktop.provisioning
                    provisioning: true
                    message: Preparing VNC desktop…
        '400':
          $ref: '#/components/responses/BadRequest'
        '401':
          $ref: '#/components/responses/Unauthorized'
        '402':
          $ref: '#/components/responses/PaymentRequired'
        '404':
          $ref: '#/components/responses/NotFound'
components:
  parameters:
    BoxId:
      name: boxId
      in: path
      required: true
      schema:
        type: string
        pattern: ^bx_[23456789abcdefghjkmnpqrstuvwxyz]{8}$
      description: Public Box id returned by create/list/get box calls.
  schemas:
    DesktopRequest:
      type: object
      additionalProperties: true
      description: >-
        Optional desktop/VNC setup options used by the Box dashboard and CLI.
        Most callers send an empty body.
      properties:
        publicAccess:
          type: boolean
          default: false
          description: >-
            For `?vnc=1`, return a noVNC URL that does not require an access
            token.
    DesktopResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          additionalProperties: true
          properties:
            type:
              type: string
              examples:
                - desktop.url
            success:
              type: boolean
            desktopUrl:
              type:
                - string
                - 'null'
              format: uri
              description: Secret-bearing desktop or noVNC URL. Redact from logs.
            ip:
              type:
                - string
                - 'null'
            mode:
              type: string
              examples:
                - vnc
            provisioning:
              type: boolean
              description: >-
                For `?vnc=1`, true means VNC is still being prepared; poll
                again.
            message:
              type: string
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
    PaymentRequired:
      description: >-
        Account cannot currently create or operate Boxes. The error body may
        include a dashboard billing URL, but billing actions are not part of the
        v1 API.
      content:
        application/json:
          schema:
            $ref: '#/components/schemas/ErrorEnvelope'
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