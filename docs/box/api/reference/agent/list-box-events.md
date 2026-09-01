> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# List box events

> Return Box work, lifecycle, and progress events so product UIs can show what the box is doing. Event payloads are extensible. Clients can long-poll this endpoint with `sort=asc` and a cursor to stream responses. Response events expose agent text, streaming partials via `data.is_streaming`, and tool calls/results via `data.tools`.



## OpenAPI

````yaml openapi/box-v1.yaml GET /boxes/{boxId}/events
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
  /boxes/{boxId}/events:
    get:
      tags:
        - Box
      summary: List box events
      description: >-
        Return Box work, lifecycle, and progress events so product UIs can show
        what the box is doing. Event payloads are extensible. Clients can
        long-poll this endpoint with `sort=asc` and a cursor to stream
        responses. Response events expose agent text, streaming partials via
        `data.is_streaming`, and tool calls/results via `data.tools`.
      operationId: events
      parameters:
        - $ref: '#/components/parameters/BoxId'
        - $ref: '#/components/parameters/Limit'
        - $ref: '#/components/parameters/Cursor'
        - $ref: '#/components/parameters/Sort'
        - name: type
          in: query
          schema:
            type: string
          description: Comma-separated event type filter, for example `prompt,response`.
      responses:
        '200':
          description: >-
            The agent's work on the box (prompts, responses), not box lifecycle.
            A box that was never prompted returns an empty list even though it
            started, stopped and resumed.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/EventsResponse'
              examples:
                progress:
                  value:
                    ok: true
                    type: events.list
                    id: bx_23456789
                    events:
                      - id: prompt_123
                        type: prompt
                        timestamp: 1780347055370
                        taskId: prompt_123
                        data:
                          prompt: Run tests and summarize failures.
                          status: running
                          is_reverted: false
                      - id: response_123-tools
                        type: response
                        timestamp: 1780347063427
                        taskId: prompt_123
                        data:
                          content: ''
                          model: gpt-5.4
                          tools:
                            - use:
                                id: call_123
                                type: tool_use
                                name: Bash
                                input:
                                  command: npm test
                              result:
                                type: tool_result
                                tool_use_id: call_123
                                is_error: false
                                content: '{"exitCode":0}'
                          is_reverted: false
                      - id: response_123
                        type: response
                        timestamp: 1780347065650
                        taskId: prompt_123
                        data:
                          content: Tests passed.
                          model: gpt-5.4
                          is_reverted: false
                    pageInfo:
                      nextCursor: null
                      hasMore: false
                      limit: 100
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
    EventsResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - id
            - events
          properties:
            type:
              type: string
              const: events.list
            id:
              type: string
            events:
              type: array
              description: >-
                Box work and lifecycle event objects. Event shapes are
                intentionally extensible; branch on each event `type` when
                present.
              items:
                $ref: '#/components/schemas/BoxEvent'
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
    BoxEvent:
      type: object
      required:
        - type
      additionalProperties: true
      properties:
        id:
          type: string
        type:
          type: string
        timestamp:
          type: integer
        taskId:
          type:
            - string
            - 'null'
        data:
          type: object
          additionalProperties: true
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