//! Every renderable transcript shape, on demand, for nothing.
//!
//! Typing `help` at a coworker on a mock door lists the fixtures; typing a fixture's name appends
//! exactly the entries that fixture describes. It exists so the client's rendering can be worked
//! on without a provider, a key or a single token of spend — the shapes a person needs to *see*
//! are the ones a model is least reliable at producing on request.
//!
//! WHY THIS IS A TOOL AND NOT A BRANCH IN THE DOOR. `mock.rs` states the invariant plainly: the
//! mock door "emits `ModelDelta`s, the same vocabulary the real door emits, and gets no private
//! path through the projection. A bug this hides is therefore a bug in the door, not in anything
//! downstream of it." Appending entries straight from the door would spend exactly that property.
//! So the door only ever does what a real door can do — it calls a tool — and this module answers
//! that call the way `group.rs` answers `SendMessage`: a `LocalTool` records into a sink, and the
//! turn appends what it finds there. The fixtures reach the transcript through the same append
//! path as every real entry.
//!
//! PROVENANCE. Every shape below is transcribed, never invented — `docs/research/client-grok-bot.md`
//! §3.1-3.2 for the wire, and a read of the shipped V2 render sites for what actually draws. The
//! two disagree in places that matter, and where they do the render site wins and the divergence
//! is commented. `projectTranscriptCardEntries` rejects a whole entry when any field guard fails
//! and reports only a `rejectedCount` — so a fixture that renders as nothing is far more likely a
//! failed guard than a missing view. Do not "tidy" a field name here.

use std::sync::{Arc, Mutex};

use opengrok_tools::{ToolCall, ToolResult};
use serde_json::{Value, json};

use opengrok_core::id::{AccountId, CoworkerId};

/// The tool the mock door calls. Offered ONLY under a mock door — `enabled()` gates it — so no
/// real coworker is ever shown a tool that fabricates its own transcript.
pub const TOOL: &str = "mock_fixture";

/// Where `user-attachment` fixtures put their files. They must exist on disk: the client's
/// projection feeds `file_path` into the host's read/download bridge, so a path that resolves to
/// nothing renders as a broken chip rather than an absent one.
const FIXTURE_DIR: &str = "/tmp/opengrok-mock-fixtures";

/// Media on an `attachment` card must be a LOCAL PATH, never a URL. This is the trap that costs
/// an afternoon, so it is spelled out.
///
/// Two different checks have to agree, and they disagree about what a "url" is:
///
/// 1. The CARD classifies on the string's extension alone — no mime type, no content-type header,
///    no sniffing (`classifyAttachmentUrl`). Images `.avif .bmp .gif .ico .jpeg .jpg .png .svg
///    .webp`; video `.m4v .mov .mp4 .ogv .webm`.
/// 2. The HOST then resolves it, and `normalizeAttachmentSource` is where a URL dies:
///    `new URL(source)` succeeding and not being `file:` returns **null**, so `resolveMedia`
///    answers null before anything is fetched and the card draws its `data-state="missing"`
///    placeholder. An absolute path throws in `new URL` and is therefore returned as-is; a
///    `file:` URL is converted to a posix path. Those are the only two shapes that survive.
///
/// So a remote `https://` URL renders blank, and serving the bytes off our own origin renders
/// blank too — `http://127.0.0.1:1447/…` fails the identical check. A `data:` URL fails both
/// checks at once. Real bytes on local disk are the only thing that works.
///
/// Vendored rather than fetched: a fixture that needs the network to produce a file is a fixture
/// that fails offline, which is most of when it is wanted.
const IMAGE_BYTES: &[u8] = include_bytes!("fixtures/mock-image.png");
const VIDEO_BYTES: &[u8] = include_bytes!("fixtures/mock-video.mp4");
const AUDIO_BYTES: &[u8] = include_bytes!("fixtures/mock-audio.mp3");

/// Is the fixture surface open? Gated on the door being a mock, because the tool writes entries no
/// real turn could produce. Read from the environment rather than threaded through: the door is
/// chosen there too (`main.rs`), and `provision::local_docker_allowed` sets the precedent.
pub fn enabled() -> bool {
    matches!(
        std::env::var("OG_MODEL_DOOR").as_deref(),
        Ok("mock") | Ok("mock-tools") | Ok("mock-cards")
    )
}

/// The OpenAI function definition the mock door is shown.
pub fn schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": TOOL,
            "description": "Append a mock transcript fixture. Pass the name the person typed; \
                            `help` lists every fixture.",
            "parameters": {
                "type": "object",
                "properties": {
                    "fixture": { "type": "string", "description": "The fixture name, or `help`." }
                },
                "required": ["fixture"]
            }
        }
    })
}

/// A `LocalTool` that records the asked-for fixture, plus the sink the turn drains.
///
/// Synchronous, like every local tool — it records, it does not reach out. The entries are built
/// here (so an unknown name is refused in the tool result, where the model can read it) and
/// appended by `drain_into`, which has the store.
pub fn tool() -> (opengrok_harness::LocalTool, Arc<Mutex<Vec<Value>>>) {
    let sink: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let into = sink.clone();
    let handler: opengrok_harness::LocalTool = Arc::new(move |call: &ToolCall| {
        let asked = call
            .arguments
            .get("fixture")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_lowercase();

        // `help` answers in the tool result rather than appending, so the door can speak it as an
        // ordinary bubble. A person asking what is available should not have to read a tool card.
        if asked == "help" || asked.is_empty() {
            return ToolResult {
                call_id: call.id.clone(),
                ok: true,
                content: help_text(),
                awaiting_approval: false,
                awaiting_reason: None,
            };
        }

        match entries_for(&asked) {
            Some(entries) => {
                let count = entries.len();
                if let Ok(mut sink) = into.lock() {
                    sink.extend(entries);
                }
                ToolResult {
                    call_id: call.id.clone(),
                    ok: true,
                    content: format!("appended the `{asked}` fixture ({count} entries)"),
                    awaiting_approval: false,
                    awaiting_reason: None,
                }
            }
            // Fail closed and say why, with the way out in the same breath.
            None => ToolResult {
                call_id: call.id.clone(),
                ok: false,
                content: format!("no fixture named `{asked}`.\n\n{}", help_text()),
                awaiting_approval: false,
                awaiting_reason: None,
            },
        }
    });
    (handler, sink)
}

/// Append whatever the turn's fixture calls recorded. Called once the turn's rounds are done.
pub async fn drain_into(
    store: &opengrok_store::PgStore,
    coworker: &CoworkerId,
    account: &AccountId,
    sink: &Arc<Mutex<Vec<Value>>>,
    at_ms: i64,
) {
    let entries = match sink.lock() {
        Ok(mut sink) => std::mem::take(&mut *sink),
        Err(_) => return,
    };
    for entry in entries {
        if let Err(error) = store.append_gateway_entry(coworker, account, &entry, at_ms).await {
            tracing::warn!(%error, "mock fixture could not be appended");
        }
    }
}

/// Every fixture, grouped as the help text presents them. The single source of truth for what
/// exists: `help_text` renders this, and `entries_for` dispatches on it.
const CATALOGUE: &[(&str, &str, &str)] = &[
    // (name, group, one-line description)
    ("text", "cards", "the ordinary assistant bubble"),
    ("widget", "cards", "a choice card with four options"),
    ("cursor-agent", "cards", "the cloud-agent card"),
    ("email-draft", "cards", "an editable email draft"),
    ("slack-draft", "cards", "an editable Slack draft"),
    ("auto-review-approval", "cards", "the approval card (interactive)"),
    ("listener-connect", "cards", "connect-a-listener (github)"),
    ("secret-request", "cards", "a secret prompt"),
    ("connector", "cards", "the plugin-connect card"),
    ("connectors", "cards", "the multi-connect card"),
    ("permission", "cards", "local-tool-permission — the real permission block"),
    ("permission-request", "cards", "the retired read-only leaf (title only, NOT interactive)"),
    ("image", "media", "attachment card, remote .jpg"),
    ("video", "media", "attachment card, remote .webm"),
    ("box", "media", "attachment card, sand://box — the Computer card"),
    ("link", "media", "attachment card, non-media http URL (legacy-link)"),
    ("markdown", "files", "user-attachment, .md"),
    ("zip", "files", "user-attachment, .zip"),
    ("rust", "files", "user-attachment, .rs"),
    ("js", "files", "user-attachment, .js"),
    ("upload", "files", "user-attachment, .png (the media branch)"),
    ("audio", "files", "user-attachment, .mp3 — audio has no card path"),
    ("notice", "kinds", "a muted system line"),
    ("event", "kinds", "a timeline event (name-changed)"),
    ("thinking", "kinds", "a collapsible reasoning block"),
    ("computer-handoff", "kinds", "send-message + boxRequestId; blank without a box"),
    ("tool-running", "tools", "tool line — running"),
    ("tool-success", "tools", "tool line — success"),
    ("tool-error", "tools", "tool line — error"),
    ("tool-denied", "tools", "tool line — denied"),
    ("tool-rejected", "tools", "tool line — rejected"),
    ("tool-cancelled", "tools", "tool line — cancelled"),
    ("tool-background", "tools", "tool line — success + isBackground"),
    ("all", "bulk", "every fixture above, in order"),
];

/// What `help` says. Grouped, because a flat list of thirty names is not a menu.
pub fn help_text() -> String {
    let mut out = String::from(
        "Mock fixtures — type any name below to have it appended to this transcript. \
         No model is called and nothing is spent.\n",
    );
    for (group, title) in [
        ("cards", "Cards (send-message)"),
        ("media", "Attachment cards — classified by URL alone"),
        ("files", "User attachments — real files under /tmp/opengrok-mock-fixtures"),
        ("kinds", "Other entry kinds"),
        ("tools", "Tool lines — the seven statuses that draw distinctly"),
        ("bulk", "Everything"),
    ] {
        out.push_str(&format!("\n{title}\n"));
        for (name, in_group, description) in CATALOGUE {
            if *in_group == group {
                out.push_str(&format!("  {name} — {description}\n"));
            }
        }
    }
    out.push_str(
        "\nTwo traps worth knowing: a `.zip`/`.md`/`.rs`/`.js` URL on an attachment CARD renders \
         as a bare link, so those fixtures use user-attachment entries instead; and `data:` URLs \
         never render as media, so the image and video fixtures point at remote URLs your host \
         must be able to fetch.\n",
    );
    out
}

fn entry_id(name: &str) -> String {
    format!("mock-{name}-{}", uuid::Uuid::now_v7().simple())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// A `send-message` entry wrapping one card body.
fn card(name: &str, message: Value) -> Value {
    json!({
        "kind": "send-message",
        "id": entry_id(name),
        "message": message,
        "timestampMs": now_ms(),
    })
}

/// Write a fixture file and hand back its path. `user-attachment` projections feed `file_path`
/// into the host's read/download bridge, so the file has to be there.
fn fixture_file(name: &str, body: &[u8]) -> String {
    let path = format!("{FIXTURE_DIR}/{name}");
    if let Err(error) = std::fs::create_dir_all(FIXTURE_DIR) {
        tracing::warn!(%error, "could not create the mock fixture directory");
    }
    if let Err(error) = std::fs::write(&path, body) {
        tracing::warn!(%error, path, "could not write a mock fixture file");
    }
    path
}

/// A `user-attachment` entry. `file_path`/`file_name` really are snake_case on this kind while its
/// neighbours are camelCase — that is what the client writes, and normalising it breaks the chip.
/// Optional numerics are omitted rather than guessed: present-but-negative rejects the whole entry.
fn user_attachment(name: &str, file_name: &str, body: &[u8]) -> Value {
    let path = fixture_file(file_name, body);
    json!({
        "kind": "user-attachment",
        "id": entry_id(name),
        "file_path": path,
        "file_name": file_name,
        "byteSize": body.len() as i64,
        "timestampMs": now_ms(),
    })
}

/// A `tool-call` entry at one status. The client normalizes six input words onto seven rendered
/// values (`tool-results/model.ts:76-82`): pending→running, done→success, aborted→cancelled,
/// failed→error. These are the words that reach each DISTINCT look, so the set below is seven
/// fixtures rather than the six statuses our own `model.ts:276` lists.
fn tool_line(name: &str, status: &str, summary: &str, extra: Option<(&str, Value)>) -> Value {
    let mut entry = json!({
        "kind": "tool-call",
        "id": entry_id(name),
        "name": "shell",
        "status": status,
        "summary": summary,
        "args": { "command": "echo hello" },
        "timestampMs": now_ms(),
    });
    if let Some((key, value)) = extra {
        entry[key] = value;
    }
    entry
}

/// The entries one fixture appends, or `None` when there is no such fixture.
pub fn entries_for(name: &str) -> Option<Vec<Value>> {
    let entries = match name {
        // ---- cards: `send-message` with a `message.type` ----
        "text" => vec![card(
            "text",
            json!({ "type": "text", "content": "A plain assistant bubble from the mock door." }),
        )],
        // 1-6 options, each with a non-empty label, or the projector drops the entry.
        "widget" => vec![card(
            "widget",
            json!({
                "type": "widget",
                "widget": {
                    "prompt": "Which door should the next turn use?",
                    "helpText": "Nothing is spent either way.",
                    "allowCustom": true,
                    "options": [
                        { "label": "Gateway", "value": "gateway", "style": "primary" },
                        { "label": "Mock", "value": "mock", "style": "default" },
                        { "label": "Mock tools", "value": "mock-tools", "style": "default" },
                        { "label": "Stop", "value": "stop", "style": "danger" }
                    ]
                }
            }),
        )],
        "cursor-agent" => vec![card(
            "cursor-agent",
            json!({ "type": "cursor-agent", "bcId": "bc_mock_0001", "title": "Mock cloud agent" }),
        )],
        "email-draft" => vec![card(
            "email-draft",
            json!({
                "type": "email-draft",
                "draft": {
                    "to": ["someone@acme.test"],
                    "cc": ["cc@acme.test"],
                    "from": "firstrun@acme.test",
                    "subject": "A mock draft",
                    "body": "This draft came from the mock door. No mail was sent."
                }
            }),
        )],
        "slack-draft" => vec![card(
            "slack-draft",
            json!({
                "type": "slack-draft",
                "draft": {
                    "target": "#engineering",
                    "workspace": "acme",
                    "body": "A mock Slack draft. Nothing was posted."
                }
            }),
        )],
        "auto-review-approval" => vec![card(
            "auto-review-approval",
            json!({
                "type": "auto-review-approval",
                "approval": {
                    "requestId": format!("req_{}", uuid::Uuid::now_v7().simple()),
                    "summary": "Run `rm -rf ./build` on the coworker's box",
                    "status": "pending",
                    "surface": "shell"
                },
                "reason": "Deletes a directory tree.",
                "command": "rm -rf ./build"
            }),
        )],
        "listener-connect" => vec![card(
            "listener-connect",
            json!({ "type": "listener-connect", "platform": "github", "reason": "Mock fixture." }),
        )],
        "secret-request" => vec![card(
            "secret-request",
            json!({
                "type": "secret-request",
                "secretRequest": {
                    "label": "GITHUB_TOKEN",
                    "description": "A mock secret prompt — do not paste a real token."
                }
            }),
        )],
        "connector" => vec![card(
            "connector",
            json!({
                "type": "connector",
                "connector": "github",
                "reason": "Mock fixture.",
                "serverId": "mock-server"
            }),
        )],
        "connectors" => vec![card(
            "connectors",
            json!({ "type": "connectors", "connectors": ["github", "slack", "linear"] }),
        )],
        // THE INTERACTIVE PERMISSION BLOCK. Emitted as a `send-message` CARD and never as its own
        // entry kind: `transcript.tsx:974` returns null for `kind === "local-tool-permission"`, so
        // the entry-kind spelling renders nothing at all. `message.ask` must be a record
        // (`production/model.ts:414`) or the whole entry is rejected.
        "permission" | "local-tool-permission" => vec![card(
            "permission",
            json!({
                "type": "local-tool-permission",
                "ask": {
                    "requestId": format!("ask_{}", uuid::Uuid::now_v7().simple()),
                    "status": "pending",
                    "action": "shell",
                    "target": { "command": "git push --force" }
                }
            }),
        )],
        // The RETIRED leaf. It draws a wrapper div containing one span of `title` and nothing
        // else — no buttons, no request id, no callback — and returns null when `title` is blank.
        // Kept so the difference from `permission` above is visible side by side.
        "permission-request" => vec![card(
            "permission-request",
            json!({ "type": "permission-request", "title": "A retired permission-request leaf" }),
        )],

        // ---- attachment cards: classified on the URL alone ----
        "image" => vec![card(
            "image",
            json!({
                "type": "attachment",
                "url": fixture_file("mock-image.png", IMAGE_BYTES),
                "alt": "A mock image attachment"
            }),
        )],
        "video" => vec![card(
            "video",
            json!({
                "type": "attachment",
                "url": fixture_file("mock-video.mp4", VIDEO_BYTES),
                "alt": "A mock video attachment"
            }),
        )],
        // `sand://box` takes the box branch — the Computer card with its status badge.
        "box" => vec![card(
            "box",
            json!({ "type": "attachment", "url": "sand://box", "alt": "The coworker's computer" }),
        )],
        // An http(s) URL with no media extension takes the legacy-link branch and is handed to the
        // URL-card provider, which is a different renderer again.
        "link" => vec![card(
            "link",
            json!({ "type": "attachment", "url": "https://example.com/a-page", "alt": "A link" }),
        )],

        // ---- user attachments: the only path that draws a real file chip ----
        // An attachment CARD whose url ends `.md`/`.zip`/`.rs`/`.js` has no view: it falls through
        // to a bare `<a>` whose text is the raw URL. These kinds draw the chip instead.
        "markdown" => vec![user_attachment(
            "markdown",
            "mock-notes.md",
            b"# Mock artifact\n\nWritten by the mock fixture catalogue.\n",
        )],
        "zip" => vec![user_attachment("zip", "mock-bundle.zip", &zip_bytes())],
        "rust" => vec![user_attachment(
            "rust",
            "mock_sample.rs",
            b"fn main() {\n    println!(\"from the mock fixture\");\n}\n",
        )],
        "js" => vec![user_attachment(
            "js",
            "mock-sample.js",
            b"export const from = 'the mock fixture';\n",
        )],
        "upload" => vec![user_attachment("upload", "mock-pixel.png", &png_bytes())],

        // ---- other entry kinds ----
        // `messageText` reads `entry.content`, then `entry.text`, then `entry.message.content` —
        // in that order, and never `message.text`. Empty or missing text returns null and the
        // whole entry vanishes silently, so `content` is required in practice however optional it
        // looks. `durationMs` is kept only when > 0.
        "thinking" => vec![json!({
            "kind": "thinking",
            "id": entry_id("thinking"),
            "content": "Weighing two options, then picking the duller one.",
            "durationMs": 4200,
            "timestampMs": now_ms(),
        })],
        // There is no `kind: "computer-handoff"` on the wire. It is a `send-message` whose
        // TOP-LEVEL `boxRequestId` is present and whose `message.type` is not "attachment".
        //
        // ORDERING TRAP: that check runs BEFORE the card projector, so a stray `boxRequestId` on
        // any other fixture would silently turn it into a handoff instead of the card its
        // `message.type` names. `no_other_fixture_carries_a_box_request_id` holds that line.
        //
        // Least deterministic fixture here: it renders through live computer state, so it can
        // legitimately draw nothing when the account has no box.
        "computer-handoff" => vec![{
            let mut entry = card(
                "computer-handoff",
                json!({ "type": "text", "content": "Handing this to your computer." }),
            );
            entry["boxRequestId"] = json!(format!("req_{}", uuid::Uuid::now_v7().simple()));
            entry["boxInstruction"] = json!("Open the settings page");
            entry["boxResolution"] = Value::Null;
            entry
        }],
        // Audio has no card path at all: no extension classifies as audio, so an `.mp3` on an
        // attachment card takes the `file` branch and renders as a bare link. The host resolves
        // audio fine — it is the card side that cannot ask for it — so this is an attachment kind.
        "audio" => vec![user_attachment("audio", "mock-audio.mp3", AUDIO_BYTES)],

        "notice" => vec![json!({
            "kind": "notice",
            "id": entry_id("notice"),
            "text": "A muted system line from the mock fixture catalogue.",
            "timestampMs": now_ms(),
        })],
        "event" => vec![json!({
            "kind": "event",
            "id": entry_id("event"),
            "event": { "type": "name-changed", "name": "Renamed by a mock fixture" },
            "timestampMs": now_ms(),
        })],

        // ---- tool lines ----
        "tool-running" => vec![tool_line("tool-running", "running", "Running a command", None)],
        "tool-success" => vec![tool_line("tool-success", "success", "Command finished", None)],
        "tool-error" => vec![tool_line("tool-error", "error", "Command failed", None)],
        "tool-denied" => vec![tool_line("tool-denied", "denied", "Policy denied it", None)],
        "tool-rejected" => vec![tool_line("tool-rejected", "rejected", "The person said no", None)],
        "tool-cancelled" => vec![tool_line("tool-cancelled", "cancelled", "Cancelled", None)],
        // `background` is not a status you can send: it is `success` plus the flag.
        "tool-background" => vec![tool_line(
            "tool-background",
            "success",
            "Still running in the background",
            Some(("isBackground", json!(true))),
        )],

        "all" => {
            let mut all = Vec::new();
            for (fixture, group, _) in CATALOGUE {
                if *group == "bulk" {
                    continue;
                }
                if let Some(entries) = entries_for(fixture) {
                    all.extend(entries);
                }
            }
            all
        }

        _ => return None,
    };
    Some(entries)
}

/// The smallest valid PNG — a single transparent pixel. Inline bytes rather than a fetch: a
/// fixture that needs the network to produce a file is a fixture that fails offline.
fn png_bytes() -> Vec<u8> {
    const PIXEL: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    PIXEL.to_vec()
}

/// An empty but structurally valid zip (end-of-central-directory record only), so the chip has a
/// real archive to point at without vendoring one.
fn zip_bytes() -> Vec<u8> {
    const EMPTY: &[u8] = &[
        0x50, 0x4B, 0x05, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    EMPTY.to_vec()
}

#[cfg(test)]
mod tests {
    // The workspace denies `expect` in shipped code; a fixture that is missing in a test is a bug
    // in the catalogue, and panicking on it is the assertion.
    #![allow(clippy::expect_used)]

    use super::*;

    /// Every name the help text advertises must actually resolve. The catalogue and the dispatch
    /// are two lists that can drift, and a fixture that is offered but answers "no such fixture"
    /// is worse than one that was never listed.
    #[test]
    fn every_advertised_fixture_resolves() {
        for (name, _, _) in CATALOGUE {
            assert!(
                entries_for(name).is_some(),
                "`{name}` is in the catalogue but has no entries"
            );
        }
    }

    #[test]
    fn an_unknown_fixture_is_refused() {
        assert!(entries_for("no-such-fixture").is_none());
    }

    /// The permission BLOCK must be a card, never its own entry kind: the entry-kind spelling
    /// renders nothing (`transcript.tsx:974`), and `ask` must be a record or the entry is dropped.
    #[test]
    fn the_permission_block_is_a_card_with_a_record_ask() {
        let entries = entries_for("permission").expect("fixture");
        let entry = &entries[0];
        assert_eq!(entry["kind"], "send-message");
        assert_eq!(entry["message"]["type"], "local-tool-permission");
        assert!(entry["message"]["ask"].is_object());
    }

    /// Media has to satisfy BOTH checks: the card classifies on the extension, and the host's
    /// `normalizeAttachmentSource` returns null for anything that parses as a non-`file:` URL. So
    /// a drawable media fixture is an absolute path to a file that exists, ending in an extension
    /// the card recognises. Anything URL-shaped renders as a `missing` placeholder.
    #[test]
    fn media_fixtures_are_local_paths_to_real_files() {
        for (fixture, extensions) in [
            ("image", [".jpg", ".jpeg", ".png", ".gif", ".webp", ".svg"].as_slice()),
            ("video", [".webm", ".mp4", ".mov", ".m4v", ".ogv"].as_slice()),
        ] {
            let entries = entries_for(fixture).expect("fixture");
            let url = entries[0]["message"]["url"].as_str().expect("a url");
            assert!(url.starts_with('/'), "{fixture} must be an absolute path: {url}");
            // `new URL(path)` throws on an absolute path, which is exactly why it survives; a
            // scheme would make it parse, and a parsed non-`file:` URL resolves to null.
            assert!(
                !url.contains("://"),
                "{fixture} looks like a URL, so the host resolves it to null: {url}"
            );
            assert!(
                extensions.iter().any(|extension| url.ends_with(extension)),
                "{fixture} needs a media extension: {url}"
            );
            assert!(
                std::path::Path::new(url).exists(),
                "{fixture} points at a file that was not written: {url}"
            );
        }
    }

    /// Neither a `data:` URL nor a remote one survives `normalizeAttachmentSource`. Serving the
    /// bytes off our own origin fails the identical check, so `http://127.0.0.1` is no escape.
    #[test]
    fn no_fixture_ships_a_url_where_a_path_is_needed() {
        for (name, _, _) in CATALOGUE {
            let Some(entries) = entries_for(name) else {
                continue;
            };
            for entry in entries {
                let Some(url) = entry["message"]["url"].as_str() else {
                    continue;
                };
                // `sand://box` is the one deliberate exception: it is a sentinel the card matches
                // before any host resolution, not a source anything fetches.
                if url == "sand://box" || entry["message"]["type"] == "attachment" && url.starts_with("https://example.com") {
                    continue;
                }
                assert!(!url.starts_with("data:"), "`{name}` ships a data: URL");
                assert!(
                    !url.starts_with("http://") && !url.starts_with("https://"),
                    "`{name}` ships a remote URL the host resolves to null: {url}"
                );
            }
        }
    }

    /// `thinking` is read from `entry.content` — not `message.text`, not `message.content` — and
    /// an empty one makes the whole entry vanish.
    #[test]
    fn thinking_carries_top_level_content() {
        let entries = entries_for("thinking").expect("fixture");
        let entry = &entries[0];
        assert_eq!(entry["kind"], "thinking");
        assert!(entry["content"].as_str().is_some_and(|text| !text.is_empty()));
    }

    /// A handoff is a `send-message` with a top-level `boxRequestId`, and its `message.type` must
    /// not be `attachment` or it takes the attachment card's box branch instead.
    #[test]
    fn the_handoff_is_a_send_message_with_a_box_request_id() {
        let entries = entries_for("computer-handoff").expect("fixture");
        let entry = &entries[0];
        assert_eq!(entry["kind"], "send-message");
        assert!(entry["boxRequestId"].as_str().is_some_and(|id| !id.is_empty()));
        assert_ne!(entry["message"]["type"], "attachment");
    }

    /// THE ORDERING TRAP. The handoff check runs before the card projector, so a stray
    /// `boxRequestId` silently converts any card into a handoff. Exactly one fixture may carry it.
    #[test]
    fn no_other_fixture_carries_a_box_request_id() {
        for (name, group, _) in CATALOGUE {
            if *name == "computer-handoff" || *group == "bulk" {
                continue;
            }
            let Some(entries) = entries_for(name) else {
                continue;
            };
            for entry in entries {
                assert!(
                    entry.get("boxRequestId").is_none(),
                    "`{name}` carries boxRequestId and would render as a handoff"
                );
            }
        }
    }

    /// The seven rendered tool statuses, and `background` spelled as success plus the flag rather
    /// than as a status the client would not recognise.
    #[test]
    fn tool_lines_cover_the_rendered_statuses() {
        for (fixture, status) in [
            ("tool-running", "running"),
            ("tool-success", "success"),
            ("tool-error", "error"),
            ("tool-denied", "denied"),
            ("tool-rejected", "rejected"),
            ("tool-cancelled", "cancelled"),
        ] {
            let entries = entries_for(fixture).expect("fixture");
            assert_eq!(entries[0]["status"], status);
        }
        let background = entries_for("tool-background").expect("fixture");
        assert_eq!(background[0]["status"], "success");
        assert_eq!(background[0]["isBackground"], true);
    }

    /// `user-attachment` keeps snake_case `file_path`/`file_name` while its neighbours are
    /// camelCase. This is the field the chip resolves through; normalising it breaks the chip.
    #[test]
    fn user_attachments_keep_their_snake_case_fields() {
        for fixture in ["markdown", "zip", "rust", "js", "upload"] {
            let entries = entries_for(fixture).expect("fixture");
            let entry = &entries[0];
            assert_eq!(entry["kind"], "user-attachment");
            assert!(entry["file_path"].as_str().is_some_and(|p| !p.is_empty()));
            assert!(entry["file_name"].as_str().is_some_and(|n| !n.is_empty()));
        }
    }

    /// `all` is the whole catalogue and must not recurse into itself.
    #[test]
    fn all_covers_every_other_fixture() {
        let all = entries_for("all").expect("fixture");
        let others = CATALOGUE.iter().filter(|(_, group, _)| *group != "bulk").count();
        assert!(all.len() >= others, "expected at least {others} entries, got {}", all.len());
    }

    #[test]
    fn help_lists_every_fixture() {
        let help = help_text();
        for (name, _, _) in CATALOGUE {
            assert!(help.contains(name), "help omits `{name}`");
        }
    }
}
