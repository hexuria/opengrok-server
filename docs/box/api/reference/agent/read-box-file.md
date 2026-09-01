> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Read Box file



## OpenAPI

````yaml openapi/box-v1.yaml GET /boxes/{boxId}/files
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
  /boxes/{boxId}/files:
    get:
      tags:
        - Box
      summary: Read a file from a Box
      operationId: readFile
      parameters:
        - $ref: '#/components/parameters/BoxId'
        - name: path
          in: query
          required: true
          schema:
            type: string
            description: >-
              Absolute path, or path relative to the Box work directory
              (/home/user). The canonicalized path must resolve under /home/user
              or /tmp; anything else is rejected with a 400 invalid_path error.
        - name: encoding
          in: query
          schema:
            type: string
            enum:
              - utf8
              - base64
            default: utf8
      responses:
        '200':
          description: File contents.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/FileReadResponse'
        '400':
          $ref: '#/components/responses/BadRequest'
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
    FileReadResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - success
            - path
            - encoding
            - size
            - content
          properties:
            type:
              type: string
              const: file.read
            success:
              type: boolean
            path:
              type: string
            encoding:
              type: string
              enum:
                - utf8
                - base64
            size:
              type: integer
            content:
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
  securitySchemes:
    BoxBearerAuth:
      type: http
      scheme: bearer
      bearerFormat: box_api_key
      description: >-
        Box bearer token in the form `box_...`. Service API keys authenticate
        Box operations.

````