//! Skills: a named, versioned bundle of instructions a person invokes for one turn by typing
//! `/name`. A `SKILL.md` body, plus whatever small files sit beside it, owned by an account.
//!
//! STORAGE AND CRUD ONLY. Nothing here reaches the model — handing a skill to a turn is its own
//! slice, and it reads these rows rather than re-deciding who may use what.
//!
//! Who may do what (`may`), in one place, so every route refuses alike:
//! - the OWNER reads, renames, enables, adds versions, deletes;
//! - somebody in the owner's ORG reads an enabled skill, and nothing else;
//! - everybody else is told there is no such skill, because telling them it exists but is not
//!   theirs is already an answer about somebody else's account.
//!
//! THE BODY IS CAPPED AT 8000 CHARACTERS (`MAX_SKILL_BODY_CHARS`). A skill lands in the same
//! single system message as the standing role, which `persona::MAX_ROLE_CHARS` holds to 1000; an
//! unbounded skill body would not be an instruction in that message, it would BE that message.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use opengrok_core::id::AccountId;
use opengrok_store::{NewSkill, NewSkillVersion, PgStore, SkillFileRow, SkillRow};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agui::AgUiState;
use crate::agui::routes::account_from_bearer;
use crate::recipes::org_of;

/// The most a skill body may be. See the module note: the body shares one system message with the
/// coworker's identity and its standing role, and 8000 characters is already eight times the role.
/// Characters rather than bytes, because that is the unit the person writing it counts in.
pub const MAX_SKILL_BODY_CHARS: usize = 8000;

/// The most every supporting file in one bundle may weigh, decoded.
///
/// 256 KiB, and the number is chosen from what the files are FOR rather than from what Postgres
/// could hold: a reference sheet, a checklist, a short script — things a coworker reads or runs
/// during a turn, which have to be copied onto its computer before that turn can start. A bundle
/// in megabytes would not be a skill, it would be a file share wearing a skill's name, and the
/// copy would be the slowest part of every turn that invoked it. `artifacts.rs` caps at 25 MiB
/// because a screen recording genuinely is that big; a skill is text.
pub const MAX_BUNDLE_BYTES: usize = 256 * 1024;

/// And a file count, because a byte cap alone lets ten thousand empty files through, and each one
/// is a row, a path to validate and a file to write onto a box.
pub const MAX_BUNDLE_FILES: usize = 32;

/// The most a description may be.
///
/// It is not decoration: a description is what a coworker reads to decide whether a skill is
/// relevant (`opengrok_plugins::Skill::description`), so it reaches the same system message the
/// body cap above is an argument about — and unlike the body it is ALSO in every row of every
/// listing. Uncapped, it was the way around the body cap: 8000 characters of "body" plus as many
/// again of "description". A line or two, which is what it is for.
pub const MAX_SKILL_DESCRIPTION_CHARS: usize = 300;

/// The most a whole request to a writing route may weigh: the bundle once base64 has grown it by
/// a third, plus room for the body, the paths and the field names around them.
///
/// LOWERS Axum's 2 MB default rather than raising it, which is the opposite of what
/// `artifacts.rs` needs and is deliberate. No valid request comes near it — that arithmetic is
/// what said a layer was unnecessary, and it was wrong about the layer's other job. The cap below
/// is counted while decoding, so without a ceiling on the request itself a caller could hand us
/// an arbitrarily long base64 string and make the server hold it before any of that counting
/// happens. The layer is the ceiling; `MAX_BUNDLE_BYTES` is the rule.
const MAX_UPLOAD_BYTES: usize = MAX_BUNDLE_BYTES / 3 * 4 + 64 * 1024;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn router(state: AgUiState) -> Router {
    Router::new()
        .route(
            "/skills",
            get(list)
                .post(create)
                .layer(axum::extract::DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .route("/skills/{id}", get(detail).put(update).delete(remove))
        .route(
            "/skills/{id}/versions",
            post(add_version).layer(axum::extract::DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .with_state(state)
}

/// What a caller is to a skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Relation {
    Owner,
    /// In the org the skill belongs to, and the skill is switched on.
    OrgMember,
    None,
}

/// What a caller wants to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Read,
    Edit,
    AddVersion,
    Delete,
}

/// The one answer to "may this person do that to this skill".
///
/// A truth table rather than a check per route, so a reader can see every answer at once and a
/// new route cannot invent a seventh rule. It is not a guard against a new `Action` going
/// unconsidered: the `(Owner, _)` arm answers yes to any action added later, which is the pattern
/// inherited from `recipes::may` and is why an owner-only action needs no edit here — and why one
/// that should NOT be owner-only needs a deliberate one.
///
/// Sharing a skill to one person is not here because nothing writes such a row yet — when it
/// does, it adds a `Relation` and a line to this table and every route below inherits it.
pub(crate) fn may(relation: Relation, action: Action) -> Result<(), &'static str> {
    use Action::*;
    use Relation::*;
    let ok = matches!((relation, action), (Owner, _) | (OrgMember, Read));
    if ok {
        Ok(())
    } else {
        Err(match relation {
            None => "no such skill",
            OrgMember => "only the skill's owner may change it",
            Owner => "refused",
        })
    }
}

fn relation_to(account: &AccountId, org: Option<&str>, skill: &SkillRow) -> Relation {
    if skill.owner_id == account.as_str() {
        return Relation::Owner;
    }
    // Both halves must be present: `org_id` is nullable on the row and `org` is `None` for a
    // person in no org, and `None == None` would make every orgless person a colleague of every
    // orgless stranger.
    match (skill.org_id.as_deref(), org) {
        (Some(theirs), Some(mine)) if theirs == mine && skill.enabled => Relation::OrgMember,
        _ => Relation::None,
    }
}

/// Loads the skill and checks the action. A deleted skill is a 404 to everyone but its owner, who
/// can still read it — a run that cited a skill has to stay able to say what it cited — and can
/// no longer write to it.
pub(crate) async fn permitted(
    state: &AgUiState,
    headers: &HeaderMap,
    id: &str,
    action: Action,
) -> Result<(AccountId, SkillRow), Response> {
    let Some(account) = account_from_bearer(state, headers) else {
        return Err((StatusCode::UNAUTHORIZED, "sign in first").into_response());
    };
    let org = org_of(state, &account).await;
    let skill = match state.auth.store.skill(id).await {
        Ok(Some(skill)) => skill,
        Ok(None) => return Err((StatusCode::NOT_FOUND, "no such skill").into_response()),
        Err(error) => {
            return Err((StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response());
        }
    };
    let relation = relation_to(&account, org.as_deref(), &skill);
    if skill.deleted_at_ms.is_some() && relation != Relation::Owner {
        return Err((StatusCode::NOT_FOUND, "no such skill").into_response());
    }
    if let Err(why) = may(relation, action) {
        let status = if relation == Relation::None {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::FORBIDDEN
        };
        return Err((status, why).into_response());
    }
    if skill.deleted_at_ms.is_some() && action != Action::Read {
        return Err((StatusCode::GONE, "that skill was deleted").into_response());
    }
    Ok((account, skill))
}

/// What a person writes a skill down as, and what the row says it came from.
///
/// `taught` is missing on purpose: it is the server's word for a body a turn wrote down, and a
/// client that could claim it would be putting a sentence in the page that is not true.
const CLIENT_KINDS: [&str; 2] = ["authored", "uploaded"];

/// The word to record, or the sentence to refuse with. `None` means "work it out from whether
/// files came with it", which is what a client that says nothing gets.
fn kind_or_refusal(asked: Option<&str>, has_files: bool) -> Result<&'static str, String> {
    let Some(asked) = asked.map(str::trim).filter(|word| !word.is_empty()) else {
        return Ok(if has_files { "uploaded" } else { "authored" });
    };
    match asked {
        "authored" => Ok("authored"),
        "uploaded" => Ok("uploaded"),
        "taught" => Err(
            "`taught` is written by the server when a turn writes a skill down, not asked for"
                .to_string(),
        ),
        other => Err(format!(
            "{other:?} is not a kind of skill; it is one of {}",
            CLIENT_KINDS.join(" or ")
        )),
    }
}

// ---- the wire ----

/// One supporting file as it travels: a relative path and its bytes, base64.
#[derive(Debug, Deserialize)]
struct FileIn {
    path: String,
    /// Base64, as `artifacts.rs` does it — a JSON body rather than multipart, so one shape
    /// carries the whole bundle and the route stays a route.
    bytes: String,
}

/// A refusal: the status it deserves and the sentence to say.
///
/// ONE TYPE FOR BOTH KINDS OF NO. When this was a bare `String` both call sites mapped every
/// refusal to 413, so a path traversal came back as "Payload Too Large" — a caller reading the
/// status alone was told to send less data when the real answer was "never send that path".
type Refusal = (StatusCode, String);

fn bad_request(why: String) -> Refusal {
    (StatusCode::BAD_REQUEST, why)
}

fn too_large(why: String) -> Refusal {
    (StatusCode::PAYLOAD_TOO_LARGE, why)
}

/// The files of one write, decoded and bounded, or the refusal.
fn decode_files(files: &[FileIn]) -> Result<Vec<SkillFileRow>, Refusal> {
    if files.len() > MAX_BUNDLE_FILES {
        return Err(too_large(format!(
            "a skill bundle carries at most {MAX_BUNDLE_FILES} files; this one has {}",
            files.len()
        )));
    }
    let mut out: Vec<SkillFileRow> = Vec::with_capacity(files.len());
    let mut total = 0usize;
    for file in files {
        let path = normalise_path(&file.path);
        check_path(&path)?;

        // THE SIZE IS CHECKED BEFORE THE DECODE, not after. Decoding first meant a caller could
        // make the server materialise an arbitrarily large `Vec<u8>` and only then be told it was
        // too big — the refusal was honest and the allocation had already happened. Base64 is four
        // characters per three bytes, so the encoded length bounds the decoded one from above
        // before a byte is written.
        let claimed = file.bytes.len() / 4 * 3;
        if total.saturating_add(claimed) > MAX_BUNDLE_BYTES {
            return Err(too_large(format!(
                "a skill bundle is at most {MAX_BUNDLE_BYTES} bytes; this one claims at least {}",
                total + claimed
            )));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&file.bytes)
            .map_err(|_| bad_request(format!("{path:?} did not decode as base64")))?;
        total += bytes.len();
        if total > MAX_BUNDLE_BYTES {
            return Err(too_large(format!(
                "a skill bundle is at most {MAX_BUNDLE_BYTES} bytes; this one reached {total}"
            )));
        }

        // Case-insensitively, because these become files on a disk: on macOS and Windows
        // `Notes.md` and `notes.md` are two rows here and ONE file there, and the second write
        // would silently replace the first.
        if out.iter().any(|kept| kept.path.eq_ignore_ascii_case(&path)) {
            return Err(bad_request(format!("{path:?} is in the bundle twice")));
        }
        out.push(SkillFileRow { path, bytes });
    }
    Ok(out)
}

/// Trim the path and collapse nothing else.
///
/// Normalising is only for DECIDING — the refusals below and the duplicate check read this form,
/// so `SKILL.md/` cannot slip past a check written against `SKILL.md`. What is stored is this
/// same trimmed string: a path we quietly rewrote would be a file the skill's own body then
/// failed to find.
fn normalise_path(path: &str) -> String {
    path.trim().trim_end_matches('/').to_string()
}

/// A path is refused, not sanitised. These files are written onto a coworker's computer before a
/// turn that invoked the skill, so `../../.ssh/authorized_keys` is a traversal wearing a
/// reference sheet's clothes.
///
/// THE LIST IS DELIBERATELY SHORT AND CLOSED. It is easier to allow a character later than to
/// find out which of them mattered once something downstream is shelling out or untarring, and
/// nothing consumes these files yet — this is the cheapest moment in the feature's life to say
/// no.
fn check_path(path: &str) -> Result<(), Refusal> {
    if path.is_empty() {
        return Err(bad_request("a bundled file needs a path".to_string()));
    }
    if path.len() > 200 {
        return Err(bad_request(format!(
            "{path:?} is too long a path for a bundled file"
        )));
    }
    // ASCII only, and no control characters. A newline or a NUL in a path is never a filename
    // somebody meant; and a non-ASCII one is refused not because U+FF0F FULLWIDTH SOLIDUS is a
    // separator today — nothing here treats it as one — but because it becomes one the moment
    // anything downstream normalises Unicode, and that change would be made somewhere else by
    // somebody who never read this function.
    if !path.is_ascii() || path.chars().any(|c| c.is_ascii_control()) {
        return Err(bad_request(format!(
            "{path:?} must be printable ASCII: a bundle's paths become filenames"
        )));
    }
    if path.starts_with('/') || path.starts_with('~') || path.contains('\\') {
        return Err(bad_request(format!(
            "{path:?} must be a relative path inside the bundle, with forward slashes"
        )));
    }
    for part in path.split('/') {
        if part.is_empty() {
            return Err(bad_request(format!("{path:?} has an empty path component")));
        }
        if part == ".." || part == "." {
            return Err(bad_request(format!("{path:?} climbs out of the bundle")));
        }
        // A leading dash is a flag, not a name: `cp -rf x` and `tar -P` are what a copier does
        // with it the first time one of these bundles is unpacked by a shell.
        if part.starts_with('-') {
            return Err(bad_request(format!(
                "{path:?} has a component starting with `-`, which a command line would read as a flag"
            )));
        }
    }
    if path.eq_ignore_ascii_case("SKILL.md") {
        return Err(bad_request(
            "the body IS the SKILL.md; a bundled file of that name would be a second one"
                .to_string(),
        ));
    }
    Ok(())
}

/// WHOSE SKILL A ROW IS, and it is on the summary rather than only the detail because a listing
/// can hold two rows with the same `name`: per-owner uniqueness stops `/review` being ambiguous
/// inside one account, and `filter=org` then lists a colleague's `review` beside your own.
///
/// THE PRECEDENCE RULE, decided here rather than by whichever branch loads skills into a turn:
/// A NAME TYPED AFTER A SLASH RESOLVES TO THE CALLER'S OWN SKILL FIRST, and only then to an org
/// one; two org skills with the same name resolve to neither and the person is asked which.
/// Stated now because the wire shape ships with this PR and a client that cannot tell two rows
/// apart has already drawn the wrong menu by the time the rule is written down.
fn summary(skill: &SkillRow) -> Value {
    json!({
        "id": skill.id,
        "ownerId": skill.owner_id,
        "name": skill.name,
        "description": skill.description,
        "source": skill.source,
        "updatedAtMs": skill.updated_at_ms,
        "versionCount": skill.version_count,
        // A skill with no body cannot be invoked, so it is a draft until one is written. Derived
        // rather than stored: a column would be a second answer to a question the versions
        // already answer, and the two would disagree the first time a write missed one.
        "draft": skill.version_count == 0,
        // Not in the first draft of the contract, and here because `PUT` accepts it: a client
        // that can set a switch it can never read back cannot draw that switch after a reload.
        "enabled": skill.enabled,
    })
}

/// The summary plus the newest body and the files that came with it.
async fn detail_of(store: &PgStore, skill: &SkillRow) -> Result<Value, Response> {
    let newest = store
        .latest_skill_version(&skill.id)
        .await
        .map_err(unavailable)?;
    let (body, version) = match &newest {
        Some(row) => (row.body.clone(), row.version),
        None => (String::new(), 0),
    };
    let files = match version {
        0 => Vec::new(),
        version => store
            .skill_files(&skill.id, version)
            .await
            .map_err(unavailable)?,
    };
    let mut out = summary(skill);
    if let Some(object) = out.as_object_mut() {
        object.insert("body".to_string(), json!(body));
        object.insert("version".to_string(), json!(version));
        object.insert(
            "files".to_string(),
            json!(
                files
                    .iter()
                    .map(|file| json!({
                        "path": file.path,
                        // Base64, the same shape the upload sent. There is no per-file read
                        // route, so if the detail did not carry the bytes the bundle would be
                        // write-only and a person could never see what they uploaded.
                        "bytes": base64::engine::general_purpose::STANDARD.encode(&file.bytes),
                    }))
                    .collect::<Vec<_>>()
            ),
        );
    }
    Ok(out)
}

fn unavailable(error: opengrok_store::StoreError) -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response()
}

// ---- routes ----

#[derive(Debug, Deserialize)]
struct ListQuery {
    /// `mine` | `shared` | `org` | (absent: everything visible).
    filter: Option<String>,
}

/// What `filter` may say. A typo used to fall through every arm and answer `200 []`, which is
/// indistinguishable from "you have none" — the reply a person blames their own account for.
const FILTERS: [&str; 4] = ["mine", "shared", "org", "all"];

/// `GET /skills?filter=mine|shared|org`
async fn list(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    let Some(account) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let org = org_of(&state, &account).await;
    let store = &state.auth.store;
    let filter = query
        .filter
        .as_deref()
        .map(str::trim)
        .filter(|filter| !filter.is_empty())
        .unwrap_or("all");
    if !FILTERS.contains(&filter) {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "{filter:?} is not a filter; it is one of {}, or leave it off for everything",
                FILTERS.join(", ")
            ),
        )
            .into_response();
    }
    let mut out = Vec::new();
    if matches!(filter, "mine" | "all") {
        match store.skills_owned_by(account.as_str()).await {
            Ok(rows) => out.extend(rows.iter().map(summary)),
            Err(error) => return unavailable(error),
        }
    }
    // `shared` IS `org` TODAY, and that is a fact about the rows rather than a shortcut: the only
    // thing that makes a skill visible to somebody who does not own it is the owner's org. When
    // sharing to one person lands, `shared` becomes those rows plus these, and `org` stays only
    // these — so the two words already mean what they will mean, and neither is a lie now.
    if matches!(filter, "shared" | "org" | "all")
        && let Some(org) = org.as_deref()
    {
        match store.skills_in_org(org, account.as_str()).await {
            Ok(rows) => out.extend(rows.iter().map(summary)),
            Err(error) => return unavailable(error),
        }
    }
    Json(out).into_response()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateRequest {
    /// Optional when the body's frontmatter names it: an uploaded `SKILL.md` already says what
    /// it is called, and making a person retype it is how the two come to disagree.
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    /// The `SKILL.md`, frontmatter and all. Absent creates a draft: the row exists, has a name,
    /// and has no body yet.
    #[serde(default)]
    body: Option<String>,
    /// `authored` | `uploaded`. Absent is worked out from whether files came with it.
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    files: Vec<FileIn>,
}

/// `POST /skills` — write one down, or take one that was uploaded.
async fn create(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Json(request): Json<CreateRequest>,
) -> Response {
    let Some(account) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };

    // ONE PARSER, in `opengrok-plugins`. A skill installed from a plugin folder and a skill
    // uploaded here are the same bytes, and a second implementation of "where does the body
    // start" would eventually let frontmatter leak into a system message.
    let parsed = request
        .body
        .as_deref()
        .map(opengrok_plugins::split_frontmatter);
    if let Some(parsed) = &parsed
        && !parsed.closed
    {
        return unclosed_fence();
    }

    // The person's own words beat the file's: they are the one naming the thing they uploaded.
    let name = request
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| parsed.as_ref().and_then(|parsed| parsed.name.clone()));
    let Some(name) = name else {
        return (
            StatusCode::BAD_REQUEST,
            "a skill needs a name — it is what a person types after the slash",
        )
            .into_response();
    };
    if let Err(why) = check_name(&name) {
        return (StatusCode::BAD_REQUEST, why).into_response();
    }
    let description = request
        .description
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .or_else(|| {
            parsed
                .as_ref()
                .and_then(|parsed| parsed.description.clone())
        })
        .unwrap_or_default();
    if let Err(why) = check_description(&description) {
        return why.into_response();
    }

    let body = parsed.as_ref().map(|parsed| parsed.body.trim());
    if let Some(body) = body
        && let Err(why) = check_body(body)
    {
        return why.into_response();
    }
    let files = match decode_files(&request.files) {
        Ok(files) => files,
        Err(why) => return why.into_response(),
    };
    if !files.is_empty() && body.is_none_or(str::is_empty) {
        return (
            StatusCode::BAD_REQUEST,
            "a bundle of files with no SKILL.md is nothing a coworker can be given",
        )
            .into_response();
    }
    let kind = match kind_or_refusal(request.source.as_deref(), !files.is_empty()) {
        Ok(kind) => kind,
        Err(why) => return (StatusCode::BAD_REQUEST, why).into_response(),
    };

    let store = &state.auth.store;
    // Asked first so the ordinary collision gets the sentence that names the skill rather than
    // the index's. The unique index is still what decides — see below.
    match store.skill_named(account.as_str(), &name).await {
        Ok(Some(_)) => return (StatusCode::CONFLICT, taken(&name)).into_response(),
        Ok(None) => {}
        Err(error) => return unavailable(error),
    }

    // The org comes off the account, never off the request: a skill may not be filed under
    // somebody else's org by asking nicely.
    let org = org_of(&state, &account).await;
    let id = format!("skl_{}", uuid::Uuid::now_v7());
    let at_ms = now_ms();
    let first = body
        .filter(|body| !body.is_empty())
        .map(|body| NewSkillVersion {
            kind,
            body,
            note: "",
            files: &files,
            created_by: account.as_str(),
        });
    // THE ROW AND ITS FIRST BODY GO IN TOGETHER. As two calls, a failure between them left a
    // named, bodiless row holding the unique name — and the person's retry was refused with
    // "you already have a skill called that" for a skill they had never successfully made, with
    // no route that could delete it because they had never been shown its id.
    if let Err(error) = store
        .create_skill(
            NewSkill {
                id: &id,
                owner_id: account.as_str(),
                org_id: org.as_deref(),
                name: &name,
                description: &description,
                source: kind,
            },
            first,
            at_ms,
        )
        .await
    {
        return created_or_refused(error, &name);
    }

    match store.skill(&id).await {
        Ok(Some(row)) => match detail_of(store, &row).await {
            Ok(detail) => Json(detail).into_response(),
            Err(response) => response,
        },
        Ok(None) => (StatusCode::SERVICE_UNAVAILABLE, "the skill did not stick").into_response(),
        Err(error) => unavailable(error),
    }
}

/// A write that lost the name to somebody else says so in the same words the non-racing path
/// uses.
///
/// The unique index is the real arbiter — the check above it is a read, and two requests can both
/// pass it — and a lost race arrives here as `StoreError::Conflict`. Left alone it surfaced as a
/// 503 saying "another writer got there first", so the SAME condition answered 409 or 503
/// depending on timing, and a client could only handle one of them.
fn created_or_refused(error: opengrok_store::StoreError, name: &str) -> Response {
    match error {
        opengrok_store::StoreError::Conflict => (StatusCode::CONFLICT, taken(name)).into_response(),
        error => unavailable(error),
    }
}

fn unclosed_fence() -> Response {
    (
        StatusCode::BAD_REQUEST,
        "this SKILL.md opens with `---` and never closes it, so where the frontmatter ends and \
         the instructions begin cannot be told apart. Add the closing `---`.",
    )
        .into_response()
}

fn taken(name: &str) -> String {
    format!("you already have a skill called {name:?}; `/{name}` has to mean one thing")
}

/// A name is what a person types after a slash, so it is held to the plugin spec's own pattern:
/// lowercase alphanumerics, dots and dashes. Checked rather than trusted because it reaches a
/// filesystem path when the bundle is copied onto a coworker's computer.
fn check_name(name: &str) -> Result<(), String> {
    if opengrok_plugins::is_valid_name(name) {
        Ok(())
    } else {
        Err(format!(
            "{name:?} cannot be a skill name: it is typed after a slash, so it is lowercase \
             letters, digits, dots and dashes, starting and ending with a letter or digit, up to \
             64 characters"
        ))
    }
}

fn check_body(body: &str) -> Result<(), Refusal> {
    let length = body.chars().count();
    if length > MAX_SKILL_BODY_CHARS {
        return Err(too_large(format!(
            "the skill body is {length} characters, over the {MAX_SKILL_BODY_CHARS} allowed — a \
             skill shares one system message with the coworker's own role, and a longer one would \
             replace it rather than add to it"
        )));
    }
    Ok(())
}

fn check_description(description: &str) -> Result<(), Refusal> {
    let length = description.chars().count();
    if length > MAX_SKILL_DESCRIPTION_CHARS {
        return Err(too_large(format!(
            "the skill description is {length} characters, over the \
             {MAX_SKILL_DESCRIPTION_CHARS} allowed — it rides in every row of every listing, and \
             it is what a coworker reads to decide whether the skill is relevant, so it is a line \
             rather than a second body"
        )));
    }
    Ok(())
}

/// `GET /skills/{id}`
async fn detail(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (_, skill) = match permitted(&state, &headers, &id, Action::Read).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    match detail_of(&state.auth.store, &skill).await {
        Ok(detail) => Json(detail).into_response(),
        Err(response) => response,
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateRequest {
    /// Absent leaves it alone. Blank is refused rather than treated as "clear it": a nameless
    /// skill could never be invoked again.
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

/// `PUT /skills/{id}` — rename, re-describe, switch on or off.
async fn update(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(request): Json<UpdateRequest>,
) -> Response {
    let (account, skill) = match permitted(&state, &headers, &id, Action::Edit).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    let name = match request.name.as_deref().map(str::trim) {
        Some("") => {
            return (
                StatusCode::BAD_REQUEST,
                "a skill needs a name — it is what a person types after the slash",
            )
                .into_response();
        }
        Some(name) => name.to_string(),
        None => skill.name.clone(),
    };
    if name != skill.name {
        if let Err(why) = check_name(&name) {
            return (StatusCode::BAD_REQUEST, why).into_response();
        }
        match state.auth.store.skill_named(account.as_str(), &name).await {
            Ok(Some(other)) if other.id != skill.id => {
                return (StatusCode::CONFLICT, taken(&name)).into_response();
            }
            Ok(_) => {}
            Err(error) => return unavailable(error),
        }
    }
    let description = request
        .description
        .as_deref()
        .map(str::trim)
        .map(str::to_string)
        .unwrap_or_else(|| skill.description.clone());
    if let Err(why) = check_description(&description) {
        return why.into_response();
    }
    let enabled = request.enabled.unwrap_or(skill.enabled);

    if let Err(error) = state
        .auth
        .store
        .update_skill(&skill.id, &name, &description, enabled, now_ms())
        .await
    {
        // A rename that lost the name to a concurrent write gets the same 409 as one that lost
        // it to a row already there.
        return created_or_refused(error, &name);
    }
    match state.auth.store.skill(&skill.id).await {
        Ok(Some(row)) => match detail_of(&state.auth.store, &row).await {
            Ok(detail) => Json(detail).into_response(),
            Err(response) => response,
        },
        Ok(None) => (StatusCode::NOT_FOUND, "no such skill").into_response(),
        Err(error) => unavailable(error),
    }
}

/// `DELETE /skills/{id}` — soft, so a run that cited this skill can still say what it cited.
async fn remove(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (_, skill) = match permitted(&state, &headers, &id, Action::Delete).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    if let Err(error) = state
        .auth
        .store
        .soft_delete_skill(&skill.id, now_ms())
        .await
    {
        return unavailable(error);
    }
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionRequest {
    body: String,
    #[serde(default)]
    note: Option<String>,
    /// `authored` | `uploaded`. Absent is worked out from whether files came with it.
    #[serde(default)]
    kind: Option<String>,
    /// The bundle AS OF THIS VERSION. Files are stored per version, so what is not sent here is
    /// not beside this body — a version that dropped a reference sheet must not keep finding the
    /// old one.
    #[serde(default)]
    files: Vec<FileIn>,
}

/// `POST /skills/{id}/versions` — a new body.
async fn add_version(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(request): Json<VersionRequest>,
) -> Response {
    let (account, skill) = match permitted(&state, &headers, &id, Action::AddVersion).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    // The frontmatter is STRIPPED here and its `name` and `description` are DISCARDED, unlike on
    // create where both are honoured. Deliberate: renaming is `PUT`'s job, and this route answers
    // with a `SkillVersion` — a body that quietly renamed the skill would change what `/name`
    // means and reply without a word about it, so the person would learn it from a menu later.
    let parsed = opengrok_plugins::split_frontmatter(&request.body);
    if !parsed.closed {
        return unclosed_fence();
    }
    let body = parsed.body.trim();
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "a version needs a body").into_response();
    }
    if let Err(why) = check_body(body) {
        return why.into_response();
    }
    let files = match decode_files(&request.files) {
        Ok(files) => files,
        Err(why) => return why.into_response(),
    };
    let kind = match kind_or_refusal(request.kind.as_deref(), !files.is_empty()) {
        Ok(kind) => kind,
        Err(why) => return (StatusCode::BAD_REQUEST, why).into_response(),
    };
    let note = request.note.as_deref().map(str::trim).unwrap_or_default();
    let at_ms = now_ms();
    let version = match state
        .auth
        .store
        .add_skill_version(
            &skill.id,
            NewSkillVersion {
                kind,
                body,
                note,
                files: &files,
                created_by: account.as_str(),
            },
            at_ms,
        )
        .await
    {
        Ok(version) => version,
        Err(error) => return unavailable(error),
    };
    Json(json!({
        "version": version,
        "kind": kind,
        "createdAtMs": at_ms,
        "note": note,
    }))
    .into_response()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// The table, stated once here so a change to `may` has to be a deliberate edit of both.
    #[test]
    fn only_the_owner_writes_and_only_the_org_reads() {
        assert!(may(Relation::Owner, Action::Delete).is_ok());
        assert!(may(Relation::Owner, Action::AddVersion).is_ok());
        assert!(may(Relation::OrgMember, Action::Read).is_ok());
        assert!(may(Relation::OrgMember, Action::Edit).is_err());
        assert!(may(Relation::OrgMember, Action::Delete).is_err());
        assert!(may(Relation::None, Action::Read).is_err());
        assert_eq!(may(Relation::None, Action::Read), Err("no such skill"));
    }

    /// A path is refused rather than sanitised: these files are written onto a computer, and
    /// every one of these got through at some point before somebody read the list again.
    #[test]
    fn a_bundled_path_cannot_climb_out_or_become_a_flag() {
        assert!(check_path("reference/cheatsheet.md").is_ok());
        assert!(check_path("a-b/c.d_e.md").is_ok());

        for refused in [
            "../../.ssh/authorized_keys",
            "/etc/passwd",
            "~/.ssh/authorized_keys", // a leading ~ is a home directory to every shell
            "windows\\path",
            "with\nnewline",
            "bell\u{7}",
            "wide\u{ff0f}slash", // not a separator here, and one the moment anything normalises
            "SKILL.md",
            "skill.md",
            "SKILL.md/", // the trailing slash used to walk straight past the check above
            "a//b",      // two rows, one file
            "./here",
            "-rf",
            "dir/-P/x",
            "",
        ] {
            assert!(
                check_path(&normalise_path(refused)).is_err(),
                "{refused:?} should be refused"
            );
        }
    }

    /// Every refusal from a bundle carries the status it deserves: a traversal is not a request
    /// to send less data.
    #[test]
    fn a_bad_path_is_a_bad_request_and_a_big_bundle_is_too_large() {
        let file = |path: &str, bytes: &str| FileIn {
            path: path.to_string(),
            bytes: bytes.to_string(),
        };
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"hello");

        let (status, why) = decode_files(&[file("../escape", &encoded)]).expect_err("refused");
        assert_eq!(status, StatusCode::BAD_REQUEST, "{why}");

        let (status, why) = decode_files(&[file("a.md", "not base64 !!!")]).expect_err("refused");
        assert_eq!(status, StatusCode::BAD_REQUEST, "{why}");

        let twice = [file("Notes.md", &encoded), file("notes.md", &encoded)];
        let (status, why) = decode_files(&twice).expect_err("refused");
        assert_eq!(status, StatusCode::BAD_REQUEST, "{why}");
        assert!(why.contains("twice"), "a disk folds the case: {why}");

        let many: Vec<FileIn> = (0..=MAX_BUNDLE_FILES)
            .map(|n| file(&format!("f{n}.md"), &encoded))
            .collect();
        let (status, why) = decode_files(&many).expect_err("refused");
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{why}");

        // The size is read off the ENCODED length, so this is refused without ever being decoded.
        let fat = "A".repeat(MAX_BUNDLE_BYTES / 3 * 4 + 8);
        let (status, why) = decode_files(&[file("big.bin", &fat)]).expect_err("refused");
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{why}");
        assert!(why.contains(&MAX_BUNDLE_BYTES.to_string()), "{why}");
    }

    /// The refusal has to name the limit AND what arrived, or the person cannot tell how much
    /// to cut.
    #[test]
    fn an_over_long_body_is_refused_with_both_numbers() {
        let body = "x".repeat(MAX_SKILL_BODY_CHARS + 7);
        let (status, why) = check_body(&body).expect_err("over the cap");
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(why.contains("8000"), "{why}");
        assert!(
            why.contains(&(MAX_SKILL_BODY_CHARS + 7).to_string()),
            "{why}"
        );
        assert!(check_body(&"x".repeat(MAX_SKILL_BODY_CHARS)).is_ok());
    }

    /// Characters, not bytes: a body of 8000 emoji is 8000 characters the person counted.
    #[test]
    fn the_body_cap_counts_characters() {
        assert!(check_body(&"é".repeat(MAX_SKILL_BODY_CHARS)).is_ok());
    }

    /// The description was the way around the body cap: it reaches the same system message.
    #[test]
    fn a_description_is_a_line_not_a_second_body() {
        assert!(check_description(&"x".repeat(MAX_SKILL_DESCRIPTION_CHARS)).is_ok());
        let (_, why) = check_description(&"x".repeat(MAX_SKILL_DESCRIPTION_CHARS + 1))
            .expect_err("over the cap");
        assert!(why.contains("300") && why.contains("301"), "{why}");
    }

    #[test]
    fn a_client_cannot_claim_a_body_was_taught() {
        assert_eq!(kind_or_refusal(None, false), Ok("authored"));
        assert_eq!(kind_or_refusal(None, true), Ok("uploaded"));
        assert_eq!(kind_or_refusal(Some("uploaded"), false), Ok("uploaded"));
        assert!(kind_or_refusal(Some("taught"), false).is_err());
        assert!(kind_or_refusal(Some("magic"), false).is_err());
    }
}
