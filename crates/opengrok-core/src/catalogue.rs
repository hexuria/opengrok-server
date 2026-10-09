//! A model as a `/v1/models` listing describes it: the gateway's (opengrok-server `models.rs`), or
//! a person's own opencodex's, on this machine or their Mac (opengrok-harness `local_proxy`). ONE
//! PARSER FOR BOTH, so a picker's row and the check a write is held to read the same fields the
//! same way, whichever door lists the model.
//!
//! A MODEL'S OWN LEVELS, NEVER OURS. opencodex sends `reasoning_efforts: [{value, label,
//! default?}]`, `reasoning_effort` and `supports_reasoning_effort: true` on a model with levels and
//! none of the three on one without (2.75.0, `src/server/index/serve-options.ts`, and its
//! `/v1/models` on 127.0.0.1:8080, 3 Oct 2026); open-ai-gateway sends the same three, left out when
//! it does not know. Nothing is inferred from an id, a provider or another row: a stop the model
//! cannot take is a setting that silently does nothing.

use serde::Serialize;
use serde_json::{Value, json};

use crate::coworker::Effort;
use crate::inference::{SourceKind, Via};

/// One row of a listing. Unknown fields are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    /// `oag.context_window`: how many tokens the route reads. Null on virtual entries
    /// (`oag/auto`), whose model is chosen per request, and on every opencodex row.
    pub context_window: Option<u64>,
    /// `oag.alias_of`: the canonical id an `@sub`/`@api` entry is a channel of.
    pub alias_of: Option<String>,
    pub levels: Levels,
    /// `oag.provider`: the upstream the gateway serves the row from (`anthropic`, `xai`, an
    /// operator's endpoint, or `oag` for a virtual name). None on every opencodex row.
    pub provider: Option<String>,
    /// `oag.channel`: the kind of credential a pinned row is reached through, `api` or `sub`, and
    /// None where the row pins none.
    pub channel: Option<String>,
}

/// The levels a row publishes that a write takes (`Levels::of`), both `None` with none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Levels {
    /// `reasoning_efforts` without `default`, in the row's order, which is low to high.
    pub efforts: Option<Vec<Level>>,
    /// The model's own level: `reasoning_effort`, else the first entry marked `default: true`,
    /// each only if a write takes it (`Levels::of`).
    pub own: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Level {
    pub value: String,
    pub label: String,
}

/// The rows of an OpenAI-shaped `/v1/models` body. A body that is not what we expected yields
/// nothing rather than a guess.
pub fn models(body: &Value) -> Vec<Model> {
    let rows = body.get("data").and_then(Value::as_array).into_iter();
    let row = |row: &Value| {
        let alias_of = row.pointer("/oag/alias_of").and_then(Value::as_str);
        Some(Model {
            id: row.get("id")?.as_str()?.to_string(),
            context_window: row.pointer("/oag/context_window").and_then(Value::as_u64),
            alias_of: alias_of.map(str::to_string),
            levels: Levels::of(row),
            provider: word_at(row, "/oag/provider"),
            channel: word_at(row, "/oag/channel"),
        })
    };
    rows.flatten().filter_map(row).collect()
}

/// The same, from a body's text, which yields nothing when it is not JSON.
pub fn models_in(text: &str) -> Vec<Model> {
    let body: Result<Value, _> = serde_json::from_str(text);
    body.map(|body| models(&body)).unwrap_or_default()
}

impl Levels {
    /// What `row` publishes. `supports_reasoning_effort: false` is none, whatever else it says. An
    /// entry with no `value` is no level, and one with no `label` is shown as its value: NativeChat
    /// reads a list with either gap as no list at all (its `effort_levels_or_none`), which would
    /// take the slider from a model over one bad entry. Blank is no word.
    ///
    /// A LEVEL IS A WORD A WRITE TAKES, read trimmed (`is_level`): NativeChat sends a level's
    /// `value` back as the effort, so the gateway's `minimal`, which `Effort::parse` refuses, or
    /// `" high "`, would be a stop the slider offers and every save refuses. Such a word is left
    /// out, and what a write is held to (`refusal`) is the same. The own level is the named one if
    /// it is a level, else the first marked `default` that is, else null: a named `minimal` hid a
    /// marked `high` (review of #344), and the slider started nowhere though the row said where.
    pub fn of(row: &Value) -> Self {
        if row.get("supports_reasoning_effort") == Some(&Value::Bool(false)) {
            return Self::default();
        }
        let entries = row.get("reasoning_efforts").and_then(Value::as_array);
        let level = |entry: &Value| {
            let value = word(entry, "value")?;
            let label = word(entry, "label").unwrap_or(value).to_string();
            let marked = entry.get("default") == Some(&Value::Bool(true));
            let value = value.to_string();
            Some((marked, Level { value, label }))
        };
        let listed: Vec<_> = entries.into_iter().flatten().filter_map(level).collect();
        let marked = listed.iter().filter(|(marked, _)| *marked);
        let marked = marked.map(|(_, level)| level.value.as_str());
        let mut own = word(row, "reasoning_effort").into_iter().chain(marked);
        let own = own.find(|own| is_level(own)).map(str::to_string);
        let listed = listed.into_iter().map(|(_, level)| level);
        let efforts: Vec<Level> = listed.filter(|level| is_level(&level.value)).collect();
        let efforts = (!efforts.is_empty()).then_some(efforts);
        Self { efforts, own }
    }
}

/// The trimmed word at `pointer`, None when it is absent, not a string, or blank.
fn word_at(row: &Value, pointer: &str) -> Option<String> {
    let word = row.pointer(pointer)?.as_str()?.trim();
    (!word.is_empty()).then(|| word.to_string())
}

fn word<'a>(object: &'a Value, key: &str) -> Option<&'a str> {
    let word = object.get(key)?.as_str()?.trim();
    (!word.is_empty()).then_some(word)
}

/// One of a coworker's words but `inherit`, which asks for the model's own level rather than
/// naming one. `none` is a word a write takes, so a model that lists it may be switched off.
fn is_level(word: &str) -> bool {
    Effort::parse(word).is_some_and(|effort| effort != Effort::Inherit)
}

impl Model {
    /// This model as `GET /models` lists it, the wire agreed with NativeChat (3 Oct 2026; `provider` and `channel` 9 Oct 2026): its
    /// `points`, the door that lists it and, on a person's own plan, the way it is reached; and its
    /// `efforts` and `ownEffort`, both null when its listing publishes none.
    pub fn entry(&self, points: Value, source: SourceKind, via: Option<Via>) -> Value {
        let (efforts, own) = (&self.levels.efforts, &self.levels.own);
        let mut entry = json!({ "id": self.id, "points": points, "source": source.as_str(),
                                "efforts": efforts, "ownEffort": own });
        if let Some(via) = via {
            entry["via"] = Value::from(via.as_str());
        }
        // Where the gateway gets the model from, so a picker can group its rows by upstream and
        // credential (open-ai-gateway's `oag.provider` and `oag.channel`). Left out, not null,
        // where the listing names none, as the rows of a person's own plan never do.
        if let Some(provider) = &self.provider {
            entry["provider"] = Value::from(provider.as_str());
        }
        if let Some(channel) = &self.channel {
            entry["channel"] = Value::from(channel.as_str());
        }
        entry
    }

    /// As a person's own plan lists it, by `via`, with no points of its own.
    pub fn on_the_plan(&self, via: Via) -> Value {
        self.entry(Value::Null, SourceKind::LocalProxy, Some(via))
    }
}

/// Why `effort` is refused on `model`, held to the rows of `rows` with its id, one for each door
/// that lists it: `None` for `inherit`, which asks for the model's own level, for a model no such
/// row gives levels, and for a level any of them lists. The sentence names the levels they list.
/// ULTRA ALONE NEEDS A ROW THAT NAMES IT: no gateway reads the word (`Effort`), so with nothing
/// known it would be dropped on the way and the Bot would think at its route's default.
pub fn refusal(model: &str, effort: Effort, rows: &[Model]) -> Option<String> {
    let mut listed: Vec<&str> = Vec::new();
    let named = rows.iter().filter(|row| row.id == model);
    for level in named
        .filter_map(|row| row.levels.efforts.as_ref())
        .flatten()
    {
        if !listed.contains(&level.value.as_str()) {
            listed.push(&level.value);
        }
    }
    let word = effort.as_str();
    let known = !listed.is_empty() || effort == Effort::Ultra;
    if effort == Effort::Inherit || !known || listed.contains(&word) {
        return None;
    }
    let Some((last, rest)) = listed.split_last() else {
        return Some(format!(
            "no listing of {model} names \"{word}\", and it is taken only where one does"
        ));
    };
    let takes = match rest {
        [] => last.to_string(),
        rest => format!("{} or {last}", rest.join(", ")),
    };
    Some(format!("{model} takes {takes}, not \"{word}\""))
}
