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
//! CRAWLING THE RESULT. `timeline-event` and `permission-request` rows carry no `data-index` —
//! they sit outside the message-row virtualization — so a crawler keyed on `[data-index]` reports
//! them missing when they drew perfectly well. Walk the scroller's direct children instead. The
//! transcript is virtualized either way, so read it scrolled to the bottom. Measured 5 Sep 2026,
//! after an index-based pass produced a false "blank" for exactly those two kinds.
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

/// The four readers that have no other way to be exercised: the PDF viewer, the spreadsheet
/// viewer, the JSON reader and the mammoth `.docx` reader all reach their bytes through
/// `readAttachmentChunk` and draw nothing without a real file. Every one of these is a genuine
/// document — `file(1)` reports "PDF document, version 1.4, 1 pages" and "Microsoft Word 2007+" —
/// because a reader given a plausible-looking stub fails in a way that teaches nobody anything.
const PDF_BYTES: &[u8] = include_bytes!("fixtures/mock-report.pdf");
const CSV_BYTES: &[u8] = include_bytes!("fixtures/mock-table.csv");
const JSON_BYTES: &[u8] = include_bytes!("fixtures/mock-data.json");
const DOCX_BYTES: &[u8] = include_bytes!("fixtures/mock-doc.docx");

/// The text-family readers, and one adversary. `mock-page.html` carries a `<script>` and an
/// `<img onerror>`: the reader MUST show them as source. A markup showcase that accidentally proves
/// the renderer executes HTML is a finding, not a fixture — this is the one file where pass and
/// fail differ in a way that matters beyond cosmetics. The client routes `.html` to `CodeFilePage`
/// (`workspace/model.ts:170-182` → `file-viewer.tsx`), which renders React text nodes and never
/// `innerHTML`; the fixture exists so that stays true.
const HTML_BYTES: &[u8] = include_bytes!("fixtures/mock-page.html");
const TXT_BYTES: &[u8] = include_bytes!("fixtures/mock-notes.txt");
const YAML_BYTES: &[u8] = include_bytes!("fixtures/mock-config.yaml");
const SVG_BYTES: &[u8] = include_bytes!("fixtures/mock-shape.svg");
const MD_BYTES: &[u8] = include_bytes!("fixtures/mock-notes.md");
const JS_BYTES: &[u8] = include_bytes!("fixtures/mock-sample.js");
const RS_BYTES: &[u8] = include_bytes!("fixtures/mock_sample.rs");
// The archives. All four are REAL files rather than built in code, because the client now reads
// them structurally — a zip through its central directory, a tar through its headers, a gzip
// stream through `DecompressionStream` — so bytes that merely start correctly would list as
// nothing. `mock-bundle.zip` and `mock-bundle.tar.gz` hold the SAME four-file, two-folder tree,
// so the two listings can be read against each other. Regenerated by
// `scripts/make-archive-fixtures.py`, which is deterministic: fixed zip dates and `mtime=0` in
// both gzip headers, so a rebuild is not a diff.
const ZIP_BYTES: &[u8] = include_bytes!("fixtures/mock-bundle.zip");
const TARGZ_BYTES: &[u8] = include_bytes!("fixtures/mock-bundle.tar.gz");
const GZ_BYTES: &[u8] = include_bytes!("fixtures/mock-notes.md.gz");
// The ONE archive the client cannot list. 7z is unsupported, so it offers Download instead —
// which is a path with its own failure mode and no fixture until now. Signature plus a stub
// header: the client never parses past the magic, so a real 7z would prove nothing more. This is
// the opposite call from the pdf and docx, where the reader DOES parse and a stub fails inside it.
const SEVENZIP_BYTES: &[u8] = include_bytes!("fixtures/mock-bundle.7z");
// The office formats. Every one is a structurally complete package that opens in a real
// application — `textutil` reads the rtf and the odt, `file(1)` names each correctly — because a
// reader is only worth testing against a file that really holds the constructs it claims to
// render. A stub that opens and shows nothing teaches its author that the code works. Built by
// `scripts/make-document-fixtures.py`, which explains each choice at the part level.
const PPTX_BYTES: &[u8] = include_bytes!("fixtures/mock-deck.pptx");
const ODT_BYTES: &[u8] = include_bytes!("fixtures/mock-notes.odt");
const ODP_BYTES: &[u8] = include_bytes!("fixtures/mock-deck.odp");
const RTF_BYTES: &[u8] = include_bytes!("fixtures/mock-rich.rtf");
// The other two Download-path fixtures, alongside `sevenzip`. THE CLIENT REFUSES BOTH ON THE
// EXTENSION, before it reads a byte: legacy `.doc`/`.ppt` and iWork `.key`/`.pages`/`.numbers`
// have no browser-side reader, so the viewer diverts to Download without opening the file (told
// to us by the client session on 6 Sep 2026, correcting an earlier note here that said the doc
// was refused on its OLE magic and the pages on its package shape — neither is looked at).
//
// The bytes are still built honestly, and that is a deliberate second-order choice: `file(1)`
// names both correctly, so a person debugging a Download that should not have happened can tell
// a wrong fixture from a wrong branch. The magic and the `Index/`+`Metadata/` folders are
// therefore documentation and a hedge against a future sniff, not the current trigger.
//
// Do not "improve" either into something a reader can open: the refusal is the coverage.
const DOC_BYTES: &[u8] = include_bytes!("fixtures/mock-legacy.doc");
const PAGES_BYTES: &[u8] = include_bytes!("fixtures/mock-page.pages");

/// The markdown showcase, as one bubble — the SAME bytes as `mock-notes.md`, by `include_str!`,
/// so the text card and the file reader show one document and cannot drift apart. It is
/// GFM-complete on purpose: ATX headings 1–6 and both setext forms, every inline style, all three
/// link kinds plus an image, escapes and entities, a hard break, nested bullets/numbers/tasks with
/// both bullet markers, nested blockquotes, indented and fenced code in five languages, two tables
/// with all three alignments, all three rule spellings, and a footnote. The four chip schemes are
/// verbatim from `workspace/inline-chips.tsx` (pinned by `tests/frontend-inline-chips.test.mjs`);
/// `sand-msg:t1u` addresses turn 1's user message, which always exists. When one construct does not
/// draw, the reader wants to know WHICH — which is why nothing here is a sample of the set.
const SHOWCASE: &str = include_str!("fixtures/mock-notes.md");

/// KaTeX. `$$` on its own lines and `\[…\]` are display; `\(…\)` is inline
/// (`patched-ui/math.ts:3-5` rewrites the bracket forms to `$$`). SINGLE `$…$` IS NOT A
/// DELIMITER, which is why the last line is there: "$5 and $6" must stay currency. If it turns
/// into math, the delimiter set widened and that line is the alarm.
const KATEX: &str = r#"## Display math — `$$` on its own lines

$$
\int_0^\infty e^{-x^2}\,dx = \frac{\sqrt{\pi}}{2}
$$

## Display math — bracket form

\[ \sum_{k=1}^{n} k = \frac{n(n+1)}{2} \qquad \prod_{i=1}^{n} i = n! \]

## Inline math mid-sentence

Einstein's \(E = mc^2\), Euler's \(e^{i\pi} + 1 = 0\), and a limit \(\lim_{x \to 0} \frac{\sin x}{x} = 1\) all sit inside this line.

## Fractions, roots, powers

$$
\frac{a}{b} \quad \dfrac{a}{b} \quad \tfrac{a}{b} \quad \sqrt{2} \quad \sqrt[3]{x^3 + y^3} \quad x^{2^{n}} \quad a_{i,j} \quad \binom{n}{k}
$$

## Greek, blackboard, calligraphic, roman, bold

$$
\alpha \beta \gamma \delta \epsilon \zeta \eta \theta \lambda \mu \pi \sigma \phi \omega \quad \Gamma \Delta \Theta \Lambda \Pi \Sigma \Phi \Omega \quad \mathbb{R} \mathbb{N} \mathbb{Z} \quad \mathcal{L} \mathcal{H} \quad \mathrm{d}x \quad \mathbf{v} \quad \mathit{f}
$$

## Operators and relations

$$
a \times b \cdot c \pm d \mp e \div f \quad \leq \geq \neq \approx \equiv \sim \propto \ll \gg \quad \infty \ \partial \ \nabla \ \forall \ \exists \ \neg \ \emptyset
$$

## Sets, logic, arrows

$$
A \cup B \quad A \cap B \quad A \subseteq B \quad x \in S \quad x \notin S \quad p \land q \quad p \lor q \quad p \implies q \quad p \iff q \quad \to \ \leftarrow \ \leftrightarrow \ \Rightarrow \ \mapsto \ \uparrow \ \downarrow
$$

## Accents and decorations

$$
\hat{x} \quad \bar{x} \quad \vec{v} \quad \tilde{n} \quad \dot{x} \quad \ddot{x} \quad \overline{AB} \quad \underline{ab} \quad \overrightarrow{PQ} \quad \widehat{abc} \quad \boxed{E = mc^2}
$$

## Delimiters that grow

$$
\left( \frac{a}{b} \right) \quad \left[ \sum_{i} x_i \right] \quad \left\{ \frac{1}{2} \right\} \quad \left\lvert \frac{x}{y} \right\rvert \quad \left\langle u, v \right\rangle \quad \left\lceil x \right\rceil \quad \left\lfloor x \right\rfloor
$$

## Matrices

$$
\begin{pmatrix} a & b \\ c & d \end{pmatrix}
\quad
\begin{bmatrix} 1 & 0 \\ 0 & 1 \end{bmatrix}
\quad
\begin{vmatrix} x & y \\ z & w \end{vmatrix}
\quad
\begin{Bmatrix} p \\ q \end{Bmatrix}
\quad
\begin{matrix} 1 & 2 & 3 \\ 4 & 5 & 6 \end{matrix}
$$

## Cases and alignment

$$
|x| = \begin{cases} x & \text{if } x \geq 0 \\ -x & \text{if } x < 0 \end{cases}
$$

$$
\begin{aligned}
(a+b)^2 &= a^2 + 2ab + b^2 \\
(a-b)^2 &= a^2 - 2ab + b^2
\end{aligned}
$$

## Calculus

$$
\frac{\mathrm{d}}{\mathrm{d}x} f(x) \quad \frac{\partial^2 u}{\partial x \, \partial y} \quad \int_a^b f(x)\,dx \quad \oint_C \mathbf{F} \cdot d\mathbf{r} \quad \iint_D \, dA \quad \lim_{n \to \infty} \left(1 + \frac{1}{n}\right)^n = e
$$

## Text, spacing, colour, size

$$
\text{speed} = \frac{\text{distance}}{\text{time}} \quad a\,b \; c \quad d \qquad e \quad \color{red}{r} \color{blue}{b} \quad \small{small} \ \large{large}
$$

## Currency must stay currency

Single `$` is NOT a delimiter here: the mock costs $5 and $6 depending on the day, and a coffee is $3.50. If any of those turned into math, the delimiter set widened."#;

/// Mermaid. A ```` ```mermaid ```` fence in a text card's content is the only trigger
/// (`workspace/transcript.tsx:585`). Two diagram kinds, so a renderer that handles one and not
/// the other is visibly half-working.
const MERMAID: &str = r#"A flowchart with a decision:

```mermaid
flowchart TD
  A[Prompt arrives] --> B{Mock door?}
  B -- yes --> C[Serve a fixture]
  B -- no --> D[Call the gateway]
  C --> E[Append and emit]
  D --> E
```

And a sequence:

```mermaid
sequenceDiagram
  participant Desktop
  participant Server
  participant Gateway
  Desktop->>Server: sendPrompt
  Server->>Gateway: chat completion
  Gateway-->>Server: stream
  Server-->>Desktop: transcript frames
```"#;

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
    state: &super::GatewayState,
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
        // APPEND THEN EMIT, in that order, exactly as every other append path does
        // (`conversation.rs` user message, placeholder, final answer). Appending alone put the
        // fixture in history and nowhere else, so every card needed a reload to appear — which is
        // why the whole catalogue was verified "after Cmd+R" and never live. The append is the
        // durable half and the frame is the visible one; a fixture that only half-arrives teaches
        // the reader that the catalogue is unreliable rather than that a card is wrong.
        if let Err(error) = state
            .agui
            .auth
            .store
            .append_gateway_entry(coworker, account, &entry, at_ms)
            .await
        {
            tracing::warn!(%error, "mock fixture could not be appended");
            continue;
        }
        super::live::emit_transcript(state, coworker.as_str(), account, "appended", entry);
    }
}

/// Read a fixture file for the desktop's attachment readers, or say why not.
///
/// THIS IS A FILE-READ API AND IT IS SCOPED TO ONE DIRECTORY ON PURPOSE.
///
/// The desktop's PDF, spreadsheet, markdown and text viewers all reach bytes through seam-A
/// `readAttachmentChunk` / `readAttachmentText`; only image, video and audio bypass it by
/// streaming through `sand-media://`. Both verbs were refused unconditionally, so those readers
/// could never load anything on an OpenGrok server — including the fixtures this module writes.
///
/// The general feature is the artifacts slice and is deliberately parked (`ROADMAP.md` Later).
/// What is open here is the fixture directory ONLY, and only while a mock door is selected, so
/// that the catalogue can be verified end to end without opening a real read surface. Reading is
/// what the whole thing is for; the containment is the part worth reviewing:
///
/// 1. the door must be a mock door (`enabled()`) — on a real deployment this answers nothing;
/// 2. the path is CANONICALISED first, so `..` and symlinks are resolved before any check;
/// 3. the canonical path must sit under the canonical fixture directory.
///
/// Prefix-matching the raw string would be the classic hole: `/tmp/opengrok-mock-fixtures/../../
/// Volumes/goldcoders/OSS/opengrok-server/.env` passes a naive `starts_with` and hands back
/// `OG_TOKEN_SECRET` and `OG_CREDENTIAL_KEK`. Canonicalising first is what closes it, and it is
/// why the check is on the RESOLVED path and never on the argument.
pub fn read_fixture(path: &str) -> Result<Vec<u8>, String> {
    if !enabled() {
        return Err("attachment reads are open only under a mock door".to_string());
    }
    read_contained(path)
}

/// The containment itself, without the door check — so it can be tested directly. Setting
/// `OG_MODEL_DOOR` from a test is not available to us: `set_var` is unsafe in edition 2024 and
/// `unsafe_code` is forbidden workspace-wide, and a security check that cannot be exercised
/// because of a test-harness detail is one that silently stops being exercised.
fn read_contained(path: &str) -> Result<Vec<u8>, String> {
    let root = fixture_root()?;
    // ONE refusal string for both "not there" and "not yours to read", byte for byte. The first
    // version of this appended the OS error to the absent case — so `/etc/passwd` (exists, outside
    // the root) and `/tmp/opengrok-mock-fixtures/nope` (inside, absent) answered differently, and
    // the difference reports whether an arbitrary path on the host exists. A test that only
    // checked both messages CONTAINED the same phrase passed anyway; the live check caught it.
    let resolved = std::fs::canonicalize(path).map_err(|_| NO_SUCH.to_string())?;
    if !resolved.starts_with(&root) {
        return Err(NO_SUCH.to_string());
    }
    std::fs::read(&resolved).map_err(|_| NO_SUCH.to_string())
}

/// The single refusal. Absent, outside the root, or unreadable all answer with this and nothing
/// more — the shape of the failure must not describe the host's filesystem.
const NO_SUCH: &str = "no such attachment";

/// Every fixture, grouped as the help text presents them. The single source of truth for what
/// exists: `help_text` renders this, and `entries_for` dispatches on it.
const CATALOGUE: &[(&str, &str, &str)] = &[
    // (name, group, one-line description)
    (
        "text",
        "cards",
        "the markdown showcase — every construct plus the four chip schemes",
    ),
    (
        "katex",
        "cards",
        "display, bracket-display and inline math; $5 and $6 must stay currency",
    ),
    (
        "mermaid",
        "cards",
        "a flowchart and a sequenceDiagram in ```mermaid fences",
    ),
    ("text-images", "cards", "gallery of 3 — the common case"),
    ("text-images-1", "cards", "gallery of 1 — its own layout"),
    ("text-images-2", "cards", "gallery of 2 — its own layout"),
    (
        "text-images-4",
        "cards",
        "gallery of 4 — three tiles and a +N fold",
    ),
    (
        "text-images-6",
        "cards",
        "gallery of 6 — the fold with a larger remainder",
    ),
    (
        "widget-multi",
        "cards",
        "a widget with multiSelect: true — answer is one \\n-joined string",
    ),
    ("widget", "cards", "a choice card with four options"),
    (
        "cursor-agent",
        "cards",
        "cloud-agent card — blank unless the bcId resolves upstream",
    ),
    ("email-draft", "cards", "an editable email draft"),
    ("slack-draft", "cards", "an editable Slack draft"),
    (
        "auto-review-approval",
        "cards",
        "the approval card (interactive)",
    ),
    ("listener-connect", "cards", "connect-a-listener (github)"),
    ("secret-request", "cards", "a secret prompt"),
    ("connector", "cards", "the plugin-connect card"),
    ("connectors", "cards", "the multi-connect card"),
    (
        "permission",
        "cards",
        "local-tool-permission — projects, but never drawn from history",
    ),
    (
        "permission-request",
        "cards",
        "the retired read-only leaf (title only, NOT interactive)",
    ),
    (
        "image",
        "media",
        "attachment card, local .png — media must be a path, not a URL",
    ),
    (
        "video",
        "media",
        "attachment card, local .mp4 — media must be a path, not a URL",
    ),
    (
        "box",
        "media",
        "attachment card, sand://box — the Computer card",
    ),
    (
        "link",
        "media",
        "attachment card, non-media http URL (legacy-link)",
    ),
    ("markdown", "files", "user-attachment, .md"),
    (
        "zip",
        "files",
        "user-attachment, .zip — four files in two folders, listed from the central directory",
    ),
    (
        "targz",
        "files",
        "user-attachment, .tar.gz — the same tree, read through DecompressionStream",
    ),
    (
        "gz",
        "files",
        "user-attachment, .md.gz — a single-member gzip",
    ),
    (
        "sevenzip",
        "files",
        "user-attachment, .7z — UNSUPPORTED on purpose: the Download prompt, not a listing",
    ),
    (
        "pptx",
        "files",
        "user-attachment, .pptx — three slides: title, nested bullets, an image with notes",
    ),
    (
        "odt",
        "files",
        "user-attachment, .odt — headings, runs, both list kinds, a 2x3 table, a link",
    ),
    ("odp", "files", "user-attachment, .odp — two ODF slides"),
    (
        "rtf",
        "files",
        "user-attachment, .rtf — bold/italic runs, a tab, a unicode escape, two paragraphs",
    ),
    (
        "doc",
        "files",
        "user-attachment, .doc — UNSUPPORTED: refused on the extension, Download prompt",
    ),
    (
        "pages",
        "files",
        "user-attachment, .pages — UNSUPPORTED: refused on the extension, Download prompt",
    ),
    ("rust", "files", "user-attachment, .rs"),
    ("js", "files", "user-attachment, .js"),
    (
        "upload",
        "files",
        "user-attachment, .png (the media branch)",
    ),
    (
        "audio",
        "files",
        "user-attachment, .mp3 — audio has no card path",
    ),
    (
        "pdf",
        "files",
        "user-attachment, .pdf — three real pages, proves the page indicator",
    ),
    (
        "csv",
        "files",
        "user-attachment, .csv — the spreadsheet reader",
    ),
    ("json", "files", "user-attachment, .json — the JSON reader"),
    (
        "docx",
        "files",
        "user-attachment, .docx — every run style mammoth maps; colour and alignment cannot survive",
    ),
    (
        "html",
        "files",
        "user-attachment, .html — MUST render as text, never execute",
    ),
    ("txt", "files", "user-attachment, .txt with tabs"),
    ("yaml", "files", "user-attachment, .yaml"),
    ("svg", "files", "user-attachment, .svg"),
    ("notice", "kinds", "a muted system line"),
    ("event", "kinds", "a timeline event (name-changed)"),
    ("thinking", "kinds", "a collapsible reasoning block"),
    (
        "computer-handoff",
        "kinds",
        "the box card mid-handoff — attachment + boxRequestId",
    ),
    ("tool-running", "tools", "tool line — running"),
    ("tool-success", "tools", "tool line — success"),
    ("tool-error", "tools", "tool line — error"),
    ("tool-denied", "tools", "tool line — denied"),
    ("tool-rejected", "tools", "tool line — rejected"),
    ("tool-cancelled", "tools", "tool line — cancelled"),
    (
        "tool-background",
        "tools",
        "tool line — success + isBackground",
    ),
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
        (
            "files",
            "User attachments — real files under /tmp/opengrok-mock-fixtures",
        ),
        ("kinds", "Other entry kinds"),
        (
            "tools",
            "Tool lines — the seven statuses that draw distinctly",
        ),
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
         as a bare link, so those fixtures use user-attachment entries instead; and media on an \
         attachment card must be a LOCAL PATH — a `data:` URL never classifies as media, and a \
         remote one resolves to null before anything is fetched, so `image` and `video` point at \
         files on disk. The one exception is a text card's `images[]`, which is rendered as a \
         plain <img> and does take remote https.\n",
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
/// The fixture root, canonicalised, and REFUSED IF IT IS A SYMLINK.
///
/// `FIXTURE_DIR` is a fixed path in world-writable `/tmp`, so any local user can pre-create it as
/// a link to `/` before the server first touches it. Canonicalising the root then makes every
/// path "contained" and turns `readAttachmentChunk` into an arbitrary-file read over seam A —
/// including the `.env` this module's own comment names as the interesting target. Checking the
/// path the caller sent is not enough when the ROOT is attacker-controlled.
///
/// `symlink_metadata` does not follow the final component, so this sees the link itself. The
/// directory is created with `0o700` so a later swap needs the server user rather than any user.
fn fixture_root() -> Result<std::path::PathBuf, String> {
    let raw = std::path::Path::new(FIXTURE_DIR);
    if let Ok(meta) = std::fs::symlink_metadata(raw)
        && meta.file_type().is_symlink()
    {
        tracing::error!(
            dir = FIXTURE_DIR,
            "the fixture directory is a symlink; refusing to serve"
        );
        return Err(NO_SUCH.to_string());
    }
    std::fs::canonicalize(raw).map_err(|_| "the fixture directory does not exist yet".to_string())
}

/// Create the fixture directory owned by this user and readable by nobody else, then write.
///
/// `create_dir_all` alone inherits the umask and would happily adopt a directory somebody else
/// planted; `fs::write` follows a symlink at `path`, so a pre-planted link would have this
/// overwrite an arbitrary file as the server user. Mode `0o700` at creation and a symlink check
/// on the root close both. Best effort by design: a fixture that cannot be written is a fixture
/// that draws nothing, not a server that fails to start.
fn fixture_file(name: &str, body: &[u8]) -> String {
    let path = format!("{FIXTURE_DIR}/{name}");
    #[cfg(unix)]
    let made = {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(FIXTURE_DIR)
    };
    #[cfg(not(unix))]
    let made = std::fs::create_dir_all(FIXTURE_DIR);
    if let Err(error) = made {
        tracing::warn!(%error, "could not create the mock fixture directory");
    }
    if fixture_root().is_err() {
        tracing::error!(
            dir = FIXTURE_DIR,
            "refusing to write into a symlinked fixture directory"
        );
        return path;
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

/// Images for the count ladder: mixed portrait and landscape so the row planner's shape handling
/// is visible, and remote `https` because `images[]` is the one path that does NOT go through the
/// attachment source normaliser (`transcript.tsx:588-591` renders a plain `<img src>`, and the
/// CSP allows `https:`). Every other media fixture had to be a local path.
const IMAGE_POOL: &[(&str, &str)] = &[
    (
        "https://upload.wikimedia.org/wikipedia/commons/3/3a/Cat03.jpg",
        "A cat, landscape",
    ),
    (
        "https://upload.wikimedia.org/wikipedia/commons/4/4d/Cat_November_2010-1a.jpg",
        "A cat, portrait",
    ),
    (
        "https://upload.wikimedia.org/wikipedia/commons/b/b6/Felis_catus-cat_on_snow.jpg",
        "A cat in snow, landscape",
    ),
    (
        "https://upload.wikimedia.org/wikipedia/commons/1/15/Cat_August_2010-4.jpg",
        "A tabby, portrait",
    ),
    (
        "https://upload.wikimedia.org/wikipedia/commons/9/9b/Gato_enervado_pola_presenza_dun_can.jpg",
        "A startled cat, landscape",
    ),
    (
        "https://upload.wikimedia.org/wikipedia/commons/2/25/Siam_lilacpoint.jpg",
        "A Siamese, portrait",
    ),
];

/// A `text` card carrying `n` images. `images` sits INSIDE `message` beside `content`
/// (`send-message-text.ts:15-18`, `projectImages :81-90`): `{url: non-empty, alt?}`, extra keys
/// passed through.
fn images_card(name: &str, n: usize) -> Value {
    let images: Vec<Value> = IMAGE_POOL
        .iter()
        .take(n)
        .map(|(url, alt)| json!({ "url": url, "alt": alt }))
        .collect();
    let content = match n {
        1 => "One image — the single-image layout.".to_string(),
        _ => format!("{n} images in one bubble — the {n}-image layout."),
    };
    card(
        name,
        json!({ "type": "text", "content": content, "images": images }),
    )
}

/// The entries one fixture appends, or `None` when there is no such fixture.
pub fn entries_for(name: &str) -> Option<Vec<Value>> {
    let entries = match name {
        // ---- cards: `send-message` with a `message.type` ----
        "text" => vec![card("text", json!({ "type": "text", "content": SHOWCASE }))],
        "katex" => vec![card("katex", json!({ "type": "text", "content": KATEX }))],
        "mermaid" => vec![card(
            "mermaid",
            json!({ "type": "text", "content": MERMAID }),
        )],
        // `images` sits INSIDE `message` beside `content` (`send-message-text.ts:15-18`,
        // `projectImages :81-90`): `{url: non-empty, alt?}` and nothing else checked. Rendered as a
        // plain `<img src>` (`transcript.tsx:588-591`) — it does NOT go through the attachment
        // source normaliser, and the CSP allows `https:`. So this is the ONE place a remote URL
        // draws; every other media fixture had to be a local path.
        "text-images" => vec![images_card("text-images", 3)],
        // The image-count ladder. The row planner lays 1, 2, 3 and 4+ out differently — at four or
        // more it shows three tiles and a "+N" pill — so a single three-image fixture exercises
        // exactly one of four layouts and hides the fold entirely. Aspects are deliberately mixed
        // portrait and landscape, because a planner that only ever sees one shape is not being
        // asked the question it exists to answer.
        "text-images-1" => vec![images_card("text-images-1", 1)],
        "text-images-2" => vec![images_card("text-images-2", 2)],
        "text-images-4" => vec![images_card("text-images-4", 4)],
        "text-images-6" => vec![images_card("text-images-6", 6)],
        // `widget.multiSelect: true` (`protocol.ts:45`, projected `:267`). The answer comes back
        // through `respondToWidget` as ONE STRING — the chosen option values joined by "\n" in
        // option order, any custom line last (`views/widget.tsx multiAnswer`) — not an array. So
        // this fixture is answerable as well as drawable, and the values are what the client echoes.
        "widget-multi" => vec![card(
            "widget-multi",
            json!({
                "type": "widget",
                "widget": {
                    "prompt": "Which fixtures should run tonight?",
                    "helpText": "Pick any number.",
                    "multiSelect": true,
                    "allowCustom": true,
                    "options": [
                        { "label": "KaTeX",   "value": "katex" },
                        { "label": "Mermaid", "value": "mermaid" },
                        { "label": "Chips",   "value": "text" },
                        { "label": "Readers", "value": "pdf" },
                        { "label": "All of them", "value": "all", "style": "primary" }
                    ]
                }
            }),
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
        // THE INTERACTIVE PERMISSION BLOCK — AND IT CANNOT DRAW FROM A TRANSCRIPT ENTRY.
        //
        // The entry projects correctly (kind `local-tool-permission`, status, requestId) and then
        // `transcript.tsx:974` returns null for it. The block a person actually sees is drawn from
        // `localToolPermissionStore`, fed by the coordinator's `bridge.localToolPermission` ask
        // channel — a live request, not history. So this fixture exercises the projection and
        // stops there, by the client's design, and no change to the body will make it appear.
        //
        // Kept because the projection is still worth exercising and because the shape is the one
        // a real ask carries: emitted as a `send-message` CARD, never as its own entry kind, with
        // `message.ask` a record (`production/model.ts:414`) or the entry is rejected.
        // To see the block itself, drive a real approval — not this.
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
            json!({
                "type": "permission-request",
                // NESTED. projectPermissionRequestEntry (model.ts:240) reads
                // `message.permission.title`, not `message.title`. A top-level title projects to
                // null and the entry is dropped — measured against the shipped client, not read.
                "permission": { "title": "A retired permission-request leaf" }
            }),
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
        "markdown" => vec![user_attachment("markdown", "mock-notes.md", MD_BYTES)],
        "html" => vec![user_attachment("html", "mock-page.html", HTML_BYTES)],
        "txt" => vec![user_attachment("txt", "mock-notes.txt", TXT_BYTES)],
        "yaml" => vec![user_attachment("yaml", "mock-config.yaml", YAML_BYTES)],
        "svg" => vec![user_attachment("svg", "mock-shape.svg", SVG_BYTES)],
        "zip" => vec![user_attachment("zip", "mock-bundle.zip", ZIP_BYTES)],
        "targz" => vec![user_attachment("targz", "mock-bundle.tar.gz", TARGZ_BYTES)],
        "gz" => vec![user_attachment("gz", "mock-notes.md.gz", GZ_BYTES)],
        "sevenzip" => vec![user_attachment(
            "sevenzip",
            "mock-bundle.7z",
            SEVENZIP_BYTES,
        )],
        "pptx" => vec![user_attachment("pptx", "mock-deck.pptx", PPTX_BYTES)],
        "odt" => vec![user_attachment("odt", "mock-notes.odt", ODT_BYTES)],
        "odp" => vec![user_attachment("odp", "mock-deck.odp", ODP_BYTES)],
        "rtf" => vec![user_attachment("rtf", "mock-rich.rtf", RTF_BYTES)],
        "doc" => vec![user_attachment("doc", "mock-legacy.doc", DOC_BYTES)],
        "pages" => vec![user_attachment("pages", "mock-page.pages", PAGES_BYTES)],
        "rust" => vec![user_attachment("rust", "mock_sample.rs", RS_BYTES)],
        "js" => vec![user_attachment("js", "mock-sample.js", JS_BYTES)],
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
        // A HANDOFF IS AN ATTACHMENT, not a text message — and there is no `computer-handoff`
        // string in the shipped contract at all.
        //
        // Transcribed from the recovered 0.18 renderer: the box card is drawn ONLY from the
        // attachment branch, keyed on the message URL (`view-BKPMMMAd.js`, component `as(T)`:
        // `const b = L(e.message.url); if (b === "box") …`), and it reads the box fields off the
        // ENTRY rather than the message — `e.boxInstruction`, `e.boxRequestId`, `e.boxResolution`,
        // `e.boxSnapshot`. A separate index (`Obn`) files every `send-message` carrying
        // `boxRequestId` into the box-card store, type-agnostically, but the attachment branch is
        // its only consumer.
        //
        // So the first version of this fixture was wrong twice over: it used `type: "text"`, and
        // it was written against V2's own generic `computer-handoff` kind, which exists nowhere in
        // the recovered bundle. For a text message that branch is unreachable — the text projector
        // returns first — so it is a dead branch in a newer client, not a defect in the contract.
        // First measured 5 Sep 2026: it drew a plain bubble, which is exactly right for what it was.
        //
        // `boxResolution: null` with no live request renders "handed_back"; a string resolution
        // renders that instead. `no_other_fixture_carries_a_box_request_id` still holds the
        // ordering line, and `box` deliberately stays without one so the two are distinguishable.
        "computer-handoff" => vec![{
            let mut entry = card(
                "computer-handoff",
                json!({
                    "type": "attachment",
                    "url": "sand://box",
                    "alt": "Handed to the computer"
                }),
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
        "pdf" => vec![user_attachment("pdf", "mock-report.pdf", PDF_BYTES)],
        "csv" => vec![user_attachment("csv", "mock-table.csv", CSV_BYTES)],
        "json" => vec![user_attachment("json", "mock-data.json", JSON_BYTES)],
        // WHAT THIS FIXTURE CANNOT PROVE, so nobody chases it. Walked run-by-run against mammoth
        // 1.12.2 on 6 Sep 2026: headings h1-h6, bold, italic, strike, superscript, subscript,
        // underline, highlight (as <mark>), monospace, nested bullets and numbers, the table, the
        // hyperlink's real href, the hard break and the Quote blockquote all survive.
        //
        // RUN COLOUR AND PARAGRAPH ALIGNMENT DO NOT, AND CANNOT. mammoth strips presentational
        // formatting by design and offers no style-map target for either, so the red run and the
        // centred/right paragraphs are in the file and will never reach the reader — on our build
        // or on the official one, which uses the same library. The page break likewise has no HTML
        // counterpart and correctly reads as a paragraph boundary. They stay in the fixture because
        // a document that omitted them would quietly imply the reader had been asked; it has, and
        // the answer is no.
        //
        // Three of the survivors needed a client-side style map (`u => u`, `highlight => mark`,
        // `p[style-name='Quote'] => blockquote:fresh`) — mammoth's default map drops underline and
        // highlight and does not know Quote. That the fixture forced those three to be added is
        // the whole argument for a complete document over a representative one.
        "docx" => vec![user_attachment("docx", "mock-doc.docx", DOCX_BYTES)],

        "notice" => vec![json!({
            "kind": "notice",
            "id": entry_id("notice"),
            "text": "A muted system line from the mock fixture catalogue.",
            "timestampMs": now_ms(),
        })],
        // `to`, not `name`: projectTimelineEvent (timeline-event-registry.ts:57) requires a string
        // `to` for name-changed and returns null without it, which drops the whole entry silently.
        // Verified by running this catalogue against the shipped V2 client on 5 Sep 2026 — the
        // first spelling drew nothing and reported nothing.
        "event" => vec![json!({
            "kind": "event",
            "id": entry_id("event"),
            // `to` is the NEW NAME and the row renders as "Renamed to {to}", so it has to read
            // like a name. "Renamed by a mock fixture" produced "Renamed to Renamed by a mock
            // fixture", which is well-formed and still tells you the field was misunderstood.
            "event": { "type": "name-changed", "to": "Mock Fixture Bot" },
            "timestampMs": now_ms(),
        })],

        // ---- tool lines ----
        "tool-running" => vec![tool_line(
            "tool-running",
            "running",
            "Running a command",
            None,
        )],
        "tool-success" => vec![tool_line(
            "tool-success",
            "success",
            "Command finished",
            None,
        )],
        "tool-error" => vec![tool_line("tool-error", "error", "Command failed", None)],
        "tool-denied" => vec![tool_line("tool-denied", "denied", "Policy denied it", None)],
        "tool-rejected" => vec![tool_line(
            "tool-rejected",
            "rejected",
            "The person said no",
            None,
        )],
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
            (
                "image",
                [".jpg", ".jpeg", ".png", ".gif", ".webp", ".svg"].as_slice(),
            ),
            (
                "video",
                [".webm", ".mp4", ".mov", ".m4v", ".ogv"].as_slice(),
            ),
        ] {
            let entries = entries_for(fixture).expect("fixture");
            let url = entries[0]["message"]["url"].as_str().expect("a url");
            assert!(
                url.starts_with('/'),
                "{fixture} must be an absolute path: {url}"
            );
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
                if url == "sand://box"
                    || entry["message"]["type"] == "attachment"
                        && url.starts_with("https://example.com")
                {
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
        assert!(
            entry["content"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
        );
    }

    /// A handoff IS the attachment box branch, carrying the box fields at the entry's top level.
    ///
    /// This test asserted the opposite until 5 Sep 2026 — that `message.type` must NOT be
    /// `attachment` — which is what a plausible reading of a newer client's generic
    /// `computer-handoff` branch gives you. The recovered renderer draws the box card only from
    /// the attachment branch, keyed on the URL, so the fixture and this assertion were both wrong
    /// in the same direction and agreed with each other. A test written from the same guess as the
    /// code confirms the guess, not the contract.
    #[test]
    fn the_handoff_is_the_attachment_box_branch() {
        let entries = entries_for("computer-handoff").expect("fixture");
        let entry = &entries[0];
        assert_eq!(entry["kind"], "send-message");
        assert_eq!(entry["message"]["type"], "attachment");
        assert_eq!(entry["message"]["url"], "sand://box");
        assert!(
            entry["boxRequestId"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        assert!(entry["boxInstruction"].as_str().is_some());
        // `box` is the same card WITHOUT a request, so the two states stay distinguishable.
        let plain = entries_for("box").expect("fixture");
        assert!(plain[0].get("boxRequestId").is_none());
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
        for fixture in [
            "markdown", "zip", "targz", "gz", "sevenzip", "rust", "js", "upload", "pptx", "odt",
            "odp", "rtf", "doc", "pages",
        ] {
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
        let others = CATALOGUE
            .iter()
            .filter(|(_, group, _)| *group != "bulk")
            .count();
        assert!(
            all.len() >= others,
            "expected at least {others} entries, got {}",
            all.len()
        );
    }

    /// Both of these were WRONG and drew nothing, silently, until the catalogue was run against
    /// the shipped client on 5 Sep 2026. A rejected entry reports only a `rejectedCount` the page
    /// does not expose, so neither failure was visible from either side — which is exactly why the
    /// field path, not just the field's presence, is worth pinning in a test.
    #[test]
    fn the_two_field_paths_that_silently_dropped_their_entries() {
        // name-changed reads `to`; `name` projects to null and the entry is dropped.
        let event = entries_for("event").expect("fixture");
        assert_eq!(event[0]["event"]["type"], "name-changed");
        assert!(
            event[0]["event"]["to"]
                .as_str()
                .is_some_and(|to| !to.is_empty()),
            "name-changed needs a string `to`: {}",
            event[0]
        );
        assert!(
            event[0]["event"]["name"].is_null(),
            "`name` is the spelling that was dropped"
        );

        // permission-request reads `message.permission.title`, not `message.title`.
        let leaf = entries_for("permission-request").expect("fixture");
        assert!(
            leaf[0]["message"]["permission"]["title"]
                .as_str()
                .is_some_and(|title| !title.is_empty()),
            "the title is nested under `permission`: {}",
            leaf[0]
        );
        assert!(
            leaf[0]["message"]["title"].is_null(),
            "a top-level title is dropped"
        );
    }

    /// The containment, which is the only part of the read surface worth reviewing.
    ///
    /// A naive `starts_with` on the ARGUMENT lets `/tmp/opengrok-mock-fixtures/../../etc/passwd`
    /// through — and on this machine the interesting target is not `/etc/passwd` but the repo's
    /// own `.env`, which holds `OG_TOKEN_SECRET` and `OG_CREDENTIAL_KEK`. Canonicalising first
    /// and checking the RESOLVED path is what closes it.
    #[test]
    fn a_traversal_out_of_the_fixture_directory_is_refused() {
        // Make sure the directory and one real file exist, so a refusal below is the CHECK
        // refusing rather than the file merely being absent.
        let _ = entries_for("markdown");
        assert!(
            read_contained(&format!("{FIXTURE_DIR}/mock-notes.md")).is_ok(),
            "a real fixture must be readable or this test proves nothing"
        );

        for escape in [
            "/etc/passwd",
            "/etc/hosts",
            &format!("{FIXTURE_DIR}/../../etc/passwd"),
            &format!("{FIXTURE_DIR}/../"),
            &format!("{FIXTURE_DIR}/./../../etc/hosts"),
        ] {
            assert!(
                read_contained(escape).is_err(),
                "`{escape}` escaped the fixture directory"
            );
        }
    }

    /// A path outside the root and a path that does not exist answer the SAME way. Whether a file
    /// elsewhere on the host exists is itself something this surface must not report.
    #[test]
    fn outside_and_absent_are_indistinguishable() {
        let _ = entries_for("markdown");
        let outside = read_contained("/etc/hosts").expect_err("must refuse");
        let absent =
            read_contained(&format!("{FIXTURE_DIR}/not-a-real-file.md")).expect_err("must refuse");
        assert_eq!(
            outside, absent,
            "the refusals must be identical, not merely similar — a suffix that differs \
             reports whether a path outside the root exists"
        );
    }

    /// Every `$` in the KaTeX fixture is half of a `$$` pair EXCEPT the currency ones. If the
    /// count drifts, either display math lost a fence or somebody added single-`$` math, which the
    /// renderer would treat as text — the fixture would then be silently testing nothing.
    #[test]
    fn katex_uses_no_single_dollar_math_and_keeps_its_currency() {
        let entries = entries_for("katex").expect("fixture");
        let content = entries[0]["message"]["content"].as_str().expect("content");
        assert!(
            content.contains("$5 and $6"),
            "the currency line is the alarm; keep it"
        );
        assert!(content.contains("$3.50"));
        let doubles = content.matches("$$").count();
        let singles = content.matches('$').count();
        // Three currency amounts, each one lone `$`, plus the one in backticks where the prose
        // states the rule ("Single `$` is NOT a delimiter"). Anything beyond four is math somebody
        // wrote with single dollars, which the renderer will show as text — a silent no-op fixture.
        assert_eq!(
            singles,
            doubles * 2 + 4,
            "a `$` that is neither a `$$` fence nor currency"
        );
        // both bracket forms present, so the rewrite path is exercised too
        assert!(content.contains("\\[") && content.contains("\\(") && content.contains("\\]"));
        // the renderer's `$$`-on-own-line rule: every fence sits at a line boundary
        for line in content.lines().filter(|l| l.trim() == "$$") {
            assert_eq!(line.trim(), "$$");
        }
    }

    /// `images` lives INSIDE `message`, three of them, each a non-empty https URL — the one place a
    /// remote URL draws, because this path does not go through the attachment normaliser.
    /// The count ladder, because 1, 2, 3 and 4+ are four different layouts and the fold only
    /// appears at four. Every URL must be remote https — this is the one path that skips the
    /// attachment normaliser, and a local path here would silently draw nothing.
    #[test]
    fn the_image_ladder_covers_every_layout() {
        for (fixture, count) in [
            ("text-images-1", 1),
            ("text-images-2", 2),
            ("text-images", 3),
            ("text-images-4", 4),
            ("text-images-6", 6),
        ] {
            let entries = entries_for(fixture).expect("fixture");
            let images = entries[0]["message"]["images"]
                .as_array()
                .expect("images inside message, beside content");
            assert_eq!(images.len(), count, "{fixture}");
            for image in images {
                let url = image["url"].as_str().expect("url");
                assert!(url.starts_with("https://"), "{fixture}: {url}");
                assert!(
                    image["alt"].as_str().is_some_and(|a| !a.is_empty()),
                    "{fixture}"
                );
            }
        }
        // Distinct URLs, or a "gallery" of six is one picture six times and the planner is
        // never asked to lay out anything.
        let six = entries_for("text-images-6").expect("fixture");
        let urls: std::collections::BTreeSet<&str> = six[0]["message"]["images"]
            .as_array()
            .expect("images")
            .iter()
            .filter_map(|i| i["url"].as_str())
            .collect();
        assert_eq!(
            urls.len(),
            6,
            "the six-image gallery must show six different images"
        );
    }

    /// `multiSelect` is on `message.widget`, and every option carries the `value` the client echoes
    /// back in its `\n`-joined answer — so the fixture is answerable, not only drawable.
    #[test]
    fn widget_multi_is_answerable() {
        let entries = entries_for("widget-multi").expect("fixture");
        let widget = &entries[0]["message"]["widget"];
        assert_eq!(widget["multiSelect"], true);
        let options = widget["options"].as_array().expect("options");
        assert!(
            options.len() >= 4 && options.len() <= 6,
            "1–6 options or the projector drops it"
        );
        for option in options {
            assert!(option["label"].as_str().is_some_and(|l| !l.is_empty()));
            assert!(option["value"].as_str().is_some_and(|v| !v.is_empty()));
        }
    }

    /// The html fixture is deliberately adversarial: a `<script>` AND an `onerror` handler, so
    /// that "showed as text" is a real claim about two execution vectors, not one.
    #[test]
    fn the_html_fixture_carries_both_execution_vectors() {
        let html = std::str::from_utf8(HTML_BYTES).expect("utf8");
        assert!(html.contains("<script>"), "no <script>");
        assert!(html.contains("onerror="), "no onerror handler");
    }

    /// The documents are the real thing, not stubs: three PDF pages declared, and the docx carries
    /// the relationship part its hyperlink and numbering need. A stub that looks right fails inside
    /// the reader and teaches nobody which half was wrong.
    #[test]
    fn the_documents_are_structurally_complete() {
        assert!(
            PDF_BYTES.windows(8).any(|w| w == b"/Count 3"),
            "pdf must declare 3 pages"
        );
        for part in [
            &b"word/_rels/document.xml.rels"[..],
            b"word/numbering.xml",
            // WITHOUT styles.xml MAMMOTH THROWS. document.xml references styles by id —
            // pStyle Heading1-6 and Quote, rStyle Hyperlink, tblStyle TableGrid — and mammoth
            // resolves every one through the styles part; a referenced table style with no part
            // at all made it dereference undefined and fail the whole document, so the reader
            // showed "Couldn't read this document". Measured against mammoth 1.12.2 on
            // 6 Sep 2026. Its default map also matches by style NAME, not id, so the part must
            // carry `<w:name w:val="heading 1"/>` or a Heading1 paragraph stays a paragraph.
            b"word/styles.xml",
        ] {
            assert!(
                DOCX_BYTES.windows(part.len()).any(|w| w == part),
                "docx lacks {}",
                String::from_utf8_lossy(part)
            );
        }
    }

    /// The archives are real archives. The client lists a zip from its central directory and a
    /// tar from its headers, so a stub that only starts correctly lists as empty — which is what
    /// the 22-byte end-of-central-directory record this replaced actually did: it proved the
    /// plumbing and never the table. Each magic is checked where the format puts it, and the zip
    /// is checked for the entry NAMES, because that is the part a listing has to show.
    #[test]
    fn the_archives_are_real_archives() {
        assert!(ZIP_BYTES.starts_with(b"PK\x03\x04"), "zip: no local header");
        assert!(
            ZIP_BYTES.windows(4).any(|w| w == b"PK\x01\x02"),
            "zip: no central directory, so nothing to list"
        );
        for name in [
            &b"bundle/README.md"[..],
            b"bundle/src/main.rs",
            b"bundle/src/lib.rs",
            b"bundle/docs/notes.txt",
        ] {
            assert!(
                ZIP_BYTES.windows(name.len()).any(|w| w == name),
                "zip lacks {}",
                String::from_utf8_lossy(name)
            );
        }
        // Both gzips, by the two-byte magic and the deflate method byte.
        for (label, bytes) in [("tar.gz", TARGZ_BYTES), ("md.gz", GZ_BYTES)] {
            assert!(
                bytes.starts_with(&[0x1F, 0x8B, 0x08]),
                "{label}: not a deflate gzip stream"
            );
        }
        // The unsupported one is identified by its signature and nothing else — that is the
        // whole fixture, and a later "improvement" that swaps it for a listable archive removes
        // the only coverage the Download path has.
        assert!(
            SEVENZIP_BYTES.starts_with(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]),
            "7z: wrong signature, so the client would not reach the unsupported branch"
        );
    }

    /// The office documents are real packages, and the ones meant to be REFUSED are still
    /// refusable. Each is pinned where its format puts the signal: OOXML and ODF are zips, so
    /// they start `PK`; ODF additionally requires `mimetype` as the FIRST entry and STORED, which
    /// is how a reader sniffs the package without inflating anything — deflate it and the sniff
    /// reads nothing. The pptx is checked for the parts a slide-by-slide outline needs, including
    /// the notes slide and the embedded PNG, because those are the two a text-only reader drops
    /// silently rather than loudly.
    #[test]
    fn the_office_documents_are_real_packages() {
        for (label, bytes) in [
            ("pptx", PPTX_BYTES),
            ("odt", ODT_BYTES),
            ("odp", ODP_BYTES),
            ("pages", PAGES_BYTES),
        ] {
            assert!(bytes.starts_with(b"PK\x03\x04"), "{label}: not a zip");
        }
        // `mimetype` first and uncompressed: the local header's name field sits at offset 30, and
        // byte 8 is the compression method, which must be 0 (stored).
        for (label, bytes) in [("odt", ODT_BYTES), ("odp", ODP_BYTES)] {
            assert_eq!(
                &bytes[30..38],
                b"mimetype",
                "{label}: mimetype is not the first entry"
            );
            assert_eq!(
                bytes[8], 0,
                "{label}: mimetype is deflated, so a sniff reads nothing"
            );
        }
        for part in [
            &b"ppt/slides/slide1.xml"[..],
            b"ppt/slides/slide2.xml",
            b"ppt/slides/slide3.xml",
            // The two a text-only outline drops without saying so.
            b"ppt/notesSlides/notesSlide1.xml",
            b"ppt/media/image1.png",
            // Without a master and a layout the package is not openable at all.
            b"ppt/slideMasters/slideMaster1.xml",
            b"ppt/slideLayouts/slideLayout1.xml",
        ] {
            assert!(
                PPTX_BYTES.windows(part.len()).any(|w| w == part),
                "pptx lacks {}",
                String::from_utf8_lossy(part)
            );
        }
        // RTF is plain text, and the escape is the interesting byte: `\u233?` carries an ASCII
        // fallback after the code point, which a careless control-word stripper leaves behind as
        // a stray `?`.
        assert!(RTF_BYTES.starts_with(br"{\rtf1"), "rtf: no header");
        for token in [&br"\u233?"[..], br"\tab", br"\b ", br"\i ", br"\par"] {
            assert!(
                RTF_BYTES.windows(token.len()).any(|w| w == token),
                "rtf lacks {}",
                String::from_utf8_lossy(token)
            );
        }
        // The two refusals. The client diverts these on the EXTENSION and never reads them, so
        // these assertions do not guard the trigger — they guard that the files stay honestly
        // what they claim to be, and that neither is quietly turned into something a reader
        // could open, which would remove the Download path's only coverage.
        assert!(
            DOC_BYTES.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]),
            "doc: wrong OLE magic — the client refuses on the extension, but a fixture that \
             does not identify as a .doc cannot tell a wrong branch from a wrong file"
        );
        for folder in [&b"Index/"[..], b"Metadata/"] {
            assert!(
                PAGES_BYTES.windows(folder.len()).any(|w| w == folder),
                "pages lacks {}, which is what the format is recognised by",
                String::from_utf8_lossy(folder)
            );
        }
    }

    /// The markdown showcase is complete on headings: all six ATX levels, plus both setext forms.
    #[test]
    fn the_showcase_has_every_heading_level() {
        for level in 1..=6 {
            // Anchored on the trailing newline, not a leading one: level 1 is the file's first line.
            let atx = format!("{} Heading {level}\n", "#".repeat(level));
            assert!(SHOWCASE.contains(&atx), "missing ATX heading level {level}");
        }
        assert!(SHOWCASE.contains("\n==="), "missing setext h1");
        assert!(SHOWCASE.contains("\n---\n"), "missing setext h2 / rule");
        // and the four chip schemes, verbatim
        for chip in [
            "(sand-msg:t1u)",
            "(grokbot://app/v1/settings?id=theme)",
            "(grokbot://app/v1/plugin/add?id=404)",
            "(sand-workflow:deploy-prod)",
        ] {
            assert!(SHOWCASE.contains(chip), "missing chip {chip}");
        }
    }

    #[test]
    fn help_lists_every_fixture() {
        let help = help_text();
        for (name, _, _) in CATALOGUE {
            assert!(help.contains(name), "help omits `{name}`");
        }
    }
}
