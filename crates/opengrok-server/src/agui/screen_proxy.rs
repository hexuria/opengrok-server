//! A Local VM's live desktop, served through this server (#191).
//!
//! A Docker box publishes noVNC on `127.0.0.1` only, and must: widening the bind would put the
//! exec, host and egress-bearer sockets on the network with it. But the person's app is on
//! another machine whenever the gateway is not loopback — which the client insists on — so a
//! loopback `vncUrl` painted nothing but the PNG fallback. The page and its websocket are proxied
//! here instead, and `vncUrl` names this server.
//!
//! THE TICKET IS IN THE PATH. A webview loading `vncUrl` sends no Authorization header, and noVNC
//! fetches its own scripts by relative path and its websocket by the `path` it is given: a token
//! in the query would be gone by the first asset. The ticket names one account, one coworker and
//! one box, and expires; the account's right to the coworker is checked again on every request,
//! and the box's place as its computer, with its port, every `PLACE_FOR` (`UPSTREAMS`), so a
//! retired coworker stops the next asset and reconnect, and a computer that moved within seconds.
//!
//! THE TICKET IS STABLE WITHIN A WINDOW. The pane polls the status, and a `vncUrl` that changed on
//! every poll would reload the desktop each time; the same claims sign to the same token, so the
//! URL changes once per window, not once per poll.
//!
//! THE BOX'S PAGES ARE HOSTILE UNTIL PROVEN OTHERWISE. The model has a shell on the box, a per-org
//! box is shared by colleagues, and their skill scripts are copied there to run — any of them can
//! replace what answers on 6080. Yet the page is served from THIS origin, which also serves
//! `/console` and honours its `og_access` cookie. See `confined` for what keeps the one from
//! acting as the other, and `DIAL` for why a redirect is never followed.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use opengrok_core::id::{AccountId, CoworkerId};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::agui::AgUiState;
use crate::auth::TokenMinter;
pub use crate::seams::screen_tickets_due;
pub use opengrok_box::viewer::public_origin;
use opengrok_box::viewer::{Dialled, Fetched, loopback_port, socket_path};

/// What a ticket is for; an access token (same key) never verifies as one, nor this as that.
const PURPOSE: &str = "screen";
/// A ticket lives between one and two of these.
const WINDOW_SECONDS: i64 = 6 * 60 * 60;

/// Each ticket's box, its noVNC port, the box's `Computer::generation` it was learned under and
/// when, until the ticket expires: asking on each of a page's ~80 files and its websocket was 160
/// `docker` processes and 500 queries at once, 3.7 s a file and 5 s to paint (3 Oct 2026). A port
/// is the box's only in that generation: a stop, start, rebuild or removal frees it for anything
/// to take. Stamped once the check that found it has finished, never before, so one cut short
/// leaves the next request to make its own.
/// The last field is the websocket path on that port: a Bot's own screen is reached by its token
/// (#376), the shared screen by the bare path.
type Upstream = (String, u16, i64, Instant, u64, String);
pub(crate) static UPSTREAMS: LazyLock<Mutex<HashMap<String, Upstream>>> =
    LazyLock::new(Mutex::default);

/// How long a ticket's box is taken to still be its coworker's computer (`scoped_box_row_for`,
/// five queries) between checks; past it, the place and the port are both asked again.
const PLACE_FOR: Duration = Duration::from_secs(5);

fn remember(ticket: &str, box_id: &str, (port, path): (u16, &str), until: i64, generation: u64) {
    let now = chrono::Utc::now().timestamp();
    if let Ok(mut known) = UPSTREAMS.lock() {
        known.retain(|_, upstream| upstream.2 > now);
        let upstream = (
            box_id.into(),
            port,
            until,
            Instant::now(),
            generation,
            path.into(),
        );
        known.insert(ticket.into(), upstream);
    }
}

/// Forget the port of every ticket for a box stopped, reset or rebuilt here, or whose port went
/// dead or that is no longer its coworker's: the old ports may by then be another box's.
pub fn forget_box(box_id: &str) {
    if let Ok(mut known) = UPSTREAMS.lock() {
        known.retain(|_, (bx, ..)| bx != box_id);
    }
}

#[derive(Serialize, Deserialize)]
struct Ticket {
    sub: String,
    cw: String,
    bx: String,
    exp: i64,
    purpose: String,
}

/// `local_page` (the box's own noVNC URL on this host) as a URL on `origin`, carrying the same
/// noVNC settings plus the websocket path under the ticket. `None` when it cannot be signed.
pub fn proxied_page(
    minter: &TokenMinter,
    origin: &str,
    account: &AccountId,
    coworker: &CoworkerId,
    box_id: &str,
    local_page: &str,
    now_seconds: i64,
) -> Option<String> {
    let (_, settings) = local_page.split_once('?')?;
    let ticket = minter
        .mint_claims(&Ticket {
            sub: account.as_str().to_string(),
            cw: coworker.as_str().to_string(),
            bx: box_id.to_string(),
            exp: (now_seconds.div_euclid(WINDOW_SECONDS) + 2) * WINDOW_SECONDS,
            purpose: PURPOSE.to_string(),
        })
        .ok()?;
    let base = format!("coworkers/{}/computer/vnc/{ticket}", coworker.as_str());
    Some(format!(
        "{origin}/{base}/vnc.html?{settings}&path={base}/websockify"
    ))
}

/// The box behind a ticket, and its noVNC port — re-authorised now, not at mint time. The port
/// is the remembered one while the box is in the generation it was learned in and its place was
/// checked within `PLACE_FOR`; otherwise the place and the port are both asked again, the
/// generation read first, so a stop or removal during the asking is seen by the next request.
async fn upstream_for(state: &AgUiState, cw: &str, ticket: &str) -> Option<(u16, String, String)> {
    let claims: Ticket = state.auth.minter.verify_claims(ticket).ok()?;
    if claims.purpose != PURPOSE || claims.cw != cw {
        return None;
    }
    let account = AccountId::from_stored(claims.sub);
    let coworker = CoworkerId::from_stored(claims.cw);
    let Ok(true) = crate::agui::routes::owned_coworker(state, &account, &coworker).await else {
        return None;
    };
    let docker = crate::agui::provision::provider_for(state, None, "local-docker").await?;
    let generation = docker.generation(&claims.bx);
    if let Ok(known) = UPSTREAMS.lock()
        && let Some((_, port, _, checked, of, path)) = known.get(ticket)
        && *of == generation
        && checked.elapsed() < PLACE_FOR
    {
        return Some((*port, claims.bx, path.clone()));
    }
    let row = crate::agui::provision::scoped_box_row_for(state, &account, &coworker).await;
    let Some(row) = row.filter(|row| row.box_id == claims.bx && row.kind == "local-docker") else {
        forget_box(&claims.bx);
        return None;
    };
    // The Bot's own screen when it was told to use one (#376), else the shared one.
    let which = crate::agui::provision::screen_for(state, &account, coworker.as_str()).await;
    let page = docker.screen_url(&row.box_id, &which).await.ok()??;
    let port = loopback_port(&page)?;
    let path = socket_path(&page);
    remember(ticket, &row.box_id, (port, &path), claims.exp, generation);
    Some((port, row.box_id, path))
}

/// The box behind a Bot's computer and that Bot's screen page, for a screen reached through this
/// server: its Docker box only, which is on this host's loopback.
async fn screen_page(
    state: &AgUiState,
    account: &AccountId,
    bot: &CoworkerId,
) -> Option<(String, String)> {
    let row = crate::agui::provision::scoped_box_row_for(state, account, bot).await?;
    let row = Some(row).filter(|row| row.kind == "local-docker")?;
    let docker = crate::agui::provision::provider_for(state, None, "local-docker").await?;
    let which = crate::agui::provision::screen_for(state, account, bot.as_str()).await;
    let page = docker.screen_url(&row.box_id, &which).await.ok()??;
    Some((row.box_id, page))
}

/// The SSH gateway's check, at every forward (#386): the code's account still has the Bot, the Bot
/// is still on the box the code was minted for, and that Bot's screen, as the proxy finds it.
pub struct SshScreens(pub AgUiState);

#[async_trait::async_trait]
impl opengrok_screen_ssh::Screens for SshScreens {
    async fn screen_for(&self, grant: &opengrok_screen_ssh::Grant) -> Option<(u16, String)> {
        let account = AccountId::from_stored(grant.account.clone());
        let bot = CoworkerId::from_stored(grant.coworker.clone());
        let Ok(true) = crate::agui::routes::owned_coworker(&self.0, &account, &bot).await else {
            return None;
        };
        let (box_id, page) = screen_page(&self.0, &account, &bot).await?;
        if box_id != grant.box_id {
            return None;
        }
        Some((loopback_port(&page)?, socket_path(&page)))
    }
}

/// `POST /coworkers/{id}/computer/ssh` — a code that reaches this Bot's screen over SSH for
/// fifteen minutes, with the command that uses it and the screen's VNC password (#386).
pub async fn ssh_code(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(bot): Path<String>,
) -> Response {
    let (account, bot) = match crate::agui::routes::owner_of(&state, &headers, bot).await {
        Ok(owner) => owner,
        Err(refusal) => return refusal,
    };
    let refused = |status, why: &str| (status, why.to_string()).into_response();
    if opengrok_screen_ssh::listening().is_none() {
        return refused(StatusCode::SERVICE_UNAVAILABLE, opengrok_screen_ssh::OFF);
    }
    let Some((box_id, page)) = screen_page(&state, &account, &bot).await else {
        return refused(StatusCode::CONFLICT, opengrok_screen_ssh::NO_SCREEN);
    };
    let (account, coworker) = (account.to_string(), bot.to_string());
    let grant = opengrok_screen_ssh::Grant {
        account,
        coworker,
        box_id,
    };
    let now = chrono::Utc::now().timestamp_millis();
    let origin = public_origin(&state.auth.public_url, &headers).unwrap_or_default();
    match opengrok_screen_ssh::offer(grant, &origin, &page, now) {
        Some(offer) => axum::Json(offer).into_response(),
        None => refused(
            StatusCode::SERVICE_UNAVAILABLE,
            "a code could not be made now",
        ),
    }
}

/// `GET /coworkers/{id}/computer/vnc/{ticket}/{*rest}` — noVNC's files, and its websocket.
/// Every refusal is the same 404: a guessed ticket learns nothing about which part was wrong.
/// A remembered port nothing answers on is asked for once more, fresh, before giving up.
pub async fn serve(
    State(state): State<AgUiState>,
    Path((coworker_id, ticket, rest)): Path<(String, String, String)>,
    mut request: Request,
) -> Response {
    let upgrade = request
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"));
    for _ in 0..2 {
        let Some((port, box_id, path)) = upstream_for(&state, &coworker_id, &ticket).await else {
            return confined((StatusCode::NOT_FOUND, "no such screen").into_response());
        };
        if upgrade {
            match tunnel(port, &path, request).await {
                Ok(response) => return response,
                Err(unsent) => request = *unsent,
            }
        } else if let Some(fetched) = fetch(port, &rest).await {
            return confined(fetched);
        }
        forget_box(&box_id);
    }
    let silent = (StatusCode::BAD_GATEWAY, SILENT).into_response();
    if upgrade { silent } else { confined(silent) }
}

const SILENT: &str = "the computer's screen did not answer";
const REDIRECTED: &str =
    "the computer's screen answered with a redirect, which this server does not follow";

/// One of noVNC's files from the box on `port`, with only its status, body and `Content-Type`
/// (`viewer::fetch_file`, which also says why a redirect is never followed). `None` when nothing
/// on `port` took the request.
async fn fetch(port: u16, rest: &str) -> Option<Response> {
    let refused = |status, why| Some((status, why).into_response());
    match opengrok_box::viewer::fetch_file(port, rest).await {
        Fetched::NoAnswer => None,
        Fetched::NotFound => refused(StatusCode::NOT_FOUND, "no such file"),
        Fetched::Silent => refused(StatusCode::BAD_GATEWAY, SILENT),
        Fetched::Redirected => refused(StatusCode::BAD_GATEWAY, REDIRECTED),
        Fetched::Got { status, kind, body } => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut response = (status, body).into_response();
            if let Some(kind) = kind {
                response.headers_mut().insert(header::CONTENT_TYPE, kind);
            }
            Some(response)
        }
    }
}

/// Every answer on the proxy's path as a page that cannot act as the person (`viewer::confine`).
fn confined(mut response: Response) -> Response {
    opengrok_box::viewer::confine(response.headers_mut());
    response
}

/// Replay the browser's websocket handshake to the box and splice the two sockets. REPLAYED, NOT
/// REMADE: the box answers the key the browser sent, so its `Sec-WebSocket-Accept` is the one the
/// browser checks, and the frames pass through untouched in both directions. `Err` hands the
/// request back when nothing on `port` took the connection.
async fn tunnel(port: u16, path: &str, mut request: Request) -> Result<Response, Box<Request>> {
    let refused = |why: &'static str| Ok((StatusCode::BAD_GATEWAY, why).into_response());
    let [key, protocol] = ["sec-websocket-key", "sec-websocket-protocol"].map(|name| {
        let value = request.headers().get(name)?.to_str().ok()?;
        Some(value.to_string())
    });
    let Some(key) = key else {
        return Ok((StatusCode::BAD_REQUEST, "not a websocket handshake").into_response());
    };
    let opened = opengrok_box::viewer::open_socket(port, path, &key, protocol.as_deref()).await;
    let (mut upstream, early, accept, chosen) = match opened {
        Dialled::NoAnswer => return Err(Box::new(request)),
        Dialled::Refused(why) => return refused(why),
        Dialled::Open(open) => (open.upstream, open.early, open.accept, open.protocol),
    };
    let upgraded = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        let Ok(upgraded) = upgraded.await else {
            return;
        };
        let mut client = hyper_util::rt::TokioIo::new(upgraded);
        // Bytes the box sent right behind its 101 (RFB speaks first) belong to the browser.
        if !early.is_empty() && client.write_all(&early).await.is_err() {
            return;
        }
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    });
    let mut response = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "websocket")
        .header("sec-websocket-accept", accept);
    if let Some(chosen) = chosen {
        response = response.header("sec-websocket-protocol", chosen);
    }
    response.body(Body::empty()).map_or_else(
        |_| refused("the computer's screen refused the handshake"),
        Ok,
    )
}

#[cfg(test)]
#[path = "../../tests/unit/screen_proxy.rs"]
mod tests;
