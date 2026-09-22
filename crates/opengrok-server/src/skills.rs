//! Skills: a named, versioned bundle of instructions a person invokes for one turn by typing
//! `/name`. A `SKILL.md` body, plus whatever small files sit beside it, owned by an account.
//!
//! CRUD, plus the one read a turn makes. `for_turn` is how a chosen skill reaches the model, and
//! it goes through the same `may()` table as every route here rather than re-deciding who may use
//! what — the checks drift, the table cannot.
//!
//! `from_tape` is the one route that asks a model for something rather than serving what a person
//! wrote: a recording becomes a lesson (`crate::tape_lesson`) and that lesson becomes the first
//! version of a skill. The prose comes back UNTRUSTED and goes through exactly the checks an
//! uploaded `SKILL.md` goes through, and the skill it makes is born switched off so a person reads
//! it before a turn does.
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
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_recipes::{Screen, TapeEvent};
use opengrok_store::{NewSkill, NewSkillVersion, PgStore, SkillFileRow, SkillRow};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agui::AgUiState;
use crate::agui::routes::{account_from_bearer, owned_coworker};
use crate::recipes::{org_of, tape_into_steps};

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
        // A STATIC SEGMENT BESIDE `{id}`, which axum resolves in favour of the static one, the
        // way `/admin/computers/docker` already sits beside `/admin/computers/{kind}`.
        //
        // AND NO LOWERED BODY LIMIT, unlike the two routes above it: what this carries is a raw
        // tape, not a bundle of reference files, and it is the same shape `POST /recipes` already
        // takes from the same recorder under axum's own default. `MAX_UPLOAD_BYTES` here would
        // refuse a minute of somebody's screen for being too big to be a skill's cheat sheet.
        .route("/skills/from-tape", post(from_tape))
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
    /// Run it in a turn: put its body inside a coworker's system message. A SEPARATE ACTION FROM
    /// `Read` even though today's answer is the same, because the two are different questions.
    /// Reading is seeing a name, a description and a body on a page. Invoking is up to 8000
    /// characters of somebody's prose landing beside a coworker's box, its shell, its machine
    /// shell and its saved-credential flow, chosen off a listing (`summary`) that does not carry
    /// the body — so the chooser has very likely not read what they picked.
    Invoke,
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
///
/// `(OrgMember, Invoke)` IS ALLOWED, AND IT IS THE ONE LINE HERE WORTH ARGUING ABOUT. It lets a
/// colleague's prose run inside this coworker's system message, and the person choosing it has
/// been shown a name and a description and not a body. It is allowed anyway because the listing
/// already offers it: a turn narrower than the menu would refuse a skill the composer had just
/// held out, with no way for the person to tell why. The mitigation is not a narrower table, it
/// is that the framing says whose words these are (`persona::SkillAuthor::Colleague`), so the
/// model and the reply can both reflect it. Written down here rather than decided at the call
/// site, so tightening it later is one edit to one line.
pub(crate) fn may(relation: Relation, action: Action) -> Result<(), &'static str> {
    use Action::*;
    use Relation::*;
    let ok = matches!((relation, action), (Owner, _) | (OrgMember, Read | Invoke));
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

/// Why a skill the person chose cannot be given to this turn.
///
/// An enum rather than a sentence, because the two readers want different things: an operator's
/// log wants which case it was, and the person's reply must not carry it — see
/// `persona::SKILL_UNAVAILABLE_LINE` for why the cases collapse into one sentence there.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NotForThisTurn {
    #[error("no skill with that id")]
    NoSuchSkill,
    /// The `skill` on the turn is not a shape any id we mint could have. Its own case because it
    /// is a CLIENT bug, not a missing row: silence here would let a client shape change stop
    /// applying skills with nothing anywhere saying so.
    #[error("the id on the turn is not shaped like a skill id")]
    NotAnId,
    #[error("the skill was deleted")]
    Deleted,
    #[error("the skill is switched off")]
    Disabled,
    #[error("the skill is not this account's to use")]
    NotTheirs,
    #[error("the skill has no body yet")]
    Draft,
    /// A stored body over the cap. Refused rather than quoted: `check_body` enforces the cap on
    /// write, so this row got in around it, and a 200,000-character body does not cost the skill —
    /// it costs the whole turn, by filling the context the person's own message needed.
    #[error("the stored body is {length} characters, over the {cap} cap")]
    TooLong { length: usize, cap: usize },
    /// No marker could be minted that the body does not already contain (`persona::skill_marker`).
    /// Unreachable by chance; reachable only by a body built against this code.
    #[error("no unforgeable marker could be minted for that body")]
    Unquotable,
    #[error("the skill could not be read: {0}")]
    Unreadable(String),
}

impl NotForThisTurn {
    /// The sentence the coworker is given. One for every case but the draft, which the composer
    /// already showed the chooser (`summary` carries `draft` and `versionCount`) and so can be
    /// named without telling them anything they were not looking at. See
    /// `persona::SKILL_UNAVAILABLE_LINE` for why the rest share one sentence that names no cause.
    pub(crate) fn line(&self) -> &'static str {
        match self {
            Self::Draft => crate::persona::SKILL_DRAFT_LINE,
            _ => crate::persona::SKILL_UNAVAILABLE_LINE,
        }
    }
}

/// A skill as a turn needs it: what to call it, and what it says.
///
/// A struct rather than a `(String, String)`, for `NewSkill`'s reason: two strings in a row, and
/// the call site that swaps them compiles and puts the whole body where the name goes.
pub(crate) struct SkillForTurn {
    pub name: String,
    pub body: String,
    /// Whether the person taking the turn wrote this, or a colleague did. The framing says which,
    /// because the listing they chose from does not carry a body and "chose it" is not "read it".
    pub author: crate::persona::SkillAuthor,
}

/// The skill the person chose for this turn, resolved against the account that signed the request.
///
/// THE ACCOUNT COMES OFF THE BEARER, NEVER OFF THE TURN (CLAUDE.md #7). `forwardedProps` carries an
/// id and nothing about whose it is, so the relation is computed here from the authenticated
/// account; an id belonging to somebody else is refused however confidently it was sent.
///
/// The same `may()` table the routes read, so a skill a person can see in their own list is one
/// they can invoke, and a skill they cannot see stays one they cannot smuggle into a prompt by id.
///
/// A SWITCHED-OFF SKILL IS REFUSED EVEN TO ITS OWNER. `permitted` still lets the owner READ one —
/// that is how they switch it back on — but switching a skill off is exactly the instruction not
/// to use it, and a turn that used it anyway would be reading the switch as decoration.
pub(crate) async fn for_turn(
    state: &AgUiState,
    account: &AccountId,
    id: &str,
) -> Result<SkillForTurn, NotForThisTurn> {
    let skill = match state.auth.store.skill(id).await {
        Ok(Some(skill)) => skill,
        Ok(None) => return Err(NotForThisTurn::NoSuchSkill),
        Err(error) => return Err(NotForThisTurn::Unreadable(error.to_string())),
    };
    let org = org_of(state, account).await;
    // Visibility first, so what is recorded about a skill this account cannot see is that it
    // cannot see it — not the state of somebody else's row.
    if may(relation_to(account, org.as_deref(), &skill), Action::Invoke).is_err() {
        // `relation_to` folds `enabled` into `OrgMember`, so a COLLEAGUE'S switched-off skill
        // arrives here looking exactly like a stranger's. Same refusal to the person either way;
        // different sentence in the log, which is the only place the difference is any use — an
        // operator asked why a shared skill stopped working goes and reads org membership when
        // the answer was the switch.
        let in_the_owners_org = matches!(
            (skill.org_id.as_deref(), org.as_deref()),
            (Some(theirs), Some(mine)) if theirs == mine
        );
        return Err(if in_the_owners_org && !skill.enabled {
            NotForThisTurn::Disabled
        } else {
            NotForThisTurn::NotTheirs
        });
    }
    if skill.deleted_at_ms.is_some() {
        return Err(NotForThisTurn::Deleted);
    }
    if !skill.enabled {
        return Err(NotForThisTurn::Disabled);
    }
    let body = match state.auth.store.latest_skill_version(&skill.id).await {
        Ok(Some(version)) => version.body,
        Ok(None) => return Err(NotForThisTurn::Draft),
        Err(error) => return Err(NotForThisTurn::Unreadable(error.to_string())),
    };
    // ONE PLACE DECIDES WHAT "EMPTY" MEANS. A version row whose body is blank is the same thing to
    // a turn as no version at all — no instructions — and it used to return `Ok` with an empty
    // body, which emitted nothing: no quote, no refusal, no log, while a MISSING version was
    // refused loudly. Two shapes of nothing must not get two different answers.
    if body.trim().is_empty() {
        return Err(NotForThisTurn::Draft);
    }
    // THE CAP IS ENFORCED ON WRITE (`check_body`), so a stored body over it is a row that got in
    // around it. Refused, not quoted and not cut: quoting 200,000 characters does not cost the
    // skill, it costs the turn, by crowding out the message the person actually sent; and cutting
    // it would hand the model half an instruction that nothing downstream could tell from a whole
    // one. The caller logs the numbers with the id.
    let length = body.chars().count();
    if length > MAX_SKILL_BODY_CHARS {
        return Err(NotForThisTurn::TooLong {
            length,
            cap: MAX_SKILL_BODY_CHARS,
        });
    }
    Ok(SkillForTurn {
        name: skill.name,
        body,
        author: if skill.owner_id == account.as_str() {
            crate::persona::SkillAuthor::Chooser
        } else {
            crate::persona::SkillAuthor::Colleague
        },
    })
}

/// What a person writes a skill down as, and what the row says it came from.
///
/// `taught` is missing on purpose: it is the server's word for a body a turn wrote down, and a
/// client that could claim it would be putting a sentence in the page that is not true.
const CLIENT_KINDS: [&str; 2] = ["authored", "uploaded"];

/// The word for a body a MODEL wrote. `from_tape` is the only place that writes it, and
/// `kind_or_refusal` refuses it from a client — the two halves of one claim: a row that says
/// `taught` was taught, and a page may say so.
const TAUGHT: &str = "taught";

/// What the version says about where the body came from. On the version rather than only the
/// row, because a later version a person writes by hand sits on the same skill and the history
/// has to be able to say which of them the model wrote.
const TAUGHT_NOTE: &str = "written from a screen recording";

/// What a taught skill's listing says when the person did not write a description.
///
/// OURS, NOT THE MODEL'S. A description rides in every row of every listing and reaches the
/// coworker that reads it to decide whether a skill is relevant; a second piece of model-written
/// prose there would be a second untrusted string to defend, for the sake of a subtitle. The
/// person renames and re-describes with `PUT /skills/{id}` once they have read the body.
const TAUGHT_DESCRIPTION: &str = "written from a screen recording; read it before you use it";

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
                // A skill a person wrote or uploaded is theirs and read: it works at once.
                // `from_tape`'s does not, and says why there.
                enabled: true,
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FromTapeRequest {
    /// Whose screen was recorded. The one field here that is not about the tape, and it is not
    /// decoration: the lesson is written by a model, and this says whose pin writes it, whose
    /// key the gateway bills and whose cap the spend guard checks (`tape_lesson`). It is also
    /// the thing this route authorises — see `from_tape`.
    coworker_id: String,
    /// Optional. Absent mints one, because a name is what a person types after a slash and the
    /// recording does not carry one. NEVER the model's: see `from_tape`.
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    screen: Option<Screen>,
    /// The raw tape (v1) — the same field, the same shape and the same recorder as
    /// `POST /recipes`, so the desktop app posts what it has already built when the person picks
    /// SKILL instead of RECIPE at the end of a recording.
    raw: Vec<TapeEvent>,
}

/// A name for a skill nobody has named yet. Short, valid by `check_name`, and obviously
/// provisional, because the person is about to read the body and rename it.
fn minted_name() -> String {
    let id = uuid::Uuid::now_v7().simple().to_string();
    format!("taught-{}", &id[..8.min(id.len())])
}

/// `POST /skills/from-tape` — a recording in, a skill a model wrote out, switched off.
///
/// THE THREE THINGS THIS ROUTE IS CAREFUL ABOUT, in the order they bite:
///
/// 1. WHOSE RECORDING IT IS. The account is the bearer's and never the body's (CLAUDE.md #7), and
///    the coworker named in the body has to be one of that account's — `owned_coworker`, the same
///    check `POST /recipes/{id}/run` makes before it plays a tape on a bot. A tape posted against
///    somebody else's coworker is refused as "no such coworker", not as "not yours": which
///    coworkers another account has is an answer about that account.
///
/// 2. WHAT THE MODEL WROTE IS UNTRUSTED. It is stored, and a later turn quotes it inside a
///    system message, so it goes through the SAME checks an uploaded `SKILL.md` goes through —
///    the one frontmatter parser, the body cap, the description cap — and its frontmatter's
///    `name` and `description` are DISCARDED, as they are on `POST /skills/{id}/versions`. The
///    model writes the lesson; it does not name the skill, does not describe it in every listing,
///    and does not decide whether it is on.
///
/// 3. NOTHING HALF-WRITTEN SURVIVES A FAILURE. Every refusal below happens before the row exists,
///    and the row and its first body are written in one transaction (`create_skill`), so a person
///    whose model refused is left with what they had: their recording, and no skill.
async fn from_tape(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Json(request): Json<FromTapeRequest>,
) -> Response {
    let Some(account) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker = CoworkerId::from_stored(request.coworker_id.clone());
    match owned_coworker(&state, &account, &coworker).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such coworker").into_response(),
        Err(refusal) => return refusal,
    }

    let name = request
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map_or_else(minted_name, str::to_string);
    if let Err(why) = check_name(&name) {
        return (StatusCode::BAD_REQUEST, why).into_response();
    }
    let description = request
        .description
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(TAUGHT_DESCRIPTION)
        .to_string();
    if let Err(why) = check_description(&description) {
        return why.into_response();
    }

    // ASKED BEFORE THE MODEL IS, not after. The name is refused either way, and asking afterwards
    // would bill the person for prose that was always going to be thrown away.
    let store = &state.auth.store;
    match store.skill_named(account.as_str(), &name).await {
        Ok(Some(_)) => return (StatusCode::CONFLICT, taken(&name)).into_response(),
        Ok(None) => {}
        Err(error) => return unavailable(error),
    }

    // The tape becomes steps through the recipe route's own road (`recipes::tape_into_steps`):
    // the same 5 MB ceiling, the same filter, the same lint, the same sentences. The serialised
    // tape it hands back is dropped here — a skill is the lesson, and storing the tape beside it
    // would be a recipe nobody asked for, under a row with no way to run it.
    let screen = request.screen.unwrap_or_default();
    let steps = match tape_into_steps(&request.raw, screen) {
        Ok((_tape, steps)) => steps,
        Err(refusal) => return refusal.into_response(),
    };

    // The coworker's own pin writes the lesson. Loaded rather than taken from the request: a
    // client that could name the model would be choosing what this account is billed for.
    let Ok((bot, _)) = state.auth.store.load_coworker(&coworker).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "that coworker could not be read",
        )
            .into_response();
    };
    let lesson = match crate::tape_lesson::lesson_from_tape(
        &state, &account, &coworker, &bot.model, &steps, screen,
    )
    .await
    {
        Ok(lesson) => lesson,
        Err(why) => {
            tracing::warn!(coworker = %coworker, %why, "a recording did not become a skill");
            // A TIMEOUT IS ITS OWN STATUS because it is the one of these worth retrying: the
            // recording was fine and the model was slow. The rest are 502 — the far side
            // answered, and what it answered with cannot be stored.
            let status = match why {
                crate::tape_lesson::NotWritten::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
                _ => StatusCode::BAD_GATEWAY,
            };
            return (status, why.to_string()).into_response();
        }
    };

    // ONE PARSER, the same one an upload goes through. A lesson that opens with `---` is read as
    // frontmatter by everything downstream, so it is read as frontmatter HERE too — otherwise a
    // model that wrote a title block would have that block stored as instructions and quoted into
    // a system message. Its `name` and `description` are dropped on the floor: what a model wrote
    // does not get to be what a person types after a slash, nor what every listing says.
    let parsed = opengrok_plugins::split_frontmatter(&lesson);
    if !parsed.closed {
        return (
            StatusCode::BAD_GATEWAY,
            "the model opened a `---` fence and never closed it, so where its frontmatter \
             ends and its instructions begin cannot be told apart",
        )
            .into_response();
    }
    let body = parsed.body.trim();
    if body.is_empty() {
        return (
            StatusCode::BAD_GATEWAY,
            "the model answered with frontmatter and no instructions",
        )
            .into_response();
    }
    // REFUSED, NOT CUT, and for `for_turn`'s reason: a body cut at 8000 characters ends
    // mid-sentence and nothing downstream can tell it from a whole one. The prompt asks for a
    // quarter of this, so a body that arrives over it is a model that ignored a plain
    // instruction — the last thing to paper over on the way into a system message.
    if let Err((_, why)) = check_body(body) {
        return (
            StatusCode::BAD_GATEWAY,
            format!("the model wrote more than a skill can hold: {why}"),
        )
            .into_response();
    }

    let org = org_of(&state, &account).await;
    let id = format!("skl_{}", uuid::Uuid::now_v7());
    let at_ms = now_ms();
    if let Err(error) = store
        .create_skill(
            NewSkill {
                id: &id,
                owner_id: account.as_str(),
                org_id: org.as_deref(),
                name: &name,
                description: &description,
                source: TAUGHT,
                // SWITCHED OFF IS THE REVIEW GATE, and it is the existing switch rather than a
                // second idea of "draft". A draft is `versionCount == 0` — a skill with nothing
                // written yet — and this one HAS a body: that is the whole point of the route,
                // and saying it was empty would be false on the one screen the person reads it
                // on. What is true is that nobody has read it: `for_turn` refuses a switched-off
                // skill even to its owner, and `skills_in_org` hides it from colleagues, so an
                // unread body cannot reach any turn. Approving it is `PUT /skills/{id}` with
                // `enabled: true` — a route that already exists and is already owner-only.
                enabled: false,
            },
            Some(NewSkillVersion {
                kind: TAUGHT,
                body,
                note: TAUGHT_NOTE,
                // No bundle. A model writing prose from a recording has no files to send, and a
                // route that accepted some here would be accepting them from the model.
                files: &[],
                created_by: account.as_str(),
            }),
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// The table, stated once here so a change to `may` has to be a deliberate edit of both.
    #[test]
    fn only_the_owner_writes_and_only_the_org_reads() {
        assert!(may(Relation::Owner, Action::Delete).is_ok());
        assert!(may(Relation::Owner, Action::AddVersion).is_ok());
        assert!(may(Relation::Owner, Action::Invoke).is_ok());
        assert!(may(Relation::OrgMember, Action::Read).is_ok());
        // The deliberate one: a colleague's skill may run inside this coworker's system message.
        // If this ever tightens, it tightens here and every caller inherits it.
        assert!(may(Relation::OrgMember, Action::Invoke).is_ok());
        assert!(may(Relation::OrgMember, Action::Edit).is_err());
        assert!(may(Relation::OrgMember, Action::Delete).is_err());
        assert!(may(Relation::None, Action::Read).is_err());
        assert!(may(Relation::None, Action::Invoke).is_err());
        assert_eq!(may(Relation::None, Action::Read), Err("no such skill"));
    }

    /// The sentence a refusal reaches the model with. A draft is the caller's own skill, so it is
    /// named; nothing else is, because an enumeration is a claim and it is false for at least two
    /// of these — a database blip is not "no such skill".
    #[test]
    fn a_refusal_names_a_cause_only_where_naming_one_leaks_nothing() {
        assert_eq!(
            NotForThisTurn::Draft.line(),
            crate::persona::SKILL_DRAFT_LINE
        );
        for refused in [
            NotForThisTurn::NoSuchSkill,
            NotForThisTurn::NotAnId,
            NotForThisTurn::Deleted,
            NotForThisTurn::Disabled,
            NotForThisTurn::NotTheirs,
            NotForThisTurn::Unquotable,
            NotForThisTurn::TooLong {
                length: 200_000,
                cap: MAX_SKILL_BODY_CHARS,
            },
            NotForThisTurn::Unreadable("the pool is closed".to_string()),
        ] {
            assert_eq!(
                refused.line(),
                crate::persona::SKILL_UNAVAILABLE_LINE,
                "{refused:?} must not describe a cause to the person"
            );
        }
        for word in ["deleted", "switched off", "no such skill"] {
            assert!(
                !crate::persona::SKILL_UNAVAILABLE_LINE.contains(word),
                "the shared sentence must stay true of a database blip too: {word}"
            );
        }
        // The store error is the operator's, and it must not travel to the model.
        assert!(
            !NotForThisTurn::Unreadable("connection refused".to_string())
                .line()
                .contains("connection refused")
        );
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
