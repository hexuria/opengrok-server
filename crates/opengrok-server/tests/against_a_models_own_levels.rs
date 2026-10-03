//! A model's own reasoning levels, agreed with NativeChat on 3 Oct 2026: `GET /models` gives every
//! row `efforts` and `ownEffort` from what its source publishes, null when it publishes none, and
//! an effort a write names is refused when the chosen model lists levels without it, on a Bot's
//! `PATCH /coworkers/{id}` and on the default for new bots.
//!
//! Stand-ins on 127.0.0.1 play the gateway and the person's own proxy. The proxy's rows are
//! opencodex 2.75.0's own, captured from its `/v1/models` on 3 Oct 2026; the gateway's are faked
//! here, in the same three fields open-ai-gateway is adding. NEVER THE OWNER'S REAL PROXY: every
//! address is a stand-in's own ephemeral port. Needs Postgres; skips loudly without
//! OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::routing::get;
use axum::{Json, Router};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_store::PgStore;
use serde_json::{Value, json};

macro_rules! database_or_skip {
    () => {
        match std::env::var("OG_DATABASE_URL") {
            Ok(url) => opengrok_store::gate_database_or_panic(url),
            Err(_) => {
                eprintln!("skipping: OG_DATABASE_URL is not set");
                return;
            }
        }
    };
}

const ROUTE: &str = "/account/inference-source";

/// The person's proxy: gpt-6-sol, gpt-6-luna and xai/grok-4.6 as opencodex 2.75.0 lists them,
/// trimmed of fields nothing reads. Sol publishes six levels up to `ultra`, luna five up to `max`,
/// each with its own; grok-4.6 publishes none.
fn plan_rows() -> Value {
    json!({ "object": "list", "data": [
        { "id": "gpt-6-sol", "object": "model", "created": 0, "owned_by": "openai",
          "supports_reasoning_effort": true, "reasoning_effort": "medium",
          "reasoning_efforts": [
              { "value": "low", "label": "Low Effort" },
              { "value": "medium", "label": "Medium Effort", "default": true },
              { "value": "high", "label": "High Effort" },
              { "value": "xhigh", "label": "Xhigh Effort" },
              { "value": "max", "label": "Max Effort" },
              { "value": "ultra", "label": "Ultra Effort" } ],
          "context_window": 872000 },
        { "id": "gpt-6-luna", "object": "model", "created": 0, "owned_by": "openai",
          "supports_reasoning_effort": true, "reasoning_effort": "medium",
          "reasoning_efforts": [
              { "value": "low", "label": "Low Effort" },
              { "value": "medium", "label": "Medium Effort", "default": true },
              { "value": "high", "label": "High Effort" },
              { "value": "xhigh", "label": "Xhigh Effort" },
              { "value": "max", "label": "Max Effort" } ],
          "context_window": 872000 },
        { "id": "xai/grok-4.6", "object": "model", "created": 0, "owned_by": "xai" }
    ]})
}

/// The gateway, faked: the same model under its own id with fewer levels than the plan's, so a
/// door's own list is what an effort is held to, and one route with none of the three fields.
fn gateway_rows() -> Value {
    json!({ "object": "list", "data": [
        { "id": "openai/gpt-6-luna", "oag": { "context_window": 400000 },
          "supports_reasoning_effort": true, "reasoning_effort": "medium",
          "reasoning_efforts": [
              { "value": "low", "label": "Low Effort" },
              { "value": "medium", "label": "Medium Effort", "default": true },
              { "value": "high", "label": "High Effort" } ] },
        { "id": "xai/grok-4.6", "oag": { "context_window": 200000 } }
    ]})
}

fn luna_on_the_plan() -> Value {
    json!([{ "value": "low", "label": "Low Effort" },
           { "value": "medium", "label": "Medium Effort" },
           { "value": "high", "label": "High Effort" },
           { "value": "xhigh", "label": "Xhigh Effort" },
           { "value": "max", "label": "Max Effort" }])
}

fn sol_on_the_plan() -> Value {
    let mut levels = luna_on_the_plan();
    if let Some(levels) = levels.as_array_mut() {
        levels.push(json!({ "value": "ultra", "label": "Ultra Effort" }));
    }
    levels
}

/// A stand-in answering `/healthz` and `/v1/models` with `rows`, counting the listings it gave.
async fn lister(rows: Value) -> (String, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let counter = asked.clone();
    let models = get(move || {
        let (rows, counter) = (rows.clone(), counter.clone());
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Json(rows)
        }
    });
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/models", models);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, asked)
}

struct Person {
    token: String,
}

struct Harness {
    base: String,
    client: reqwest::Client,
    store: PgStore,
    minter: Arc<TokenMinter>,
    gateway_asked: Arc<AtomicUsize>,
    proxy_url: String,
}

async fn harness(database_url: &str) -> Harness {
    harness_on(database_url, None).await
}

/// The server, its catalogue on the gateway stand-in, or on `gateway` when one is named: an
/// address nothing answers is a catalogue that cannot be read.
async fn harness_on(database_url: &str, gateway: Option<&str>) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let (gateway_url, gateway_asked) = lister(gateway_rows()).await;
    let (proxy_url, _) = lister(plan_rows()).await;
    let gateway_url = gateway.map_or(gateway_url, str::to_string);
    let minter = Arc::new(TokenMinter::new(b"model-levels-test-secret"));
    let catalogue = opengrok_server::models::ModelCatalogue::new(gateway_url, "oag_live_levels");
    let auth = AuthState::new(store.clone(), minter.clone(), "owner@og.local".to_string())
        .with_model_catalogue(Some(Arc::new(catalogue)))
        .with_gateway_admin(None);
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::echoing()),
        model: "xai/grok-4.6".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: None,
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
        host_settings: None,
    };
    let host = HostState::new(agui.clone(), Some("http://opengrok.lan:1447".to_string()));
    let app = opengrok_server::router(agui, host);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Harness {
        base: format!("http://127.0.0.1:{}", addr.port()),
        client: reqwest::Client::new(),
        store,
        minter,
        gateway_asked,
        proxy_url,
    }
}

impl Harness {
    async fn person(&self) -> Person {
        let id = AccountId::new();
        let email = format!("levels-{}@og.local", uuid::Uuid::now_v7().simple());
        let at_ms = chrono::Utc::now().timestamp_millis();
        let events = Account::default()
            .decide(AccountCommand::Register {
                email: email.clone(),
                password_hash: "x".to_string(),
                first_name: "Test".to_string(),
                last_name: "User".to_string(),
                org_id: String::new(),
                plan: Plan::Ultra,
                verified: true,
                enabled: true,
                at_ms,
            })
            .expect("register");
        let view = AccountView {
            id: id.clone(),
            email: email.clone(),
            plan: Plan::Ultra,
            trial: false,
            updated_at_ms: at_ms,
            password_hash: Some("x".to_string()),
            first_name: "Test".to_string(),
            last_name: "User".to_string(),
            org_id: None,
            verified: true,
            enabled: true,
            avatar_url: None,
        };
        self.store
            .append_account(&id, 0, &events, &view)
            .await
            .expect("append account");
        let now = chrono::Utc::now().timestamp();
        let token = self
            .minter
            .mint_access(id.as_str(), "sess-levels", &email, "ultra", now, 3600)
            .expect("mint access");
        Person { token }
    }

    async fn send(
        &self,
        who: &Person,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = self.client.request(method, format!("{}{path}", self.base));
        request = request.header("Authorization", format!("Bearer {}", who.token));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let res = request.send().await.expect("request");
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        (status, body)
    }

    async fn models(&self, who: &Person, query: &str) -> (u16, Value) {
        let path = format!("/models{query}");
        self.send(who, reqwest::Method::GET, &path, None).await
    }

    async fn set(&self, who: &Person, body: Value) -> (u16, Value) {
        self.send(who, reqwest::Method::PUT, ROUTE, Some(body))
            .await
    }

    /// The person's own proxy saved as their setting, at `base`.
    async fn on_a_proxy_at(&self, who: &Person, base: &str) {
        let body = json!({ "kind": "local_proxy", "baseUrl": base, "localModel": "gpt-6-luna" });
        let (status, saved) = self.set(who, body).await;
        assert_eq!(status, 200, "{saved}");
    }

    async fn on_the_proxy(&self, who: &Person) {
        self.on_a_proxy_at(who, &self.proxy_url).await;
    }

    async fn hire(&self, who: &Person) -> String {
        let body = Some(json!({ "name": "Ada" }));
        let post = reqwest::Method::POST;
        let (status, hired) = self.send(who, post, "/coworkers", body).await;
        assert_eq!(status, 201, "{hired}");
        hired["id"].as_str().expect("id").to_string()
    }

    async fn patch(&self, who: &Person, bot: &str, body: Value) -> (u16, Value) {
        let path = format!("/coworkers/{bot}");
        self.send(who, reqwest::Method::PATCH, &path, Some(body))
            .await
    }

    /// `bot`'s row on the roster, as the app reads it.
    async fn row(&self, who: &Person, bot: &str) -> Value {
        let (status, roster) = self
            .send(who, reqwest::Method::GET, "/coworkers", None)
            .await;
        assert_eq!(status, 200, "{roster}");
        let rows = roster.as_array().cloned().unwrap_or_default();
        let row = rows.into_iter().find(|row| row["id"] == bot);
        row.unwrap_or_else(|| panic!("{bot} is on the roster: {roster}"))
    }

    fn gateway_asked(&self) -> usize {
        self.gateway_asked.load(Ordering::SeqCst)
    }
}

/// THE PICKER ON A PERSON'S OWN PLAN, recorded for NativeChat: a model the proxy gives levels
/// carries them low to high, without `default`, and its own; one it gives none carries nulls,
/// which is no slider. One call on `/models`.
#[tokio::test]
async fn the_model_list_gives_a_plan_model_its_own_levels_and_none_to_one_without() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    h.on_the_proxy(&ada).await;
    let (status, listing) = h.models(&ada, "?source=local_proxy").await;
    assert_eq!(status, 200, "{listing}");
    assert_eq!(
        listing["models"],
        json!([
            { "id": "gpt-6-sol", "points": null, "source": "local_proxy", "via": "loopback",
              "efforts": sol_on_the_plan(), "ownEffort": "medium" },
            { "id": "gpt-6-luna", "points": null, "source": "local_proxy", "via": "loopback",
              "efforts": luna_on_the_plan(), "ownEffort": "medium" },
            { "id": "xai/grok-4.6", "points": null, "source": "local_proxy", "via": "loopback",
              "efforts": null, "ownEffort": null },
        ])
    );
}

/// THE PICKER ON THE GATEWAY, recorded for NativeChat: the gateway's three fields read as the
/// proxy's are, and a route without them carries nulls. One call on `/models`.
#[tokio::test]
async fn the_model_list_gives_a_gateway_model_the_levels_the_gateway_publishes() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let (status, listing) = h.models(&ada, "?source=gateway").await;
    assert_eq!(status, 200, "{listing}");
    let three = json!([{ "value": "low", "label": "Low Effort" },
                       { "value": "medium", "label": "Medium Effort" },
                       { "value": "high", "label": "High Effort" }]);
    assert_eq!(
        listing["models"],
        json!([
            { "id": "openai/gpt-6-luna", "points": null, "source": "gateway",
              "efforts": three, "ownEffort": "medium" },
            { "id": "xai/grok-4.6", "points": null, "source": "gateway",
              "efforts": null, "ownEffort": null },
        ])
    );
}

/// A Bot's effort is held to the levels its model lists on the door and pin the body leaves it
/// on, and the refusal names them. The body is one decision: nothing beside it is applied.
#[tokio::test]
async fn an_effort_a_bots_model_does_not_list_is_refused_naming_the_levels_it_does() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    h.on_the_proxy(&ada).await;
    let bot = h.hire(&ada).await;
    let before = h.row(&ada, &bot).await;

    let plan = json!({ "source": "local_proxy", "model": "gpt-6-luna", "effort": "none",
                       "name": "Renamed" });
    let (status, refused) = h.patch(&ada, &bot, plan).await;
    assert_eq!(status, 400, "{refused}");
    let why = "effort: gpt-6-luna takes low, medium, high, xhigh or max, not \"none\"";
    assert_eq!(refused, json!({ "error": why }));

    // The same model on the gateway is the gateway's, with its own list.
    let server = json!({ "source": "gateway", "model": "openai/gpt-6-luna", "effort": "max" });
    let (status, refused) = h.patch(&ada, &bot, server).await;
    assert_eq!(status, 400, "{refused}");
    let why = "effort: openai/gpt-6-luna takes low, medium or high, not \"max\"";
    assert_eq!(refused, json!({ "error": why }));
    assert_eq!(
        h.row(&ada, &bot).await,
        before,
        "nothing in either body applied"
    );

    // An effort alone is held to the pin the Bot is on.
    let repin = json!({ "source": "gateway", "model": "openai/gpt-6-luna" });
    assert_eq!(h.patch(&ada, &bot, repin).await.0, 200);
    let (status, refused) = h.patch(&ada, &bot, json!({ "effort": "xhigh" })).await;
    assert_eq!(status, 400, "{refused}");
    let why = "effort: openai/gpt-6-luna takes low, medium or high, not \"xhigh\"";
    assert_eq!(refused, json!({ "error": why }));
    assert_eq!(h.row(&ada, &bot).await["effort"], "inherit");
}

/// `inherit` is the model's own level and never refused; a level the model lists is taken; and a
/// model that publishes no levels, or that nothing lists, takes any word, as before levels.
#[tokio::test]
async fn inherit_a_listed_level_and_a_model_with_no_levels_are_all_accepted() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    h.on_the_proxy(&ada).await;
    let bot = h.hire(&ada).await;
    let accepted = [
        (
            json!({ "source": "local_proxy", "model": "gpt-6-luna", "effort": "max" }),
            "max",
        ),
        (json!({ "effort": "inherit" }), "inherit"),
        (json!({ "effort": "low" }), "low"),
        (json!({ "effort": null }), "inherit"),
        (
            json!({ "source": "gateway", "model": "openai/gpt-6-luna", "effort": "high" }),
            "high",
        ),
        (json!({ "model": "xai/grok-4.6", "effort": "none" }), "none"),
        (
            json!({ "model": "openai/gpt-7-unlisted", "effort": "max" }),
            "max",
        ),
        (
            json!({ "source": "local_proxy", "model": "xai/grok-4.6", "effort": "xhigh" }),
            "xhigh",
        ),
    ];
    for (body, effort) in accepted {
        let (status, patched) = h.patch(&ada, &bot, body.clone()).await;
        assert_eq!(status, 200, "{body}: {patched}");
        assert_eq!(patched["effort"], effort, "{body}: {patched}");
    }
    assert_eq!(h.row(&ada, &bot).await["effort"], "xhigh");
}

/// NEVER A REWRITE: an effort saved where its model listed no levels stays as it is when the Bot
/// moves to a model that does not list it, and on every other edit, since neither names it.
#[tokio::test]
async fn a_stored_effort_is_kept_by_a_repin_and_by_an_edit_that_does_not_name_it() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let bot = h.hire(&ada).await;
    let saved = json!({ "source": "gateway", "model": "xai/grok-4.6", "effort": "none" });
    assert_eq!(h.patch(&ada, &bot, saved).await.0, 200);
    let (status, moved) = h
        .patch(&ada, &bot, json!({ "model": "openai/gpt-6-luna" }))
        .await;
    assert_eq!(status, 200, "{moved}");
    assert_eq!(moved["effort"], "none", "{moved}");
    let (status, renamed) = h.patch(&ada, &bot, json!({ "name": "Grace" })).await;
    assert_eq!(status, 200, "{renamed}");
    assert_eq!(renamed["effort"], "none", "{renamed}");
}

/// The default for new bots is held to its model's levels on its own door, refused whole; a
/// level it lists, `inherit`, and a model with none or that nothing lists are taken.
#[tokio::test]
async fn a_default_for_new_bots_is_refused_an_effort_its_model_does_not_list() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    h.on_the_proxy(&ada).await;
    let default = |source: &str, model: &str, effort: &str| {
        json!({ "kind": "local_proxy", "localModel": "xai/grok-4.6",
                "newBotDefault": { "source": source, "model": model, "effort": effort } })
    };
    let refused = [
        (
            default("local_proxy", "gpt-6-luna", "none"),
            "newBotDefault.effort: gpt-6-luna takes low, medium, high, xhigh or max, not \"none\"",
        ),
        (
            default("gateway", "openai/gpt-6-luna", "max"),
            "newBotDefault.effort: openai/gpt-6-luna takes low, medium or high, not \"max\"",
        ),
    ];
    for (body, why) in refused {
        let (status, said) = h.set(&ada, body.clone()).await;
        assert_eq!(status, 400, "{body}: {said}");
        assert_eq!(said, json!({ "error": why }));
    }
    let (status, read) = h.send(&ada, reqwest::Method::GET, ROUTE, None).await;
    assert_eq!(status, 200, "{read}");
    assert_eq!(read["newBotDefault"], Value::Null, "refused whole: {read}");
    assert_eq!(
        read["localModel"], "gpt-6-luna",
        "nothing beside it saved: {read}"
    );

    let accepted = [
        default("local_proxy", "gpt-6-luna", "max"),
        default("local_proxy", "gpt-6-luna", "inherit"),
        default("gateway", "openai/gpt-6-luna", "high"),
        default("gateway", "xai/grok-4.6", "none"),
        default("gateway", "openai/gpt-7-unlisted", "max"),
    ];
    for body in accepted {
        let (status, saved) = h.set(&ada, body.clone()).await;
        assert_eq!(status, 200, "{body}: {saved}");
        assert_eq!(saved["newBotDefault"], body["newBotDefault"], "{saved}");
    }
}

/// The fallback that answers while the relay is off (#332) is the gateway's, so its effort is held
/// to the gateway's levels for its model, whatever the person's own plan lists under an id, and
/// it is refused whole: the switch saved beside it is not saved either.
#[tokio::test]
async fn a_fallback_for_the_relay_is_refused_an_effort_its_gateway_model_does_not_list() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    h.on_the_proxy(&ada).await;
    // The switch is read from the person's computers, off with none enrolled: one enrolled, on,
    // is what shows the refused save switched nothing.
    let enrol = Some(json!({ "label": "Ada's Mac" }));
    let post = reqwest::Method::POST;
    let (status, enrolled) = h.send(&ada, post, "/local-exec/daemon", enrol).await;
    assert_eq!(status, 200, "{enrolled}");
    let fallback = |model: &str, effort: &str| {
        json!({ "kind": "local_proxy", "relayEnabled": false,
                "planFallback": { "model": model, "effort": effort } })
    };
    let (status, said) = h.set(&ada, fallback("openai/gpt-6-luna", "max")).await;
    assert_eq!(status, 400, "{said}");
    let why = "planFallback.effort: openai/gpt-6-luna takes low, medium or high, not \"max\"";
    assert_eq!(said, json!({ "error": why }));
    let (status, read) = h.send(&ada, reqwest::Method::GET, ROUTE, None).await;
    assert_eq!(status, 200, "{read}");
    assert_eq!(read["planFallback"], Value::Null, "refused whole: {read}");
    assert_eq!(
        read["relayEnabled"], true,
        "the switch beside it too: {read}"
    );

    // gpt-6-luna is the plan's id, which the gateway does not list: held to the gateway, nothing
    // known refuses a word there, though the plan's own luna has no `none`.
    let accepted = [
        fallback("openai/gpt-6-luna", "high"),
        fallback("openai/gpt-6-luna", "inherit"),
        fallback("gpt-6-luna", "none"),
        fallback("xai/grok-4.6", "max"),
    ];
    for body in accepted {
        let (status, saved) = h.set(&ada, body.clone()).await;
        assert_eq!(status, 200, "{body}: {saved}");
        assert_eq!(saved["planFallback"], body["planFallback"], "{saved}");
    }
}

/// `ultra` is opencodex's word and no gateway's, so it is taken only where the model's listing
/// names it: on a model whose levels stop at `max` it is refused like any unlisted level, and on
/// one that lists none, which takes every other word, it is refused in words of its own.
#[tokio::test]
async fn ultra_is_taken_only_where_the_models_listing_names_it() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    h.on_the_proxy(&ada).await;
    let bot = h.hire(&ada).await;
    let on = |model: &str| json!({ "source": "local_proxy", "model": model, "effort": "ultra" });
    let (status, patched) = h.patch(&ada, &bot, on("gpt-6-sol")).await;
    assert_eq!(status, 200, "{patched}");
    assert_eq!(patched["effort"], "ultra", "{patched}");
    let refused = [
        (
            "gpt-6-luna",
            "effort: gpt-6-luna takes low, medium, high, xhigh or max, not \"ultra\"",
        ),
        (
            "xai/grok-4.6",
            "effort: no listing of xai/grok-4.6 names \"ultra\", and it is taken only where one does",
        ),
    ];
    for (model, why) in refused {
        let (status, said) = h.patch(&ada, &bot, on(model)).await;
        assert_eq!(status, 400, "{model}: {said}");
        assert_eq!(said, json!({ "error": why }));
    }
    let row = h.row(&ada, &bot).await;
    assert_eq!(row["model"], "gpt-6-sol", "nothing refused applied: {row}");
    assert_eq!(row["effort"], "ultra", "{row}");
}

/// A WRITE READS THE LISTING THE PICKER WAS SHOWN. With nothing cached it asks the gateway once;
/// after that neither a write nor the picker asks again while the listing is fresh.
#[tokio::test]
async fn a_write_asks_the_gateway_only_when_it_has_no_listing_to_read() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let bot = h.hire(&ada).await;
    assert_eq!(h.gateway_asked(), 0);
    let body = json!({ "source": "gateway", "model": "openai/gpt-6-luna", "effort": "max" });
    assert_eq!(h.patch(&ada, &bot, body.clone()).await.0, 400);
    assert_eq!(h.gateway_asked(), 1, "nothing was cached: one lookup");
    assert_eq!(h.patch(&ada, &bot, body).await.0, 400);
    let (status, listing) = h.models(&ada, "?source=gateway").await;
    assert_eq!(status, 200, "{listing}");
    let body = json!({ "effort": "low" });
    assert_eq!(h.patch(&ada, &bot, body).await.0, 200);
    assert_eq!(
        h.gateway_asked(),
        1,
        "every read after it was the cached listing"
    );
}

/// A REFUSAL ONLY EVER NARROWS ON KNOWN DATA: a gateway that cannot be listed and a proxy that is
/// down know no levels, so a write is taken as it was before levels existed. `ultra` is not: no
/// listing names it, and it is taken only where one does.
#[tokio::test]
async fn an_effort_is_taken_when_its_models_levels_cannot_be_read() {
    let database_url = database_or_skip!();
    let h = harness_on(&database_url, Some("http://127.0.0.1:1")).await;
    let ada = h.person().await;
    let bot = h.hire(&ada).await;
    let server = json!({ "source": "gateway", "model": "openai/gpt-6-luna", "effort": "max" });
    let (status, patched) = h.patch(&ada, &bot, server).await;
    assert_eq!(status, 200, "{patched}");
    h.on_a_proxy_at(&ada, "http://127.0.0.1:1").await;
    let plan = json!({ "source": "local_proxy", "model": "gpt-6-luna", "effort": "none" });
    let (status, patched) = h.patch(&ada, &bot, plan).await;
    assert_eq!(status, 200, "{patched}");
    assert_eq!(patched["effort"], "none");
    let (status, said) = h.patch(&ada, &bot, json!({ "effort": "ultra" })).await;
    assert_eq!(status, 400, "{said}");
    let why =
        "effort: no listing of gpt-6-luna names \"ultra\", and it is taken only where one does";
    assert_eq!(said, json!({ "error": why }));
}
