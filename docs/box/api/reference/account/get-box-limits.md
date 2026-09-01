> ## Documentation Index
> Fetch the complete documentation index at: https://docs.ascii.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Get Box limits

> Check remaining machine starts, compute time, credits, access readiness, and concurrent-box capacity for the authenticated account. Pass `org` / `X-Box-Org` (or `teamId`) to read a team wallet you belong to.



## OpenAPI

````yaml openapi/box-v1.yaml GET /limits
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
  /limits:
    get:
      tags:
        - Box
      summary: Get Box limits
      description: >-
        Check remaining machine starts, compute time, credits, access readiness,
        and concurrent-box capacity for the authenticated account. Pass `org` /
        `X-Box-Org` (or `teamId`) to read a team wallet you belong to.
      operationId: limits
      parameters:
        - $ref: '#/components/parameters/OrgId'
        - $ref: '#/components/parameters/OrgHeader'
        - name: teamId
          in: query
          required: false
          schema:
            type: string
          description: >-
            Legacy alias for `org`. Takes precedence over `org` / `X-Box-Org`
            when set.
      responses:
        '200':
          description: Current creation and concurrent-box limits.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/LimitsResponse'
              examples:
                ready:
                  value:
                    ok: true
                    type: limits.info
                    canStart: true
                    activeBoxes: 1
                    activeStates:
                      - provisioned
                      - cloning
                      - ready
                      - idle
                      - running
                    boxPlanKey: box_20
                    boxPlanDollars: 20
                    maxActiveBoxes: 100
                    maxCreationRequestsPerMinute: 10
                    maxCreationRequestsPerDay: null
                    startLimits:
                      perMinute: 10
                      perHour: 50
                      perDay: 150
                    starts:
                      unlimited: false
                      minute:
                        limit: 10
                        used: 3
                        remaining: 7
                      hour:
                        limit: 50
                        used: 12
                        remaining: 38
                      day:
                        limit: 150
                        used: 47
                        remaining: 103
                    billingStatus: active
                    creditBalanceSeconds: 7200
                    creditBalanceHours: 2
                    packBalanceSeconds: 500000
                    packBalanceHours: 138.89
                    packBalanceDollars: 5
        '401':
          $ref: '#/components/responses/Unauthorized'
components:
  parameters:
    OrgId:
      name: org
      in: query
      required: false
      schema:
        type: string
      description: >-
        Billing wallet for this request. A team id you belong to reads that
        team's limits / bills a create to that team. Your own account id is
        personal. Boxes, snapshots, and environments stay creator-private.
    OrgHeader:
      name: X-Box-Org
      in: header
      required: false
      schema:
        type: string
      description: Same as the `org` query parameter. Query wins when both are set.
  schemas:
    LimitsResponse:
      allOf:
        - $ref: '#/components/schemas/SuccessBase'
        - $ref: '#/components/schemas/LimitsFields'
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
    LimitsFields:
      type: object
      required:
        - activeBoxes
        - maxActiveBoxes
        - canStart
        - billingStatus
      additionalProperties: true
      properties:
        accessTier:
          type: string
          examples:
            - trial
        blockedReason:
          type:
            - string
            - 'null'
        currentLimits:
          type: object
          additionalProperties: true
          properties:
            activeBoxes:
              type: integer
            creationRatePerMinute:
              type: integer
            creationRequestsPerDay:
              type:
                - integer
                - 'null'
        standardLimits:
          type: object
          additionalProperties: true
          properties:
            activeBoxes:
              type: integer
            creationRatePerMinute:
              type: integer
            creationRequestsPerDay:
              type:
                - integer
                - 'null'
        trialLimits:
          type: object
          additionalProperties: true
          properties:
            activeBoxes:
              type: integer
            creationRatePerMinute:
              type: integer
            creationRequestsPerDay:
              type:
                - integer
                - 'null'
        upgradeEffects:
          type: object
          additionalProperties: true
        canStart:
          type: boolean
          description: >-
            Whether the authenticated account can create or operate boxes right
            now.
        checkoutRequired:
          type: boolean
        startBlockedReason:
          type:
            - string
            - 'null'
        contactMessage:
          type:
            - string
            - 'null'
        activeBoxes:
          type: integer
        activeStates:
          type: array
          items:
            type: string
        maxActiveBoxes:
          type: integer
        maxCreationRequestsPerMinute:
          type: integer
        maxCreationRequestsPerDay:
          type:
            - integer
            - 'null'
        startLimits:
          type:
            - object
            - 'null'
          description: >-
            Plan caps for machine starts. Null on unlimited accounts. Create,
            fork and resume each count as one start.
          properties:
            perMinute:
              type: integer
            perHour:
              type: integer
            perDay:
              type: integer
        starts:
          type: object
          description: >-
            Remaining machine starts in the rolling minute, hour and day
            windows. Null windows mean the account is unlimited.
          properties:
            unlimited:
              type: boolean
            minute:
              $ref: '#/components/schemas/StartWindowUsage'
            hour:
              $ref: '#/components/schemas/StartWindowUsage'
            day:
              $ref: '#/components/schemas/StartWindowUsage'
        creditBalanceHours:
          type:
            - number
            - 'null'
          description: >-
            Remaining machine time in hours (`creditBalanceSeconds / 3600`).
            Null on unlimited accounts.
        packBalanceHours:
          type: number
          description: Remaining purchased credit packs in hours.
        packBalanceDollars:
          type: number
          description: Remaining purchased credit packs in dollars.
        hasPaymentHistory:
          type: boolean
        package:
          type: object
          additionalProperties: true
        subscriptionQuotaSeconds:
          type: integer
        subscriptionRemainingSeconds:
          type: integer
        packBalanceSeconds:
          type: integer
        creditPurchasedSeconds:
          type: integer
        creditUsedSeconds:
          type: integer
        liveUsageSeconds:
          type: integer
        creditSecondsPerDollar:
          type: integer
        billingStatus:
          type: string
          description: >-
            Account access state returned by the current backend. Billing
            endpoints are not part of v1.
        subscriptionStatus:
          type:
            - string
            - 'null'
        subscriptionCancelAtPeriodEnd:
          type: boolean
        hasSubscription:
          type: boolean
        subscriptionTrialEndsAt:
          type:
            - string
            - 'null'
          format: date-time
        subscriptionCurrentPeriodEnd:
          type:
            - string
            - 'null'
          format: date-time
        creditBalanceSeconds:
          type: integer
        teamId:
          type: string
          description: >-
            Present when limits were read for a team wallet (`?teamId=`,
            `?org=`, or `X-Box-Org`).
        teamRole:
          type: string
          description: Caller's role on that team when `teamId` is present.
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
    StartWindowUsage:
      type:
        - object
        - 'null'
      description: One rolling start window. Null when the account is unlimited.
      properties:
        limit:
          type: integer
        used:
          type: integer
        remaining:
          type: integer
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