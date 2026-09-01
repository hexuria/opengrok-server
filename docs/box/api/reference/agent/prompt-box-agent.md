> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Prompt Box

> Queue a natural-language work item for Codex or Claude Code inside the box. Observe progress with `GET /boxes/{boxId}/events`.



## OpenAPI

````yaml openapi/box-v1.yaml POST /boxes/{boxId}/prompt
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
  /boxes/{boxId}/prompt:
    post:
      tags:
        - Box
      summary: Prompt Box
      description: >-
        Queue a natural-language work item for Codex or Claude Code inside the
        box. Observe progress with `GET /boxes/{boxId}/events`.
      operationId: prompt
      parameters:
        - $ref: '#/components/parameters/BoxId'
      requestBody:
        required: true
        content:
          application/json:
            schema:
              $ref: '#/components/schemas/PromptRequest'
            examples:
              repoWork:
                value:
                  provider: codex
                  model: gpt-5.4
                  reasoningEffort: medium
                  prompt: >-
                    Work on the selected repo, run tests, fix failures, commit
                    the result, and report any hosted preview URL.
              computerUse:
                value:
                  provider: claude-code
                  model: sonnet
                  reasoningEffort: high
                  prompt: >-
                    Use the browser to research flight options to France and
                    return the best itinerary summary with source links.
      responses:
        '202':
          description: Prompt queued.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/PromptResponse'
              examples:
                queued:
                  value:
                    ok: true
                    type: prompt.queued
                    id: bx_23456789
                    promptId: prompt_123
                    promptRun:
                      id: prompt_123
                      promptId: prompt_123
                      boxId: bx_23456789
                      status: queued
                      done: false
                    status: queued
                    provider: codex
                    model: gpt-5.4
                    reasoningEffort: medium
        '400':
          $ref: '#/components/responses/BadRequest'
        '401':
          $ref: '#/components/responses/Unauthorized'
        '402':
          $ref: '#/components/responses/PaymentRequired'
        '404':
          $ref: '#/components/responses/NotFound'
        '409':
          $ref: '#/components/responses/Conflict'
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
    PromptRequest:
      type: object
      description: >
        Work item to queue inside an existing Box. Provider credentials must
        already be configured in the Box dashboard.


        Provider values: `codex`, `claude-code`; the backend currently also
        accepts `claude` as an alias for `claude-code`.


        Recommended model ids come from `backend/models.json`: Codex
        `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`, `gpt-5.5`, `gpt-5.4`,
        `gpt-5.4-mini`, `gpt-5.3-codex`; Claude Code `claude-fable-5`, `opus-5`,
        `opus`, `sonnet`, `haiku`. If omitted, the user's saved provider default
        is used.
      required:
        - provider
        - prompt
      properties:
        provider:
          type: string
          enum:
            - codex
            - claude-code
            - claude
        model:
          type:
            - string
            - 'null'
          description: >-
            Optional provider model id. Prefer the documented ids; unknown
            explicit ids are currently forwarded rather than rejected by request
            validation.
          examples:
            - gpt-5.6-terra
            - claude-fable-5
            - sonnet
        reasoningEffort:
          type:
            - string
            - 'null'
          description: >-
            Optional reasoning effort. Codex supports `none`, `low`, `medium`,
            `high`, `xhigh`, `max` for GPT-5.6 Sol/Terra/Luna, `none`, `low`,
            `medium`, `high`, `xhigh` for GPT-5.4/5.5/5.4-mini and `low`,
            `medium`, `high`, `xhigh` for GPT-5.3 Codex. Claude
            `claude-fable-5`, `opus-5` and `opus` support `low`, `medium`,
            `high`, `max`; `sonnet` supports `low`, `medium`, `high`; `haiku`
            has no reasoning control.
          examples:
            - medium
        prompt:
          type: string
          minLength: 1
          description: >-
            Natural-language task for the Box, including repo, preview, or
            browser-use instructions.
      examples:
        - provider: codex
          model: gpt-5.4
          reasoningEffort: medium
          prompt: >-
            Work on the selected repo, run tests, fix failures, commit the
            result, and report any hosted preview URL.
    PromptResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - type: object
          required:
            - id
            - promptId
            - promptRun
            - status
            - provider
          properties:
            type:
              type: string
              const: prompt.queued
            id:
              type: string
              description: Box id.
            promptId:
              type: string
            promptRun:
              $ref: '#/components/schemas/PromptRun'
            status:
              type: string
              enum:
                - queued
            provider:
              type: string
            model:
              type:
                - string
                - 'null'
            reasoningEffort:
              type:
                - string
                - 'null'
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
    PromptRun:
      type: object
      required:
        - id
        - promptId
        - boxId
        - status
        - done
      properties:
        id:
          type: string
        promptId:
          type: string
        boxId:
          type: string
        status:
          type: string
          enum:
            - sending
            - queued
            - running
            - finished
            - failed
        done:
          type: boolean
        createdAt:
          type:
            - string
            - 'null'
          format: date-time
        model:
          type:
            - string
            - 'null'
        reasoningEffort:
          type:
            - string
            - 'null'
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