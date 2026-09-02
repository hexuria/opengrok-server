//! A group's turn: the client's own orchestrator, transcribed (`group-chat-orchestrator.ts`,
//! `groups/group-chat.ts`), run on the server so a group is a coworker like any other.
//!
//! A group is a coworker with members and no model of its own. A prompt to it runs ROUNDS: in
//! each, the members the history addresses speak once, in an order that rotates by round; a
//! member's turn is a normal harness turn on that member's model, key, tools and policy, with
//! the room's system prompt and the messages since it last spoke; the ONLY way a member says
//! something the room sees is the `SendMessage` tool (plain text is private scratch); "(pass)"
//! is silence; the rounds stop when one produces nothing, or at the caps. Every constant here is
//! the client's, not ours.
//!
//! A card raised inside a member's turn — a tool that needs a person's yes — is the MEMBER's card
//! in the ROOM's transcript, under the member's name; the room pauses where its round stood
//! (`room_pause`), and the answer on the card resumes that member inside the room and then the
//! members still to speak. The verbs that answer cards accept the group as the agent
//! (`conversation.rs::run_belongs_to`).

use std::sync::{Arc, Mutex};

use opengrok_core::coworker::Coworker;
use opengrok_core::id::{AccountId, CoworkerId, RunId};
use opengrok_core::run::PendingApproval;
use opengrok_harness::{
    ChatMessage, ModelRequest, ResumeOutcome, Resumption, RunContext, ToolRunner,
    resume_conversation, run_conversation,
};
use opengrok_tools::{ToolCall, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::GatewayState;
use super::live;
use crate::agui::routes::StoreJournal;

/// The client's limits (`groups/group-chat.ts`).
const GROUP_MAX_ROUNDS: usize = 3;
const GROUP_MAX_MEMBER_TURNS: usize = 10;
const GROUP_MAX_MESSAGES_PER_TURN: usize = 2;
const GROUP_PROMPT_HISTORY_LIMIT: usize = 24;
const GROUP_CHAT_TAG_PREFIX: &str = "[Group chat: ";

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn entry_id() -> String {
    format!("e_{}", uuid::Uuid::now_v7().simple())
}

/// A member as the prompts describe it.
#[derive(Debug, Clone)]
pub struct Member {
    pub id: CoworkerId,
    pub name: String,
    pub description: String,
    pub model: String,
}

/// One line of the room's history, as the client reads it back out of the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Speaker {
    User { name: Option<String> },
    Member { id: String, name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMessage {
    pub speaker: Speaker,
    pub content: String,
}

/// `parseGroupMentions`: `@name`, `@firstname`, `@nameWithoutSpaces` as whole words; `@everyone`
/// and `@all` address the room.
fn mention_handles(name: &str) -> Vec<String> {
    let lower = name.trim().to_lowercase();
    if lower.is_empty() {
        return Vec::new();
    }
    let mut handles = vec![lower.clone(), lower.split_whitespace().collect::<String>()];
    if let Some(first) = lower.split_whitespace().next() {
        handles.push(first.to_string());
    }
    handles.dedup();
    handles
}

fn is_word_char(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_ascii_alphanumeric())
}

fn has_mention_at(lower: &str, handle: &str) -> bool {
    let needle = format!("@{handle}");
    let chars: Vec<char> = lower.chars().collect();
    let needle_chars: Vec<char> = needle.chars().collect();
    let mut index = 0;
    while index + needle_chars.len() <= chars.len() {
        if chars[index..index + needle_chars.len()] == needle_chars[..] {
            let before = if index == 0 {
                None
            } else {
                Some(chars[index - 1])
            };
            let after = chars.get(index + needle_chars.len()).copied();
            if !is_word_char(before) && !is_word_char(after) {
                return true;
            }
        }
        index += 1;
    }
    false
}

fn parse_mentions(text: &str, members: &[Member]) -> (bool, Vec<String>) {
    let lower = text.to_lowercase();
    let mut ids = Vec::new();
    for member in members {
        if !ids.contains(&member.id.as_str().to_string())
            && mention_handles(&member.name)
                .iter()
                .any(|handle| has_mention_at(&lower, handle))
        {
            ids.push(member.id.as_str().to_string());
        }
    }
    let everyone = ["everyone", "all"]
        .iter()
        .any(|handle| has_mention_at(&lower, handle));
    (everyone, ids)
}

/// `resolveResponders`: everybody, unless the messages since the last user message mention
/// specific members and nobody said `@everyone`.
pub fn resolve_responders(members: &[Member], history: &[GroupMessage]) -> Vec<Member> {
    let start = history
        .iter()
        .rposition(|m| matches!(m.speaker, Speaker::User { .. }))
        .unwrap_or(0);
    let mut everyone = false;
    let mut mentioned: Vec<String> = Vec::new();
    for message in &history[start..] {
        let (all, ids) = parse_mentions(&message.content, members);
        everyone |= all;
        for id in ids {
            if !mentioned.contains(&id) {
                mentioned.push(id);
            }
        }
    }
    if everyone || mentioned.is_empty() {
        members.to_vec()
    } else {
        members
            .iter()
            .filter(|m| mentioned.contains(&m.id.as_str().to_string()))
            .cloned()
            .collect()
    }
}

/// `orderRoundSpeakers`: rotate by round so the same member does not always open.
pub fn order_round_speakers(members: &[Member], round: usize) -> Vec<Member> {
    if members.is_empty() {
        return Vec::new();
    }
    let offset = round % members.len();
    let mut ordered = members[offset..].to_vec();
    ordered.extend_from_slice(&members[..offset]);
    ordered
}

/// `isPassContent`: nothing, or "(pass)" in its spellings.
pub fn is_pass(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return true;
    }
    let inner = trimmed
        .trim_start_matches('(')
        .trim_end_matches('.')
        .trim_end_matches(')')
        .trim();
    inner.eq_ignore_ascii_case("pass")
}

fn describe_group(name: &str, description: &str) -> String {
    let name = if name.trim().is_empty() {
        "the group"
    } else {
        name.trim()
    };
    let description = description.trim();
    if description.is_empty() {
        format!("\"{name}\"")
    } else {
        format!("\"{name}\" — {description}")
    }
}

/// `buildGroupMemberSystemPrompt`, word for word.
pub fn member_system_prompt(
    member: &Member,
    group_name: &str,
    group_description: &str,
    peers: &[Member],
) -> String {
    let mut lines = vec![format!(
        "You are {}, one participant in a group chat ({}).",
        member.name,
        describe_group(group_name, group_description)
    )];
    if !member.description.trim().is_empty() {
        lines.push(format!("Your persona: {}", member.description.trim()));
    }
    if !peers.is_empty() {
        lines.push(String::new());
        lines.push("Other participants in the room:".to_string());
        for peer in peers {
            let description = peer.description.trim();
            lines.push(if description.is_empty() {
                format!("- {}", peer.name)
            } else {
                format!("- {} ({description})", peer.name)
            });
        }
    }
    lines.push(String::new());
    lines.push(if peers.is_empty() {
        "Right now you are speaking in this group chat.".to_string()
    } else {
        format!(
            "Right now you are speaking in this group chat, with {}.",
            peers
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    });
    lines.push(
        "You have your full toolkit in this room. Do the work first, then deliver the result with SendMessage."
            .to_string(),
    );
    lines.push(String::new());
    lines.push(format!(
        "Stay fully in character as {}. The ONLY way to say something the room can see is the SendMessage tool. Keep each message short and conversational. If you have nothing new worth adding, send exactly \"(pass)\". Never reveal private one-on-one context.",
        member.name
    ));
    lines.join("\n")
}

fn format_line(message: &GroupMessage, viewer: &CoworkerId) -> String {
    match &message.speaker {
        Speaker::User { name: Some(name) } => format!("{name} (user): {}", message.content),
        Speaker::User { name: None } => format!("User: {}", message.content),
        Speaker::Member { id, name } => format!(
            "{name}{}: {}",
            if id == viewer.as_str() { " (you)" } else { "" },
            message.content
        ),
    }
}

/// `messagesSinceMemberLastSpoke`.
pub fn messages_since_last_spoke<'a>(
    history: &'a [GroupMessage],
    member: &CoworkerId,
) -> &'a [GroupMessage] {
    match history
        .iter()
        .rposition(|m| matches!(&m.speaker, Speaker::Member { id, .. } if id == member.as_str()))
    {
        Some(index) => &history[index + 1..],
        None => history,
    }
}

/// `buildGroupTurnPrompt`, word for word.
pub fn turn_prompt(
    member: &Member,
    group_name: &str,
    peers: &[Member],
    new_messages: &[GroupMessage],
) -> String {
    let tag = format!(
        "{GROUP_CHAT_TAG_PREFIX}\"{}\"{}]",
        if group_name.trim().is_empty() {
            "the group"
        } else {
            group_name.trim()
        },
        if peers.is_empty() {
            String::new()
        } else {
            format!(
                " - with {}",
                peers
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    );
    let recent: Vec<String> = new_messages
        .iter()
        .rev()
        .take(GROUP_PROMPT_HISTORY_LIMIT)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|m| format_line(m, &member.id))
        .collect();
    let body = if new_messages.is_empty() {
        "No new messages in the room since your last turn.".to_string()
    } else {
        format!(
            "New messages in the room (oldest first):\n{}",
            recent.join("\n")
        )
    };
    [
        tag,
        body,
        String::new(),
        format!(
            "It's your turn, {}. Reply in character with a single SendMessage if you have something worth adding, or send \"(pass)\" if you don't.",
            member.name
        ),
    ]
    .join("\n")
}

/// The `SendMessage` tool the room offers, as OpenAI function-calling sees it.
pub fn send_message_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "SendMessage",
            "description": "Say something the whole room can see. Everything else you write is private. Send exactly \"(pass)\" to say nothing this turn.",
            "parameters": {
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "What the room sees." }
                },
                "required": ["content"]
            }
        }
    })
}

/// The room's history as the client reads it: user messages, and members' `send-message`
/// entries with an author. A streaming placeholder and anything else is not a line.
pub fn history_of(entries: &[Value]) -> Vec<GroupMessage> {
    let mut history = Vec::new();
    for entry in entries {
        match entry.get("kind").and_then(Value::as_str) {
            Some("message") if entry.get("role").and_then(Value::as_str) == Some("user") => {
                let content = entry
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if content.is_empty() {
                    continue;
                }
                let name = entry
                    .pointer("/fromUser/name")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                history.push(GroupMessage {
                    speaker: Speaker::User { name },
                    content: match super::conversation::reply_context(entries, entry) {
                        Some(context) => format!("{context}\n\n{content}"),
                        None => content.to_string(),
                    },
                });
            }
            Some("send-message")
                if entry.pointer("/message/type").and_then(Value::as_str) == Some("text")
                    && entry.get("author").is_some()
                    && entry.get("streaming").and_then(Value::as_bool) != Some(true) =>
            {
                let (Some(id), Some(name)) = (
                    entry.pointer("/author/id").and_then(Value::as_str),
                    entry.pointer("/author/name").and_then(Value::as_str),
                ) else {
                    continue;
                };
                history.push(GroupMessage {
                    speaker: Speaker::Member {
                        id: id.to_string(),
                        name: name.to_string(),
                    },
                    content: entry
                        .pointer("/message/content")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                });
            }
            _ => {}
        }
    }
    history
}

/// The members that can take a turn: the group's list, minus anybody retired or gone.
async fn resolve_members(state: &GatewayState, group: &Coworker) -> Vec<Member> {
    let mut members = Vec::with_capacity(group.members.len());
    for id in &group.members {
        let Ok((coworker, _)) = state.agui.auth.store.load_coworker(id).await else {
            continue;
        };
        if coworker.retired || coworker.is_group() {
            continue;
        }
        let description = state
            .agui
            .auth
            .store
            .seamb_profile(id)
            .await
            .ok()
            .flatten()
            .and_then(|profile| {
                profile
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default();
        members.push(Member {
            id: id.clone(),
            name: coworker.name,
            description,
            model: coworker.model,
        });
    }
    members
}

/// Post what a member said, under its name, to the group's transcript and the live stream.
async fn post_member_message(
    state: &GatewayState,
    group_id: &CoworkerId,
    member: &Member,
    content: &str,
) {
    let entry = json!({
        "kind": "send-message",
        "id": entry_id(),
        "message": { "type": "text", "content": content },
        "timestampMs": now_ms(),
        "author": { "id": member.id.as_str(), "name": member.name },
    });
    if let Err(error) = state
        .agui
        .auth
        .store
        .append_gateway_entry(group_id, &entry, now_ms())
        .await
    {
        tracing::error!(%error, group = %group_id.as_str(), "group: a member's message could not be appended");
        return;
    }
    live::emit_transcript(state, group_id.as_str(), "appended", entry);
}

/// The group as the prompts describe it.
struct Room<'a> {
    id: &'a CoworkerId,
    name: &'a str,
    description: &'a str,
}

/// What a member's turn came to: what it sent, in order — or the card it raised, with the run
/// now waiting on a person.
enum MemberOutcome {
    Spoke(Vec<String>),
    Suspended {
        run_id: RunId,
        suspension: super::conversation::Suspension,
    },
}

/// The member's tools: its own runner — its policy, its computer, its gate — with the room's
/// `SendMessage` added, delivering into the returned list. `gate_yes`/`review_yes` carry an
/// answered call id on a resume, exactly as a coworker's own resume does.
async fn member_runner(
    state: &GatewayState,
    account_id: &AccountId,
    member: &Member,
    gate_yes: &[String],
    review_yes: &[String],
) -> (ToolRunner, Arc<Mutex<Vec<String>>>) {
    let sent: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = sent.clone();
    let deliver: opengrok_harness::LocalTool = Arc::new(move |call: &ToolCall| {
        let content = call
            .arguments
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if let Ok(mut sent) = sink.lock() {
            sent.push(content);
        }
        ToolResult {
            call_id: call.id.clone(),
            ok: true,
            content: "delivered to the room".to_string(),
            awaiting_approval: false,
            awaiting_reason: None,
        }
    });
    let runner = crate::agui::routes::tools_for_coworker(
        &state.agui,
        account_id,
        &member.id,
        gate_yes,
        review_yes,
        crate::agui::routes::TURN_WAKE_PATIENCE,
    )
    .await
    .unwrap_or_else(ToolRunner::local_only)
    .with_local(send_message_schema(), deliver);
    (runner, sent)
}

/// What the room hears of a member's turn: its messages minus passes, capped.
fn spoken(sent: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    sent.lock()
        .map(|sent| sent.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|content| !is_pass(content))
        .take(GROUP_MAX_MESSAGES_PER_TURN)
        .collect()
}

/// One member's turn: a normal harness turn on the member's own model, key, tools and policy,
/// with the room's prompts and the `SendMessage` tool added.
async fn run_member_turn(
    state: &GatewayState,
    account_id: &AccountId,
    room: &Room<'_>,
    member: &Member,
    peers: &[Member],
    history: &[GroupMessage],
) -> MemberOutcome {
    let group_id = room.id;
    let (runner, sent) = member_runner(state, account_id, member, &[], &[]).await;
    let new_messages = messages_since_last_spoke(history, &member.id);
    let request = ModelRequest {
        model: member.model.clone(),
        system: Some(member_system_prompt(
            member,
            room.name,
            room.description,
            peers,
        )),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: turn_prompt(member, room.name, peers, new_messages),
        }],
        tools: Vec::new(),
        gateway_key: crate::spend::key_for(&state.agui, &member.id).await,
        spend_scope: Some(member.id.as_str().to_string()),
    };
    let thread_id = format!("gateway-{}", group_id.as_str());
    let run_id = RunId::new();
    let journal = StoreJournal {
        state: state.agui.clone(),
        thread_id: thread_id.clone(),
        account_id: Some(account_id.clone()),
        coworker_id: Some(member.id.clone()),
        model: Some(member.model.clone()),
    };
    let events = run_conversation(
        state.agui.door.as_ref(),
        Some(&runner),
        &journal,
        request,
        &thread_id,
        run_id.as_str(),
        now_ms(),
    )
    .await;
    if events
        .iter()
        .any(|event| event.event_type == opengrok_wire::agui::EventType::RunError)
    {
        tracing::warn!(member = %member.id.as_str(), group = %group_id.as_str(), "group: a member's turn failed; it said nothing");
    }
    if let Some(suspension) = super::conversation::find_suspension(&events) {
        return MemberOutcome::Suspended { run_id, suspension };
    }
    MemberOutcome::Spoke(spoken(&sent))
}

/// Where a round stood when a member's run suspended: persisted with the pause so the answer
/// continues the round, not the prompt from the top. `remaining` is the members still to speak
/// in this round, in order, after the paused one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundCursor {
    pub round: usize,
    pub total: usize,
    pub this_round: usize,
    pub remaining: Vec<String>,
}

/// The card a member raised, into the ROOM's transcript under the member's name, and the round's
/// position with it. `true` when the room is now paused on a card; `false` when this kind of
/// pause has no card yet — the member's turn then ends with nothing said, as it always did.
async fn pause_room(
    state: &GatewayState,
    room: &Room<'_>,
    member: &Member,
    run_id: &RunId,
    suspension: &super::conversation::Suspension,
    cursor: &RoundCursor,
) -> bool {
    let Some(mut card) = super::conversation::card_for(suspension) else {
        tracing::warn!(
            member = %member.id.as_str(),
            group = %room.id.as_str(),
            tool = %suspension.tool,
            reason = suspension.reason.as_str(),
            "group: a member's run suspended for a reason that has no card yet; its turn ends with nothing said"
        );
        return false;
    };
    card["author"] = json!({ "id": member.id.as_str(), "name": member.name });
    if let Err(error) = state
        .agui
        .auth
        .store
        .append_gateway_entry(room.id, &card, now_ms())
        .await
    {
        tracing::error!(%error, group = %room.id.as_str(), "group: a member's card could not be appended");
    }
    live::emit_transcript(state, room.id.as_str(), "appended", card);
    if let Err(error) = state
        .agui
        .auth
        .store
        .save_room_pause(room.id, run_id, &member.id, &json!(cursor), now_ms())
        .await
    {
        // The card is out and the run waits either way; without the cursor the answer resumes
        // the member and stops there instead of finishing the round.
        tracing::error!(%error, group = %room.id.as_str(), "group: the round's position could not be saved");
    }
    tracing::info!(
        member = %member.id.as_str(),
        group = %room.id.as_str(),
        run = %run_id.as_str(),
        round = cursor.round,
        "group: paused on a member's card"
    );
    true
}

/// The rounds from a cursor — the client's loop, made resumable. Ends at the caps, on a round in
/// which nobody spoke, or on a card (the room is then paused; `resume_member_turn` continues).
async fn run_rounds(
    state: &GatewayState,
    account_id: &AccountId,
    room: &Room<'_>,
    members: &[Member],
    history: &mut Vec<GroupMessage>,
    cursor: RoundCursor,
) {
    let agent_id = room.id.as_str().to_string();
    let mut round = cursor.round;
    let mut total = cursor.total;
    let mut this_round = cursor.this_round;
    let mut speakers: Vec<Member> = cursor
        .remaining
        .iter()
        .filter_map(|id| members.iter().find(|m| m.id.as_str() == id).cloned())
        .collect();
    loop {
        while !speakers.is_empty() {
            let member = speakers.remove(0);
            if total >= GROUP_MAX_MEMBER_TURNS {
                return;
            }
            let peers: Vec<Member> = members
                .iter()
                .filter(|m| m.id != member.id)
                .cloned()
                .collect();
            live::set_running(
                state,
                &agent_id,
                true,
                json!({ "activeRemoteMemberId": member.id.as_str() }),
            )
            .await;
            match run_member_turn(state, account_id, room, &member, &peers, history).await {
                MemberOutcome::Spoke(sent) => {
                    for content in sent {
                        post_member_message(state, room.id, &member, &content).await;
                        history.push(GroupMessage {
                            speaker: Speaker::Member {
                                id: member.id.as_str().to_string(),
                                name: member.name.clone(),
                            },
                            content,
                        });
                        total += 1;
                        this_round += 1;
                        if total >= GROUP_MAX_MEMBER_TURNS {
                            return;
                        }
                    }
                }
                MemberOutcome::Suspended { run_id, suspension } => {
                    let at = RoundCursor {
                        round,
                        total,
                        this_round,
                        remaining: speakers.iter().map(|m| m.id.as_str().to_string()).collect(),
                    };
                    if pause_room(state, room, &member, &run_id, &suspension, &at).await {
                        return;
                    }
                }
            }
        }
        if this_round == 0 {
            return;
        }
        round += 1;
        if round >= GROUP_MAX_ROUNDS {
            return;
        }
        this_round = 0;
        let responders = resolve_responders(members, history);
        speakers = order_round_speakers(&responders, round);
    }
}

/// The room's description, from the group's seam-B profile.
async fn room_description(state: &GatewayState, group_id: &CoworkerId) -> String {
    state
        .agui
        .auth
        .store
        .seamb_profile(group_id)
        .await
        .ok()
        .flatten()
        .and_then(|profile| {
            profile
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// The room's turn, off the request's clock: `GroupChatOrchestrator.run`, transcribed.
pub async fn run_group_turn(
    state: GatewayState,
    account_id: AccountId,
    group_id: CoworkerId,
    group: Coworker,
) {
    let agent_id = group_id.as_str().to_string();
    let _guard = RoomGuard {
        state: state.clone(),
        agent_id: agent_id.clone(),
    };
    let description = room_description(&state, &group_id).await;
    let members = resolve_members(&state, &group).await;
    if members.is_empty() {
        return;
    }
    let mut history = state
        .agui
        .auth
        .store
        .gateway_transcript(&group_id)
        .await
        .map(|entries| history_of(&entries))
        .unwrap_or_default();
    // A new prompt abandons the pause an earlier one may be holding: that card can still be
    // answered — the member then speaks — but its round is not continued.
    if let Err(error) = state.agui.auth.store.clear_room_pause(&group_id).await {
        tracing::warn!(%error, group = %agent_id, "group: an earlier pause could not be cleared");
    }
    let room = Room {
        id: &group_id,
        name: &group.name,
        description: &description,
    };
    let first: Vec<String> = order_round_speakers(&resolve_responders(&members, &history), 0)
        .iter()
        .map(|m| m.id.as_str().to_string())
        .collect();
    run_rounds(
        &state,
        &account_id,
        &room,
        &members,
        &mut history,
        RoundCursor {
            round: 0,
            total: 0,
            this_round: 0,
            remaining: first,
        },
    )
    .await;
}

/// The answer on a member's card: resume THAT member's run inside the room — its words go to the
/// room under its name — then finish the round where it stood. Spawned by the verbs that answer
/// cards when the run's thread is a room's.
pub async fn resume_member_turn(
    state: GatewayState,
    account_id: AccountId,
    run_id: RunId,
    group_id: CoworkerId,
    pending: PendingApproval,
    resumed_seq: u32,
    outcome: ResumeOutcome,
) {
    let agent_id = group_id.as_str().to_string();
    let Ok((run, _)) = state.agui.auth.store.load_run(&run_id).await else {
        return;
    };
    let Some(member_id) = run.coworker_id.clone() else {
        return;
    };
    let Ok((group, _)) = state.agui.auth.store.load_coworker(&group_id).await else {
        return;
    };
    let pause = state
        .agui
        .auth
        .store
        .take_room_pause_for_run(&run_id)
        .await
        .ok()
        .flatten();
    let _guard = RoomGuard {
        state: state.clone(),
        agent_id: agent_id.clone(),
    };
    let description = room_description(&state, &group_id).await;
    let members = resolve_members(&state, &group).await;
    let Some(member) = members.iter().find(|m| m.id == member_id).cloned() else {
        tracing::warn!(member = %member_id.as_str(), group = %agent_id, "group: the member whose card was answered is no longer in the room");
        return;
    };
    let peers: Vec<Member> = members
        .iter()
        .filter(|m| m.id != member.id)
        .cloned()
        .collect();
    let room = Room {
        id: &group_id,
        name: &group.name,
        description: &description,
    };
    let mut history = state
        .agui
        .auth
        .store
        .gateway_transcript(&group_id)
        .await
        .map(|entries| history_of(&entries))
        .unwrap_or_default();
    live::set_running(
        &state,
        &agent_id,
        true,
        json!({ "activeRemoteMemberId": member.id.as_str() }),
    )
    .await;

    // The answered call rides the runner as a GATE yes (the machine owner's or the policy's
    // card) or a REVIEW yes, by the suspension's reason — as a coworker's own resume does.
    let (gate_yes, review_yes): (&[String], &[String]) = match pending.reason {
        opengrok_core::run::SuspendReason::AutoReview => {
            (&[], std::slice::from_ref(&pending.call_id))
        }
        _ => (std::slice::from_ref(&pending.call_id), &[]),
    };
    let (runner, sent) = member_runner(&state, &account_id, &member, gate_yes, review_yes).await;
    let journal = StoreJournal {
        state: state.agui.clone(),
        thread_id: run.thread_id.clone(),
        account_id: Some(account_id.clone()),
        coworker_id: Some(member.id.clone()),
        model: run.model.clone(),
    };
    // The room's system prompt again: the journal holds the conversation, not the instructions,
    // and a member resumed without them would not know it is in a room.
    let request = ModelRequest {
        model: run.pin_for_resume(&member.model),
        system: Some(member_system_prompt(
            &member,
            &group.name,
            &description,
            &peers,
        )),
        messages: crate::agui::routes::conversation_from(&run),
        tools: Vec::new(),
        gateway_key: crate::spend::key_for(&state.agui, &member.id).await,
        spend_scope: Some(member.id.as_str().to_string()),
    };
    let events = resume_conversation(
        state.agui.door.as_ref(),
        &runner,
        &journal,
        request,
        RunContext::new(&run.thread_id, run_id.as_str(), now_ms()),
        Resumption {
            approved: ToolCall {
                id: pending.call_id,
                name: pending.tool,
                arguments: pending.arguments,
            },
            message_seq: resumed_seq,
            outcome,
        },
    )
    .await;

    // Without a saved position (a newer prompt took the room, or the row was lost) the member's
    // words still land, and the round is not continued.
    let mut cursor = pause
        .and_then(|pause| serde_json::from_value::<RoundCursor>(pause.cursor).ok())
        .unwrap_or(RoundCursor {
            round: GROUP_MAX_ROUNDS,
            total: 0,
            this_round: 0,
            remaining: Vec::new(),
        });
    for content in spoken(&sent) {
        post_member_message(&state, &group_id, &member, &content).await;
        history.push(GroupMessage {
            speaker: Speaker::Member {
                id: member.id.as_str().to_string(),
                name: member.name.clone(),
            },
            content,
        });
        cursor.total += 1;
        cursor.this_round += 1;
    }
    // A resumed member may raise another card; it gets one exactly like the first.
    if let Some(suspension) = super::conversation::find_suspension(&events)
        && pause_room(&state, &room, &member, &run_id, &suspension, &cursor).await
    {
        return;
    }
    if cursor.total >= GROUP_MAX_MEMBER_TURNS || cursor.round >= GROUP_MAX_ROUNDS {
        return;
    }
    run_rounds(&state, &account_id, &room, &members, &mut history, cursor).await;
}

/// Clears the group's running state on every way out, including a stopAgentTurn abort, and
/// shows the last thing said in the sidebar.
struct RoomGuard {
    state: GatewayState,
    agent_id: String,
}

impl Drop for RoomGuard {
    fn drop(&mut self) {
        if let Ok(mut cancels) = self.state.cancels.lock() {
            cancels.remove(&self.agent_id);
        }
        let state = self.state.clone();
        let agent_id = self.agent_id.clone();
        tokio::spawn(async move {
            let last = state
                .agui
                .auth
                .store
                .gateway_transcript(&CoworkerId::from_stored(agent_id.clone()))
                .await
                .ok()
                .and_then(|entries| {
                    entries.iter().rev().find_map(|e| {
                        e.pointer("/message/content")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                });
            let preview: String = last.unwrap_or_default().chars().take(120).collect();
            live::set_running(
                &state,
                &agent_id,
                false,
                json!({
                    "activeRemoteMemberId": Value::Null,
                    "lastMessagePreview": preview,
                    "lastEntry": { "kind": "text", "text": preview },
                }),
            )
            .await;
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn member(id: &str, name: &str) -> Member {
        Member {
            id: CoworkerId::from_stored(id),
            name: name.to_string(),
            description: String::new(),
            model: "oag/cheap".to_string(),
        }
    }

    fn user(text: &str) -> GroupMessage {
        GroupMessage {
            speaker: Speaker::User { name: None },
            content: text.to_string(),
        }
    }

    fn said(id: &str, name: &str, text: &str) -> GroupMessage {
        GroupMessage {
            speaker: Speaker::Member {
                id: id.to_string(),
                name: name.to_string(),
            },
            content: text.to_string(),
        }
    }

    #[test]
    fn mentions_pick_responders_and_everyone_or_nobody_means_all() {
        let members = vec![member("cw_a", "Ada Lovelace"), member("cw_b", "Bob")];
        let all = resolve_responders(&members, &[user("hello room")]);
        assert_eq!(all.len(), 2);
        let ada = resolve_responders(&members, &[user("@ada what do you think?")]);
        assert_eq!(ada.len(), 1);
        assert_eq!(ada[0].name, "Ada Lovelace");
        let ada = resolve_responders(&members, &[user("hey @adalovelace")]);
        assert_eq!(ada.len(), 1);
        let none = resolve_responders(&members, &[user("email me at bob@example.com")]);
        assert_eq!(none.len(), 2, "@ inside a word is not a mention");
        let everyone = resolve_responders(&members, &[user("@ada and @everyone")]);
        assert_eq!(everyone.len(), 2);
        // Only the messages since the LAST user message count.
        let later = resolve_responders(
            &members,
            &[
                user("@ada"),
                said("cw_a", "Ada Lovelace", "hi"),
                user("thanks all"),
            ],
        );
        assert_eq!(later.len(), 2);
    }

    #[test]
    fn rounds_rotate_and_pass_is_silence() {
        let members = vec![
            member("cw_a", "Ada"),
            member("cw_b", "Bob"),
            member("cw_c", "Cy"),
        ];
        let names = |round: usize| {
            order_round_speakers(&members, round)
                .iter()
                .map(|m| m.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(0), ["Ada", "Bob", "Cy"]);
        assert_eq!(names(1), ["Bob", "Cy", "Ada"]);
        assert_eq!(names(2), ["Cy", "Ada", "Bob"]);
        assert_eq!(names(3), ["Ada", "Bob", "Cy"]);
        for pass in ["(pass)", "pass", "PASS.", "( pass )", "", "  "] {
            assert!(is_pass(pass), "{pass:?}");
        }
        assert!(!is_pass("I pass the ball"));
    }

    #[test]
    fn the_prompts_are_the_clients_word_for_word() {
        let ada = Member {
            id: CoworkerId::from_stored("cw_a"),
            name: "Ada".to_string(),
            description: "a careful reviewer".to_string(),
            model: "oag/cheap".to_string(),
        };
        let bob = member("cw_b", "Bob");
        let system = member_system_prompt(
            &ada,
            "Review room",
            "where code is reviewed",
            std::slice::from_ref(&bob),
        );
        assert!(system.starts_with("You are Ada, one participant in a group chat (\"Review room\" — where code is reviewed).\nYour persona: a careful reviewer\n\nOther participants in the room:\n- Bob\n\nRight now you are speaking in this group chat, with Bob.\n"), "{system}");
        assert!(system.ends_with("If you have nothing new worth adding, send exactly \"(pass)\". Never reveal private one-on-one context."), "{system}");
        let history = vec![
            user("ship it?"),
            said("cw_b", "Bob", "looks fine"),
            said("cw_a", "Ada", "one nit"),
            said("cw_b", "Bob", "fixed"),
        ];
        let since = messages_since_last_spoke(&history, &ada.id);
        assert_eq!(since.len(), 1);
        let prompt = turn_prompt(&ada, "Review room", &[bob], since);
        assert_eq!(
            prompt,
            "[Group chat: \"Review room\" - with Bob]\nNew messages in the room (oldest first):\nBob: fixed\n\nIt's your turn, Ada. Reply in character with a single SendMessage if you have something worth adding, or send \"(pass)\" if you don't."
        );
        let fresh = turn_prompt(&ada, "Review room", &[], &[]);
        assert!(fresh.contains("No new messages in the room since your last turn."));
    }

    #[test]
    fn the_history_is_read_the_way_the_client_reads_it() {
        let entries = vec![
            json!({ "kind": "message", "role": "user", "content": "hi all" }),
            json!({ "kind": "send-message", "message": { "type": "text", "content": "" }, "streaming": true }),
            json!({ "kind": "send-message", "message": { "type": "text", "content": "Ada here" }, "author": { "id": "cw_a", "name": "Ada" } }),
            json!({ "kind": "send-message", "message": { "type": "text", "content": "no author, not a line" } }),
            json!({ "kind": "message", "role": "assistant", "content": "not a user line" }),
        ];
        let history = history_of(&entries);
        assert_eq!(
            history,
            vec![user("hi all"), said("cw_a", "Ada", "Ada here")]
        );
    }
}
