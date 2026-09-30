use super::*;
use crate::model::ChatMessage;
use cred_swap_core::{Policy, Style};
use std::sync::Mutex;

/// A door that records what it was asked and replays a scripted stream.
struct ScriptedDoor {
    seen: Arc<Mutex<Vec<ModelRequest>>>,
    script: Arc<Mutex<Vec<ModelDelta>>>,
}

impl ScriptedDoor {
    fn wired(script: Vec<ModelDelta>) -> (Arc<dyn ModelDoor>, Arc<Mutex<Vec<ModelRequest>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let door = Arc::new(Self {
            seen: Arc::clone(&seen),
            script: Arc::new(Mutex::new(script)),
        });
        (door, seen)
    }
}

#[async_trait::async_trait]
impl ModelDoor for ScriptedDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        if let Ok(mut seen) = self.seen.lock() {
            seen.push(request);
        }
        let script = match self.script.lock() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            Err(_) => Vec::new(),
        };
        Ok(Box::pin(stream::iter(script.into_iter().map(Ok))))
    }
}

fn store() -> SessionStore {
    SessionStore::builder()
        .policy(Policy::default())
        .style(Style::Realistic)
        .secret(b"cloaked door tests")
        .build()
        .unwrap()
}

fn request(text: &str) -> ModelRequest {
    ModelRequest {
        model: "oag/cheap".to_string(),
        messages: vec![ChatMessage::text("user", text.to_string())],
        spend_scope: Some("cw_test".to_string()),
        spend_actor: Some("acct_test".to_string()),
        ..ModelRequest::default()
    }
}

async fn collect(stream: DeltaStream) -> Vec<ModelDelta> {
    stream
        .filter_map(|item| async move { item.ok() })
        .collect()
        .await
}

fn said(deltas: &[ModelDelta]) -> String {
    deltas
        .iter()
        .filter_map(|delta| match delta {
            ModelDelta::Text(words) => Some(words.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn the_provider_never_sees_the_real_values() {
    let (inner, seen) = ScriptedDoor::wired(Vec::new());
    let door = CloakedDoor::new(inner, store());

    drop(
        door.stream(request("ssh db.prod.internal, mail dana@corp.com"))
            .await
            .unwrap(),
    );

    let sent = seen.lock().unwrap();
    let content = &sent[0].messages[0].content;
    assert!(!content.contains("db.prod.internal"), "{content}");
    assert!(!content.contains("dana@corp.com"), "{content}");
    // Still a sentence about a host and an address, so the model can answer it.
    assert!(content.contains("ssh "), "{content}");
    assert!(content.contains('@'), "{content}");
}

#[tokio::test]
async fn the_system_prompt_is_scrubbed_but_the_tool_schema_is_not() {
    let (inner, seen) = ScriptedDoor::wired(Vec::new());
    let door = CloakedDoor::new(inner, store());

    let mut request = request("hello");
    request.system = Some("You maintain db.prod.internal.".to_string());
    request.tools = vec![serde_json::json!({
        "type": "function",
        "function": {"name": "ssh_run", "parameters": {"properties": {"host": {"type": "string"}}}}
    })];

    drop(door.stream(request).await.unwrap());

    let sent = seen.lock().unwrap();
    let system = sent[0].system.clone().unwrap();
    assert!(!system.contains("db.prod.internal"), "{system}");
    // Renaming a tool or its parameters would make the model ask for
    // something that does not exist.
    assert_eq!(sent[0].tools[0]["function"]["name"], "ssh_run");
    assert!(
        sent[0].tools[0]["function"]["parameters"]["properties"]["host"].is_object(),
        "the tool schema was rewritten"
    );
}

#[tokio::test]
async fn a_stand_in_split_across_deltas_is_restored() {
    // Learn the stand-in the way the door will produce it.
    let sessions = store();
    let stand_in = sessions
        .session("pin:cw_test:acct_test")
        .unwrap()
        .scrub("db.prod.internal")
        .unwrap()
        .replacements[0]
        .fake
        .clone();

    // The provider streams it one character at a time, the worst case.
    let mut script = vec![ModelDelta::Text("connect to ".to_string())];
    script.extend(stand_in.chars().map(|c| ModelDelta::Text(c.to_string())));
    script.push(ModelDelta::Text(" and retry".to_string()));

    let (inner, _) = ScriptedDoor::wired(script);
    let door = CloakedDoor::new(inner, sessions);

    let out = collect(
        door.stream(request("check db.prod.internal"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(said(&out), "connect to db.prod.internal and retry");
}

#[tokio::test]
async fn tool_arguments_are_restored_before_the_tool_would_run() {
    let sessions = store();
    let stand_in = sessions
        .session("pin:cw_test:acct_test")
        .unwrap()
        .scrub("db.prod.internal")
        .unwrap()
        .replacements[0]
        .fake
        .clone();

    // Arguments arrive as JSON split mid-value, which is why they are held
    // until the call closes.
    let half = stand_in.len() / 2;
    let script = vec![
        ModelDelta::ToolCallStart {
            id: "call_1".to_string(),
            name: "ssh_run".to_string(),
        },
        ModelDelta::ToolCallArgs {
            id: "call_1".to_string(),
            delta: format!("{{\"host\":\"{}", &stand_in[..half]),
        },
        ModelDelta::ToolCallArgs {
            id: "call_1".to_string(),
            delta: format!("{}\",\"command\":\"uptime\"}}", &stand_in[half..]),
        },
        ModelDelta::ToolCallEnd {
            id: "call_1".to_string(),
        },
    ];

    let (inner, _) = ScriptedDoor::wired(script);
    let door = CloakedDoor::new(inner, sessions);
    let out = collect(
        door.stream(request("check db.prod.internal"))
            .await
            .unwrap(),
    )
    .await;

    let arguments: String = out
        .iter()
        .filter_map(|delta| match delta {
            ModelDelta::ToolCallArgs { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();

    let parsed: serde_json::Value = serde_json::from_str(&arguments).unwrap();
    assert_eq!(
        parsed["host"], "db.prod.internal",
        "the coworker would have sshed to a host that does not exist"
    );
    assert_eq!(parsed["command"], "uptime");

    // The bracket order the projection depends on is preserved.
    assert!(matches!(
        out.first(),
        Some(ModelDelta::ToolCallStart { .. })
    ));
    assert!(matches!(out.last(), Some(ModelDelta::ToolCallEnd { .. })));
}

#[tokio::test]
async fn text_before_a_tool_call_is_flushed_in_order() {
    let (inner, _) = ScriptedDoor::wired(vec![
        ModelDelta::Text("I will check.".to_string()),
        ModelDelta::ToolCallStart {
            id: "call_1".to_string(),
            name: "ssh_run".to_string(),
        },
        ModelDelta::ToolCallEnd {
            id: "call_1".to_string(),
        },
    ]);
    let door = CloakedDoor::new(inner, store());
    let out = collect(door.stream(request("check it")).await.unwrap()).await;

    assert!(matches!(out.first(), Some(ModelDelta::Text(_))));
    assert_eq!(said(&out), "I will check.");
    assert_eq!(out.len(), 3);
}

#[tokio::test]
async fn a_tool_result_coming_back_round_is_scrubbed_too() {
    let (inner, seen) = ScriptedDoor::wired(Vec::new());
    let door = CloakedDoor::new(inner, store());

    // The shape the harness builds in `tool_result_message`.
    let mut request = request("check the logs");
    request.messages.push(ChatMessage::tool_result(
        "call_1",
        "url=postgresql://appuser:s3cr3t@db.prod.internal/orders",
    ));

    drop(door.stream(request).await.unwrap());

    let sent = seen.lock().unwrap();
    let result = &sent[0].messages[1];
    assert!(!result.content.contains("s3cr3t"), "{result:?}");
    assert!(!result.content.contains("db.prod.internal"), "{result:?}");
    assert!(result.content.starts_with("url="), "{result:?}");
    assert_eq!(result.tool_call_id.as_deref(), Some("call_1"));
}

/// The model's own call from an earlier round comes back in the next request with the real
/// values restored into it; it leaves scrubbed like every other message, or the cloak has a
/// hole the size of every command the coworker ever ran.
#[tokio::test]
async fn a_call_the_model_made_leaves_scrubbed_too() {
    let (inner, seen) = ScriptedDoor::wired(Vec::new());
    let door = CloakedDoor::new(inner, store());

    let mut request = request("check the logs");
    request.messages.push(ChatMessage::calls(
        "",
        vec![crate::model::ToolCallRef {
            id: "call_1".to_string(),
            name: "shell".to_string(),
            arguments: r#"{"command":"psql postgresql://appuser:s3cr3t@db.prod.internal/orders"}"#
                .to_string(),
        }],
    ));

    drop(door.stream(request).await.unwrap());

    let sent = seen.lock().unwrap();
    let call = &sent[0].messages[1].tool_calls[0];
    assert!(!call.arguments.contains("s3cr3t"), "{call:?}");
    assert!(!call.arguments.contains("db.prod.internal"), "{call:?}");
    assert_eq!((call.id.as_str(), call.name.as_str()), ("call_1", "shell"));
}

#[tokio::test]
async fn a_conversation_keeps_one_stand_in_across_rounds() {
    let sessions = store();
    let (inner, seen) = ScriptedDoor::wired(Vec::new());
    let door = CloakedDoor::new(inner, sessions);

    drop(door.stream(request("mail dana@corp.com")).await.unwrap());
    drop(
        door.stream(request("remind dana@corp.com again"))
            .await
            .unwrap(),
    );

    let sent = seen.lock().unwrap();
    let first = sent[0].messages[0].content.replace("mail ", "");
    assert!(
        sent[1].messages[0].content.contains(&first),
        "the second round gave the same person a different stand-in: {:?}",
        *sent
    );
}

#[tokio::test]
async fn two_coworkers_do_not_share_stand_ins() {
    let sessions = store();
    let (inner, seen) = ScriptedDoor::wired(Vec::new());
    let door = CloakedDoor::new(inner, sessions);

    let mut theirs = request("mail dana@corp.com");
    theirs.spend_scope = Some("cw_other".to_string());
    theirs.spend_actor = Some("acct_other".to_string());

    drop(door.stream(request("mail dana@corp.com")).await.unwrap());
    drop(door.stream(theirs).await.unwrap());

    let sent = seen.lock().unwrap();
    assert_ne!(
        sent[0].messages[0].content, sent[1].messages[0].content,
        "one coworker's stand-in identified the value in another's transcript"
    );
}

#[tokio::test]
async fn an_upstream_error_does_not_swallow_the_text_before_it() {
    struct Failing;

    #[async_trait::async_trait]
    impl ModelDoor for Failing {
        async fn stream(&self, _: ModelRequest) -> Result<DeltaStream, ModelError> {
            Ok(Box::pin(stream::iter(vec![
                Ok(ModelDelta::Text("partial answer".to_string())),
                Err(ModelError::Stream("upstream died".to_string())),
            ])))
        }
    }

    let door = CloakedDoor::new(Arc::new(Failing), store());
    let mut stream = door.stream(request("hello")).await.unwrap();

    let mut words = String::new();
    let mut saw_error = false;
    while let Some(item) = stream.next().await {
        match item {
            Ok(ModelDelta::Text(said)) => words.push_str(&said),
            Err(_) => saw_error = true,
            Ok(_) => {}
        }
    }
    assert_eq!(words, "partial answer");
    assert!(saw_error, "the error was lost");
}

#[tokio::test]
async fn text_with_nothing_sensitive_passes_through_unchanged() {
    let (inner, seen) = ScriptedDoor::wired(vec![ModelDelta::Text(
        "Use a BTreeMap so the order is stable.".to_string(),
    )]);
    let door = CloakedDoor::new(inner, store());

    let asked = "Refactor the parser to stop allocating per token.";
    let out = collect(door.stream(request(asked)).await.unwrap()).await;

    assert_eq!(seen.lock().unwrap()[0].messages[0].content, asked);
    assert_eq!(said(&out), "Use a BTreeMap so the order is stable.");
}

#[test]
fn the_conversation_key_follows_the_spend_pin() {
    let mut request = request("hello");
    assert_eq!(default_key(&request), "pin:cw_test:acct_test");

    // Without a pin, the opening message separates conversations.
    request.spend_scope = None;
    request.spend_actor = None;
    let mine = default_key(&request);
    assert!(mine.starts_with("open:"), "{mine}");

    let mut other = request.clone();
    other.messages[0].content = "a different opening".to_string();
    assert_ne!(mine, default_key(&other));

    // And it is stable as the conversation grows.
    request
        .messages
        .push(ChatMessage::text("assistant", "sure"));
    assert_eq!(mine, default_key(&request));
}
