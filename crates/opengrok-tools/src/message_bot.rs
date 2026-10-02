//! `message_bot` (#314): one of a person's Bots messages another of theirs, or several, each in
//! the side thread the two share. The call only writes the messages down (`BotMail`); the turn a
//! message gives its receiver is started by the server from that durable row, so a message
//! outlives whatever happens to the turn that sent it.
//!
//! A CEILING ROW, ON BY DEFAULT (`Executor::every_builtin`), but not the executor's to run: the
//! server offers it (`ToolRunner::with_bots`) only when the person driving the turn owns the
//! sender, the sender's ceiling allows it, the sending run is under `MAX_HOPS` and there is another
//! Bot to name, and its `BotMail` asks all of that again, from the store, when a call comes.
//!
//! THE WORDS ARE THE CONTRACT'S (hexuria/opengrok-server#314, "Contract of record", agreed with
//! NativeChat on 2 Oct 2026): the arguments, the result's two lists and the three refusals.

use serde_json::{Value, json};

use crate::{ToolCall, ToolResult};

pub const MESSAGE_BOT: &str = "message_bot";

/// What `/coworkers/{id}/ceiling` and the model are told the tool is for.
pub const MESSAGE_BOT_DESCRIPTION: &str = "Send a message to another of your person's Bots, or \
     to several at once. Each one gets it in the side thread it shares with you, which your person \
     can read, and may answer you there with this tool. Only this tool reaches another Bot: what \
     you write in a reply is never sent to one. Use it when another Bot is better placed to do part \
     of the work or needs to know something; do not use it to chat.";

/// The most Bots one call may name.
pub const MAX_RECIPIENTS: usize = 8;
/// The longest message, in characters.
pub const MAX_MESSAGE_CHARS: usize = 8000;
/// How many messages deep a chain may go. A turn started by a message of this hop is not offered
/// the tool, and a call from one is refused: a chain cannot outgrow it (Lean `Chain`).
pub const MAX_HOPS: u32 = 4;
/// The most messages one chain may hold, every hop and every receiver counted.
pub const PER_CHAIN: i64 = 12;
/// The most messages one person's Bots may send in any hour.
pub const PER_HOUR: i64 = 60;

/// One of the person's Bots, as a call names it: `label` is its name, or `Name (cw_…)` when two
/// of them share the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotOffer {
    pub id: String,
    pub name: String,
    pub label: String,
}

/// The person's Bots, `(id, name)`, as calls may name them. The sender is kept, so a call naming
/// it is told so; the schema leaves it out.
pub fn offers(roster: &[(String, String)]) -> Vec<BotOffer> {
    let shared = |name: &str| roster.iter().filter(|(_, other)| other == name).count() > 1;
    let label = |id: &str, name: &str| match shared(name) {
        true => format!("{name} ({id})"),
        false => name.to_string(),
    };
    roster
        .iter()
        .map(|(id, name)| BotOffer {
            id: id.clone(),
            name: name.clone(),
            label: label(id, name),
        })
        .collect()
}

/// The function definition: `to` held to the Bots the sender may name.
pub fn schema(offers: &[BotOffer], sender: &str) -> Value {
    let labels: Vec<&str> = offers
        .iter()
        .filter(|bot| bot.id != sender)
        .map(|bot| bot.label.as_str())
        .collect();
    let to = json!({ "type": "array", "items": { "type": "string", "enum": labels },
        "minItems": 1, "maxItems": MAX_RECIPIENTS, "uniqueItems": true,
        "description": "The Bots to send it to, by name: one, or up to eight for the same message." });
    let message = json!({ "type": "string", "maxLength": MAX_MESSAGE_CHARS,
        "description": "What to tell them, in full: they see nothing else of this conversation." });
    json!({ "type": "function", "function": { "name": MESSAGE_BOT,
        "description": MESSAGE_BOT_DESCRIPTION,
        "parameters": { "type": "object", "properties": { "to": to, "message": message },
            "required": ["to", "message"] } } })
}

/// One message written down: the Bot, its pair thread, and the outbox row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivered {
    pub coworker_id: String,
    pub thread_id: String,
    pub message_id: String,
}

/// Where a call's messages are written. The server implements it over the store, as the person
/// the turn is for and as the sending run, so a call has no say in who sends or how deep it is.
#[async_trait::async_trait]
pub trait BotMail: Send + Sync {
    /// Write one call's message to each of `to`, all or none: what each became, or the sentence
    /// every one of them is refused in.
    async fn send(
        &self,
        call_id: &str,
        to: &[BotOffer],
        message: &str,
    ) -> Result<Vec<Delivered>, String>;
}

/// Which of the person's Bots a name means, or the contract's refusal. A label first; else a bare
/// name that one Bot other than the sender has, as written and then in any case.
fn resolve<'a>(asked: &str, offers: &'a [BotOffer], sender: &str) -> Result<&'a BotOffer, String> {
    let others = || offers.iter().filter(|bot| bot.id != sender);
    let one = |found: Vec<&'a BotOffer>| (found.len() == 1).then(|| found[0]);
    let by_label = offers.iter().find(|bot| bot.label == asked);
    let by_name = || one(others().filter(|bot| bot.name == asked).collect());
    let any_case = || {
        one(others()
            .filter(|bot| bot.name.eq_ignore_ascii_case(asked))
            .collect())
    };
    match by_label.or_else(by_name).or_else(any_case) {
        Some(bot) if bot.id != sender => Ok(bot),
        found => {
            let me = offers.iter().find(|bot| bot.id == sender);
            let named_me = me.is_some_and(|me| me.name.eq_ignore_ascii_case(asked));
            if found.is_some() || named_me {
                return Err(format!("\"{asked}\" is you; message another Bot"));
            }
            let labels: Vec<&str> = others().map(|bot| bot.label.as_str()).collect();
            Err(format!(
                "no Bot of yours is called \"{asked}\"; you can message: {}",
                labels.join(", ")
            ))
        }
    }
}

/// One call: `{delivered: [{bot, coworkerId, threadId, messageId}], refused: [{bot, why}]}`, a
/// result either way (CLAUDE.md #8). Arguments it cannot read are refused in words saying the shape.
pub async fn answer(
    call: &ToolCall,
    offers: &[BotOffer],
    sender: &str,
    mail: &dyn BotMail,
) -> ToolResult {
    let shape = "call it as {\"to\": [\"<a Bot's name>\"], \"message\": \"<what to tell them>\"}";
    let to = call.arguments.get("to").and_then(Value::as_array);
    let message = call.arguments.get("message").and_then(Value::as_str);
    let (Some(to), Some(message)) = (to, message.map(str::trim)) else {
        return ToolResult::refused(&call.id, format!("bad arguments: {shape}"));
    };
    let asked: Vec<&str> = to.iter().filter_map(Value::as_str).map(str::trim).collect();
    if message.is_empty() || asked.is_empty() || asked.len() != to.len() {
        return ToolResult::refused(&call.id, format!("bad arguments: {shape}"));
    }
    // A name twice is one message: the schema says unique, and a model may not listen.
    let mut seen = std::collections::BTreeSet::new();
    let asked: Vec<&str> = asked
        .into_iter()
        .filter(|name| seen.insert(*name))
        .collect();
    if asked.len() > MAX_RECIPIENTS || message.chars().count() > MAX_MESSAGE_CHARS {
        let why = format!(
            "name at most {MAX_RECIPIENTS} Bots, and say it in at most {MAX_MESSAGE_CHARS} \
             characters; nothing was sent"
        );
        return ToolResult::refused(&call.id, why);
    }
    let (mut named, mut refused) = (Vec::new(), Vec::new());
    for asked in asked {
        match resolve(asked, offers, sender) {
            Ok(bot)
                if !named
                    .iter()
                    .any(|(_, b): &(&str, &BotOffer)| b.id == bot.id) =>
            {
                named.push((asked, bot));
            }
            Ok(_) => {}
            Err(why) => refused.push(json!({ "bot": asked, "why": why })),
        }
    }
    let bots: Vec<BotOffer> = named.iter().map(|(_, bot)| (*bot).clone()).collect();
    let mut delivered = Vec::new();
    if !bots.is_empty() {
        match mail.send(&call.id, &bots, message).await {
            Ok(written) => {
                for ((asked, _), row) in named.iter().zip(written) {
                    delivered.push(json!({ "bot": asked, "coworkerId": row.coworker_id,
                        "threadId": row.thread_id, "messageId": row.message_id }));
                }
            }
            Err(why) => {
                let each = named
                    .iter()
                    .map(|(asked, _)| json!({ "bot": asked, "why": why }));
                refused.extend(each);
            }
        }
    }
    let content = json!({ "delivered": delivered, "refused": refused }).to_string();
    ToolResult {
        ok: !delivered.is_empty(),
        ..ToolResult::ok(&call.id, content)
    }
}
