//! The web console's HTML shell and pages — login, signup, and the message screens.
//!
//! Hand-written HTML with one shared dark shell, matching the Open Grok desktop brand: a
//! near-black ground, the smiley wordmark, a single centred card. This is deliberately NOT a
//! framework — a login form and a signup form do not need reactivity, and one styled shell keeps
//! every auth page looking like it belongs to the same product. The richer account/admin surfaces
//! (Leptos-on-Axum) render the interactive views; these entry pages stay plain and fast.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

pub(crate) fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The Open Grok mark: a friendly face in a rounded disc, inline so no asset fetch is needed.
const LOGO: &str = r##"<svg width="40" height="40" viewBox="0 0 100 100" fill="none" aria-hidden="true">
<rect width="100" height="100" rx="30" fill="#fff"/>
<circle cx="38" cy="46" r="7" fill="#111"/>
<circle cx="66" cy="46" r="7" fill="#111"/>
<path d="M36 64 Q50 74 64 64" stroke="#111" stroke-width="6" stroke-linecap="round" fill="none"/>
</svg>"##;

/// Wrap page content in the dark shell. `subtitle` sits under the wordmark; `body` is the card's
/// inner HTML (a form, a message).
pub fn shell(title: &str, subtitle: &str, body: &str) -> String {
    format!(
        r##"<!doctype html><html lang=en><head><meta charset=utf8>
<meta name=viewport content="width=device-width,initial-scale=1">
<title>{title} · Open Grok</title>
<style>
  :root{{ color-scheme: dark; }}
  *,*::before,*::after{{ box-sizing:border-box; }}
  body{{ margin:0; min-height:100vh; display:grid; place-items:center;
    font:16px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",system-ui,sans-serif;
    color:#ededef; background:#0a0a0b;
    background-image:radial-gradient(70rem 40rem at 50% -10%, #16161a 0%, #0a0a0b 60%); }}
  .card{{ width:min(92vw, 25rem); padding:2.5rem 2.25rem 2.25rem;
    background:#141417; border:1px solid #24242a; border-radius:18px;
    box-shadow:0 24px 60px -20px rgba(0,0,0,.7); }}
  .brand{{ display:flex; align-items:center; gap:.7rem; margin-bottom:.35rem; }}
  .brand b{{ font-size:1.35rem; font-weight:700; letter-spacing:-.01em; }}
  .sub{{ color:#9a9aa2; font-size:.95rem; margin:0 0 1.6rem; }}
  label{{ display:block; font-size:.82rem; color:#b9b9c2; margin:1rem 0 .4rem; font-weight:500; }}
  input{{ width:100%; padding:.7rem .8rem; font-size:1rem; color:#f5f5f7;
    background:#0e0e10; border:1px solid #2a2a31; border-radius:10px; outline:none;
    transition:border-color .15s, box-shadow .15s; }}
  input:focus{{ border-color:#5b6cff; box-shadow:0 0 0 3px rgba(91,108,255,.22); }}
  button{{ width:100%; margin-top:1.6rem; padding:.75rem 1rem; font-size:1rem; font-weight:600;
    color:#0a0a0b; background:#fff; border:0; border-radius:10px; cursor:pointer;
    display:inline-flex; align-items:center; justify-content:center; gap:.4rem;
    transition:transform .06s ease, background .15s; }}
  button:hover{{ background:#ececf2; }}
  button:active{{ transform:translateY(1px); }}
  .err{{ margin:.2rem 0 0; padding:.6rem .75rem; border-radius:9px; font-size:.88rem;
    color:#ffd0d0; background:rgba(220,60,60,.14); border:1px solid rgba(220,60,60,.35); }}
  .msg{{ color:#c7c7cf; font-size:.98rem; margin:.4rem 0 0; }}
  .foot{{ margin-top:1.5rem; font-size:.85rem; color:#7c7c86; text-align:center; }}
  .foot a{{ color:#a9b2ff; text-decoration:none; }}
  .foot a:hover{{ text-decoration:underline; }}
</style></head><body>
  <main class=card>
    <div class=brand>{LOGO}<b>Open Grok</b></div>
    <p class=sub>{subtitle}</p>
    {body}
  </main>
</body></html>"##,
        title = escape(title),
        subtitle = escape(subtitle),
    )
}

fn html(status: StatusCode, page: String) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        page,
    )
        .into_response()
}

/// The styled sign-in card. Preserves the hidden PKCE fields exactly; only the look changes.
pub fn login(challenge: &str, uuid: &str, error: Option<&str>) -> Response {
    let err = error
        .map(|message| format!("<p class=err>{}</p>", escape(message)))
        .unwrap_or_default();
    let body = format!(
        r##"<form method=post action="/loginDeepControl">
  <input type=hidden name=challenge value="{challenge}">
  <input type=hidden name=uuid value="{uuid}">
  {err}
  <label for=email>Email</label>
  <input id=email name=email type=email autocomplete=username required autofocus>
  <label for=password>Password</label>
  <input id=password name=password type=password autocomplete=current-password required>
  <button type=submit>Sign in <span aria-hidden=true>&rarr;</span></button>
</form>
<p class=foot><a href="/forgot-password">Forgot your password?</a></p>"##,
        challenge = escape(challenge),
        uuid = escape(uuid),
    );
    html(
        StatusCode::OK,
        shell("Sign in", "Your team of always-on agents.", &body),
    )
}

/// The styled signup card. The invite code is prefilled from the link and read-only when present,
/// so a person who clicked an invite does not retype it; a person who pasted the URL without one
/// gets an editable field to paste into.
pub fn signup(code: Option<&str>, error: Option<&str>) -> Response {
    let err = error
        .map(|message| format!("<p class=err>{}</p>", escape(message)))
        .unwrap_or_default();
    let (code_value, code_attr) = match code {
        Some(code) if !code.is_empty() => (escape(code), "readonly"),
        _ => (String::new(), ""),
    };
    let body = format!(
        r##"<form method=post action="/signup">
  {err}
  <label for=code>Invite code</label>
  <input id=code name=code value="{code_value}" {code_attr} placeholder="inv_…" required>
  <label for=first>First name</label>
  <input id=first name=firstName type=text autocomplete=given-name>
  <label for=last>Last name</label>
  <input id=last name=lastName type=text autocomplete=family-name>
  <label for=email>Work email</label>
  <input id=email name=email type=email autocomplete=email required>
  <label for=password>Password</label>
  <input id=password name=password type=password autocomplete=new-password minlength=8 required>
  <button type=submit>Create account <span aria-hidden=true>&rarr;</span></button>
</form>
<p class=foot>Already have an account? Your admin will point you to sign in.</p>"##,
    );
    html(
        StatusCode::OK,
        shell("Sign up", "Join your organization on Open Grok.", &body),
    )
}

/// The "send me a reset link" card. `mailer` false ⇒ this server cannot email, and the card says
/// so instead of pretending a link is on its way — the operator resets from the shell instead.
pub fn forgot_password(mailer: bool, error: Option<&str>) -> Response {
    let err = error
        .map(|message| format!("<p class=err>{}</p>", escape(message)))
        .unwrap_or_default();
    let body = if mailer {
        format!(
            r##"<form method=post action="/forgot-password">
  {err}
  <label for=email>Email</label>
  <input id=email name=email type=email autocomplete=username required autofocus>
  <button type=submit>Send reset link <span aria-hidden=true>&rarr;</span></button>
</form>
<p class=foot>We will email a link that works once and expires in an hour.</p>"##
        )
    } else {
        r##"<p class=msg>This server is not set up to send email, so it cannot mail a reset link.
Ask your administrator to reset your password — they can do it from the server's shell.</p>"##
            .to_string()
    };
    html(
        StatusCode::OK,
        shell("Forgot password", "Reset your Open Grok password.", &body),
    )
}

/// The "choose a new password" card the emailed link opens. The token rides as a hidden field so
/// the POST carries it without the browser ever needing script.
pub fn reset_password(token: &str, error: Option<&str>) -> Response {
    let err = error
        .map(|message| format!("<p class=err>{}</p>", escape(message)))
        .unwrap_or_default();
    let body = format!(
        r##"<form method=post action="/reset-password">
  <input type=hidden name=token value="{token}">
  {err}
  <label for=password>New password</label>
  <input id=password name=password type=password autocomplete=new-password minlength=8 required autofocus>
  <label for=confirm>Confirm new password</label>
  <input id=confirm name=confirm type=password autocomplete=new-password minlength=8 required>
  <button type=submit>Set password <span aria-hidden=true>&rarr;</span></button>
</form>"##,
        token = escape(token),
    );
    html(
        StatusCode::OK,
        shell("Reset password", "Choose a new password.", &body),
    )
}

/// The MCP door's OAuth sign-in card: a client (Claude Code) is asking to use a coworker, and the
/// person signs in first. `hidden` is the authorization request carried through as hidden inputs
/// (already escaped by the caller), so the POST re-validates exactly what the client asked for.
/// Where the client comes from, as its own element the name cannot crowd out: the name is the
/// client's word for itself; the origin is what the server resolved it from. `None` for a
/// registered client, which has no origin to show.
fn origin_line(origin: Option<&str>) -> String {
    origin
        .map(|host| format!("<p class=msg>From <code>{}</code></p>", escape(host)))
        .unwrap_or_default()
}

pub fn oauth_login(
    client_name: &str,
    origin: Option<&str>,
    hidden: &str,
    error: Option<&str>,
) -> Response {
    let err = error
        .map(|message| format!("<p class=err>{}</p>", escape(message)))
        .unwrap_or_default();
    let origin = origin_line(origin);
    let body = format!(
        r##"<form method=post action="/oauth/mcp/authorize">
  {hidden}
  {err}
  <p class=msg><b>{client}</b> wants to use one of your coworkers. Sign in to choose which.</p>
  {origin}
  <label for=email>Email</label>
  <input id=email name=email type=email autocomplete=username required autofocus>
  <label for=password>Password</label>
  <input id=password name=password type=password autocomplete=current-password required>
  <button type=submit>Sign in <span aria-hidden=true>&rarr;</span></button>
</form>"##,
        client = escape(client_name),
    );
    html(
        StatusCode::OK,
        shell("Sign in", "Connect a tool to your coworker.", &body),
    )
}

/// The consent card: pick the coworker whose toolbox the client gets. `consent` is the signed
/// token that says who is choosing; `coworkers` are (id, name) pairs from the person's own roster.
pub fn oauth_consent(
    client_name: &str,
    origin: Option<&str>,
    hidden: &str,
    consent: &str,
    coworkers: &[(String, String)],
) -> Response {
    let origin = origin_line(origin);
    let options: String = coworkers
        .iter()
        .map(|(id, name)| format!("<option value=\"{}\">{}</option>", escape(id), escape(name)))
        .collect();
    let body = if coworkers.is_empty() {
        "<p class=msg>You have no coworkers yet. Hire one in Open Grok, then try again.</p>"
            .to_string()
    } else {
        format!(
            r##"<form method=post action="/oauth/mcp/authorize">
  {hidden}
  <input type=hidden name=consent value="{consent}">
  <p class=msg><b>{client}</b> will run tools as the coworker you choose, on that coworker's own
  computer, under your policy. You can revoke this key any time from the coworker's key list.</p>
  {origin}
  <label for=coworker>Coworker</label>
  <select id=coworker name=coworker required style="width:100%;padding:.7rem .8rem;font-size:1rem;color:#f5f5f7;background:#0e0e10;border:1px solid #2a2a31;border-radius:10px">{options}</select>
  <button type=submit>Allow <span aria-hidden=true>&rarr;</span></button>
</form>
<p class=foot>Not you? Close this tab; nothing has been granted.</p>"##,
            consent = escape(consent),
            client = escape(client_name),
        )
    };
    html(
        StatusCode::OK,
        shell("Allow access", "Connect a tool to your coworker.", &body),
    )
}

/// A plain message card — verification results, sign-in success, the like.
pub fn message(status: StatusCode, title: &str, message: &str) -> Response {
    let body = format!("<p class=msg>{}</p>", escape(message));
    html(status, shell(title, "", &body))
}
