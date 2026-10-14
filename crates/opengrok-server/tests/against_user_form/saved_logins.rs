//! Saved logins: typed only on the site and box they belong to, by the bots they're shared with.

use super::*;

/// A saved login is the person's own. On a box the account shares (the default sharing
/// mode), the submit that carries it is refused and the card stays open; once the bot has
/// its own box, the same submit fills. A hand-typed submit was never subject to the rule.
#[tokio::test]
async fn a_saved_login_fills_only_a_dedicated_box() {
    let database_url = database_or_skip!();
    let email = format!("user-form-saved-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;

    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let entry_id = card["id"].as_str().expect("entry id").to_string();
    let submit = json!({
        "entryId": entry_id,
        "agentId": agent,
        "savedLogin": true,
        "values": { "email": EMAIL, "password": SECRET }
    });

    let (status, body) = h
        .agui(&token, "/ag-ui/user-form/submit", submit.clone())
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"], "shared-computer", "{body}");
    assert!(
        h.stub.acts.lock().expect("acts").is_empty(),
        "nothing was typed into the shared box"
    );
    let still_open = h.wait_for_form(&agent).await;
    assert!(
        still_open.get("formResolution").is_none()
            && still_open["message"].get("formResolution").is_none(),
        "the card is still open: {still_open}"
    );

    // A bot hired once the account gives each bot its own computer fills from the same card.
    h.store
        .set_sharing_mode("account", h.account.as_str(), "per-bot", 1)
        .await
        .expect("per-bot");
    let own = h.hire(&token, "Bea").await;
    // NativeChat names the saved login it fills from; this one is shared with Bea.
    let login = format!("sl_{}", uuid::Uuid::now_v7().simple());
    seed_login(&h.store, &h.account, &login, "password", Some(own.as_str())).await;
    h.turn(&token, &own, "sign in").await;
    let card = h.wait_for_form(&own).await;
    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({
                "entryId": card["id"].as_str().expect("entry id"),
                "agentId": own,
                "savedLogin": true,
                "savedLoginId": login,
                "values": { "email": EMAIL, "password": SECRET }
            }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["formResolution"], "submitted", "{body}");
    assert!(
        !h.stub.acts.lock().expect("acts").is_empty(),
        "the dedicated box was typed into"
    );
    // A login that came from the vault is not offered to the vault again, and nothing of it
    // is shared back to the model.
    assert!(
        body.get("sharedValues")
            .and_then(Value::as_object)
            .is_none_or(|shared| shared.is_empty()),
        "a saved login shares nothing: {body}"
    );
    let replay = h
        .client
        .get(format!("{}/ag-ui/threads/gateway-{own}", h.base))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("replay thread");
    let replayed: Value = replay.json().await.expect("replay json");
    let offered = replayed["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|run| run["events"].as_array())
        .flatten()
        .any(|event| event["name"] == "credential.offer_save");
    assert!(!offered, "no save offer after a vault fill: {replayed}");

    // A bot the person shares with their org is driven by everyone in it: its box never gets
    // a saved login, even though the box is the bot's own. Marked `org` in the store rather
    // than through PATCH, because this person is in no org and the route now refuses to share
    // with nobody; the rule keys on the word, and must hold for a bot that still carries it
    // after its owner left their org.
    let shown = h.hire(&token, "Dee").await;
    mark_org_visible(&h.store, &h.account, &shown).await;
    h.turn(&token, &shown, "sign in").await;
    let card = h.wait_for_form(&shown).await;
    let acts_before = h.stub.acts.lock().expect("acts").len();
    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({
                "entryId": card["id"].as_str().expect("entry id"),
                "agentId": shown,
                "savedLogin": true,
                "values": { "email": EMAIL, "password": SECRET }
            }),
        )
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"], "shared-computer", "{body}");
    assert_eq!(
        h.stub.acts.lock().expect("acts").len(),
        acts_before,
        "nothing typed into an org-visible bot's box"
    );
}

/// The app asks before Touch ID whether a saved login can be used for this Bot at all, so the
/// person is never asked for their fingerprint only to be refused; and one Bot can be given its
/// own computer while the account's others keep sharing, after which the same card fills
/// (8 Oct 2026: the refusal came after Touch ID, with no way forward).
#[tokio::test]
async fn a_saved_login_is_checked_before_touch_id_and_one_bot_can_get_its_own_computer() {
    let database_url = database_or_skip!();
    let email = format!("user-form-own-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;
    let other = h.hire(&token, "Bo").await;
    let call = |method: reqwest::Method, path: String, body: Option<Value>| {
        let mut req = h
            .client
            .request(method, format!("{}{path}", h.base))
            .header("authorization", format!("Bearer {token}"));
        if let Some(body) = body {
            req = req.json(&body);
        }
        async move {
            let res = req.send().await.expect("call");
            let status = res.status().as_u16();
            (status, res.json::<Value>().await.unwrap_or(Value::Null))
        }
    };
    let check = |bot: String| {
        call(
            reqwest::Method::GET,
            format!("/coworkers/{bot}/saved-login"),
            None,
        )
    };

    let (status, body) = check(agent.clone()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["usable"], false, "{body}");
    assert_eq!(body["reason"], "shared-computer", "{body}");
    assert_eq!(body["ownComputer"], false, "{body}");

    let (status, body) = call(
        reqwest::Method::PUT,
        format!("/coworkers/{agent}/own-computer"),
        Some(json!({ "on": true })),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (_, body) = check(agent.clone()).await;
    assert_eq!(body["usable"], true, "{body}");
    assert_eq!(body["ownComputer"], true, "{body}");
    let (_, body) = check(other.clone()).await;
    assert_eq!(
        body["usable"], false,
        "the account's other Bots still share: {body}"
    );

    // NativeChat names the saved login it fills from; this one is shared with Ada.
    let login = format!("sl_{}", uuid::Uuid::now_v7().simple());
    seed_login(
        &h.store,
        &h.account,
        &login,
        "password",
        Some(agent.as_str()),
    )
    .await;
    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let (status, body) = h
        .agui(
            &token,
            "/ag-ui/user-form/submit",
            json!({
                "entryId": card["id"].as_str().expect("entry id"),
                "agentId": agent,
                "savedLogin": true,
                "savedLoginId": login,
                "values": { "email": EMAIL, "password": SECRET }
            }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["formResolution"], "submitted", "{body}");

    // An org-visible Bot is driven by everyone in it: never usable, whatever its computer.
    mark_org_visible(&h.store, &h.account, &agent).await;
    let (_, body) = check(agent.clone()).await;
    assert_eq!(body["usable"], false, "{body}");
    assert_eq!(body["reason"], "shared-bot", "{body}");
}

/// A saved login fills only for a Bot it is shared with: a Bot starts with none, the share is
/// one switch the person sets, and taking it back stops the next fill (8 Oct 2026).
#[tokio::test]
async fn a_saved_login_fills_only_for_a_bot_it_is_shared_with() {
    let database_url = database_or_skip!();
    let email = format!("user-form-share-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    h.store
        .set_sharing_mode("account", h.account.as_str(), "per-bot", 1)
        .await
        .expect("per-bot");
    let agent = h.hire(&token, "Ada").await;
    let call = |method: reqwest::Method, path: String, body: Option<Value>| {
        let mut req = h
            .client
            .request(method, format!("{}{path}", h.base))
            .header("authorization", format!("Bearer {token}"));
        if let Some(body) = body {
            req = req.json(&body);
        }
        async move {
            let res = req.send().await.expect("call");
            let status = res.status().as_u16();
            (status, res.json::<Value>().await.unwrap_or(Value::Null))
        }
    };
    let login = format!("sl_{}", uuid::Uuid::now_v7().simple());
    seed_login(&h.store, &h.account, &login, "password", None).await;
    let (_, shared) = call(
        reqwest::Method::GET,
        format!("/coworkers/{agent}/site-logins"),
        None,
    )
    .await;
    assert_eq!(
        shared["logins"],
        json!([]),
        "a Bot starts with no logins: {shared}"
    );

    let fill = |card: Value| {
        json!({
            "entryId": card["id"].as_str().expect("entry id"),
            "agentId": agent,
            "savedLogin": true,
            "savedLoginId": login,
            "values": { "email": EMAIL, "password": SECRET }
        })
    };
    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let (status, body) = h
        .agui(&token, "/ag-ui/user-form/submit", fill(card.clone()))
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"], "not-shared", "{body}");
    assert!(
        h.stub.acts.lock().expect("acts").is_empty(),
        "nothing typed"
    );

    let share = |on: bool| {
        call(
            reqwest::Method::PUT,
            format!("/site-logins/{login}/bots/{agent}"),
            Some(json!({ "shared": on })),
        )
    };
    // Shared and taken back: the still-open card's fill is refused again.
    assert_eq!(share(true).await.0, 200);
    assert_eq!(share(false).await.0, 200);
    let (status, body) = h
        .agui(&token, "/ag-ui/user-form/submit", fill(card.clone()))
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"], "not-shared", "{body}");

    let (status, body) = share(true).await;
    assert_eq!(status, 200, "{body}");
    let (_, bots) = call(
        reqwest::Method::GET,
        format!("/site-logins/{login}/bots"),
        None,
    )
    .await;
    assert_eq!(bots["bots"], json!([agent]), "{bots}");
    let (status, body) = h.agui(&token, "/ag-ui/user-form/submit", fill(card)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["formResolution"], "submitted", "{body}");

    // Another account's Bot is not one to share with.
    let (status, _) = call(
        reqwest::Method::PUT,
        format!("/site-logins/{login}/bots/cw_not_theirs"),
        Some(json!({ "shared": true })),
    )
    .await;
    assert_eq!(status, 404);
}

/// With the person's "share my logins with all my Bots" switch on, a saved login fills on the
/// computer their own Bots share, for a Bot it was never shared with one by one: no Bot needs a
/// fresh computer (and loses the page it had open) to sign in. Off, it is refused as before,
/// and a Bot shown to an org is refused either way (9 Oct 2026).
#[tokio::test]
async fn sharing_logins_with_all_bots_fills_on_the_computer_they_share() {
    let database_url = database_or_skip!();
    let email = format!("user-form-all-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness_with_door(
        &database_url,
        &email,
        Arc::new(SiteLoginDoor("www.facebook.com")),
    )
    .await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;
    let call = |method: reqwest::Method, path: String, body: Option<Value>| {
        let mut req = h
            .client
            .request(method, format!("{}{path}", h.base))
            .header("authorization", format!("Bearer {token}"));
        if let Some(body) = body {
            req = req.json(&body);
        }
        async move {
            let res = req.send().await.expect("call");
            let status = res.status().as_u16();
            (status, res.json::<Value>().await.unwrap_or(Value::Null))
        }
    };
    let check = || {
        call(
            reqwest::Method::GET,
            format!("/coworkers/{agent}/saved-login"),
            None,
        )
    };
    let all_bots = |on: bool| {
        call(
            reqwest::Method::PUT,
            "/site-logins/sharing".to_string(),
            Some(json!({ "allBots": on })),
        )
    };
    let login = format!("sl_{}", uuid::Uuid::now_v7().simple());
    let origin = "https://www.facebook.com";
    seed_login_at(&h.store, &h.account, &login, "password", None, origin).await;

    let (_, body) = call(reqwest::Method::GET, "/site-logins/sharing".into(), None).await;
    assert_eq!(
        body["allBots"], false,
        "off until the person turns it on: {body}"
    );
    let (_, body) = check().await;
    assert_eq!(body["reason"], "shared-computer", "{body}");
    assert_eq!(
        body["ownBotsOnly"], true,
        "the switch would help here: {body}"
    );

    let (status, body) = all_bots(true).await;
    assert_eq!(status, 200, "{body}");
    let (_, body) = check().await;
    assert_eq!(body["usable"], true, "{body}");
    assert_eq!(
        body["ownComputer"], false,
        "no computer of its own was made: {body}"
    );
    let (_, shared) = call(
        reqwest::Method::GET,
        format!("/coworkers/{agent}/site-logins"),
        None,
    )
    .await;
    assert_eq!(shared["logins"], json!([login]), "{shared}");

    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let fill = json!({
        "entryId": card["id"].as_str().expect("entry id"),
        "agentId": agent,
        "savedLogin": true,
        "savedLoginId": login,
        "values": { "email": EMAIL, "password": SECRET }
    });
    // Another Bot on this computer brought its own page forward: nothing is typed into it.
    *h.stub.front.lock().expect("front") = Some("https://www.youtube.com/".into());
    let (status, body) = h
        .agui(&token, "/ag-ui/user-form/submit", fill.clone())
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"], "wrong-page", "{body}");
    assert!(
        h.stub.acts().is_empty(),
        "nothing typed into another Bot's page"
    );
    // A box that cannot say which page is in front is refused too, on a shared computer.
    *h.stub.front.lock().expect("front") = None;
    let (status, body) = h
        .agui(&token, "/ag-ui/user-form/submit", fill.clone())
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"], "wrong-page", "{body}");
    assert!(h.stub.acts().is_empty(), "nothing typed blind");
    // No browser page open: the keys would go to whatever else has focus.
    *h.stub.front.lock().expect("front") = Some(String::new());
    let (status, body) = h
        .agui(&token, "/ag-ui/user-form/submit", fill.clone())
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"], "wrong-page", "{body}");
    assert!(h.stub.acts().is_empty(), "nothing typed into a terminal");
    // A saved login fill that does not name its login is refused: its site cannot be read.
    let mut unnamed = fill.clone();
    unnamed
        .as_object_mut()
        .expect("a body")
        .remove("savedLoginId");
    *h.stub.front.lock().expect("front") = Some("https://www.facebook.com/login".into());
    let (status, body) = h.agui(&token, "/ag-ui/user-form/submit", unnamed).await;
    assert_eq!(status, 400, "{body}");
    assert!(
        h.stub.acts().is_empty(),
        "nothing typed for an unnamed login"
    );
    // The login's own site in front: the card, still open, fills.
    *h.stub.front.lock().expect("front") = Some("https://www.facebook.com/login".into());
    let (status, body) = h.agui(&token, "/ag-ui/user-form/submit", fill).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["formResolution"], "submitted", "{body}");
    assert!(!h.stub.acts().is_empty(), "typed into the login's own page");

    // Off again: back to the shares made one Bot at a time, of which this Bot has none.
    assert_eq!(all_bots(false).await.0, 200);
    let (_, body) = check().await;
    assert_eq!(body["reason"], "shared-computer", "{body}");

    // A Bot everyone in an org drives is never one a saved login fills for.
    assert_eq!(all_bots(true).await.0, 200);
    mark_org_visible(&h.store, &h.account, &agent).await;
    let (_, body) = check().await;
    assert_eq!(body["reason"], "shared-bot", "{body}");
}

/// A saved login is typed only on the site it was saved for. The card's `liveHost` is the
/// model's word: a card for a Facebook login that names a look-alike page, with that page in
/// front, used to pass the check and get the person's Facebook password typed into it.
#[tokio::test]
async fn a_saved_login_is_typed_only_on_the_site_it_was_saved_for() {
    let database_url = database_or_skip!();
    let email = format!("user-form-site-{}@og.local", uuid::Uuid::now_v7().simple());
    let door = Arc::new(SiteLoginDoor("fb.evil.example"));
    let h = harness_with_door(&database_url, &email, door).await;
    let token = h.access_token(&email);
    h.store
        .set_sharing_mode("account", h.account.as_str(), "per-bot", 1)
        .await
        .expect("per-bot");
    let agent = h.hire(&token, "Ada").await;
    let login = format!("sl_{}", uuid::Uuid::now_v7().simple());
    let origin = "https://www.facebook.com";
    seed_login_at(
        &h.store,
        &h.account,
        &login,
        "password",
        Some(agent.as_str()),
        origin,
    )
    .await;
    h.turn(&token, &agent, "sign in").await;
    let card = h.wait_for_form(&agent).await;
    let fill = json!({
        "entryId": card["id"].as_str().expect("entry id"),
        "agentId": agent,
        "savedLogin": true,
        "savedLoginId": login,
        "values": { "email": EMAIL, "password": SECRET }
    });

    *h.stub.front.lock().expect("front") = Some("https://fb.evil.example/login".into());
    let (status, body) = h
        .agui(&token, "/ag-ui/user-form/submit", fill.clone())
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"], "wrong-page", "{body}");
    assert!(
        h.stub.acts().is_empty(),
        "nothing typed into the look-alike"
    );

    // No browser page open, on the Bot's own computer: the keys would go to a terminal.
    *h.stub.front.lock().expect("front") = Some(String::new());
    let (status, body) = h
        .agui(&token, "/ag-ui/user-form/submit", fill.clone())
        .await;
    assert_eq!(status, 403, "{body}");
    assert!(h.stub.acts().is_empty(), "nothing typed with no page open");

    // The login's own site in front: it fills, whatever host the card named.
    *h.stub.front.lock().expect("front") = Some("https://www.facebook.com/login".into());
    let (status, body) = h.agui(&token, "/ag-ui/user-form/submit", fill).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["formResolution"], "submitted", "{body}");
}

/// Bots that share a computer each work on a screen of their own, with no switch to flip (#376,
/// the owner's call on 11 Oct 2026): one Bot's fill lands on its screen and the other's on its
/// own, each Bot's computer says so, and each Bot's screen is started as its turn begins.
#[tokio::test]
async fn bots_that_share_a_computer_each_work_on_a_screen_of_their_own() {
    let database_url = database_or_skip!();
    let email = format!(
        "user-form-screen-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let door = Arc::new(SiteLoginDoor("www.facebook.com"));
    let h = harness_with_door(&database_url, &email, door).await;
    let token = h.access_token(&email);
    let (ada, bea) = (h.hire(&token, "Ada").await, h.hire(&token, "Bea").await);
    let status_of = |bot: String| {
        let req = h
            .client
            .get(format!("{}/coworkers/{bot}/computer", h.base))
            .header("authorization", format!("Bearer {token}"));
        async move {
            req.send()
                .await
                .expect("call")
                .json::<Value>()
                .await
                .expect("json")
        }
    };

    // Each Bot fills a card by hand on the page in front; the actions say whose screen.
    *h.stub.front.lock().expect("front") = Some("https://www.facebook.com/login".into());
    for bot in [&bea, &ada] {
        h.turn(&token, bot, "sign in").await;
        let card = h.wait_for_form(bot).await;
        let fill = json!({
            "entryId": card["id"].as_str().expect("entry id"),
            "agentId": bot,
            "values": { "email": EMAIL, "password": "typed-by-hand" }
        });
        let (status, body) = h.agui(&token, "/ag-ui/user-form/submit", fill).await;
        assert_eq!(status, 200, "{body}");
    }
    let screens = h.stub.screens.lock().expect("screens").clone();
    let on_bea = screens
        .iter()
        .take_while(|s| **s == opengrok_box::Screen::Own(bea.to_string()))
        .count();
    assert!(on_bea > 0, "Bea's fill is on her own screen: {screens:?}");
    assert!(
        on_bea < screens.len()
            && screens[on_bea..]
                .iter()
                .all(|s| *s == opengrok_box::Screen::Own(ada.to_string())),
        "Ada's fill is on her own screen, not Bea's or a shared one: {screens:?}"
    );
    for bot in [&ada, &bea] {
        let body = status_of(bot.to_string()).await;
        assert_eq!(body["screen"], "own", "{body}");
    }
    // Each Bot's screen was started as its turn began, not on its first action.
    let opened = h.stub.opened.lock().expect("opened").clone();
    for bot in [&ada, &bea] {
        assert!(
            opened.contains(&opengrok_box::Screen::Own(bot.to_string())),
            "{bot}'s screen is started ahead of its turn: {opened:?}"
        );
    }
}

/// A BOT CAN LOOK AT ANOTHER BOT'S SCREEN (11 Oct 2026): with every Bot on its own screen, Ada
/// asks to see Bea's, by her name, and is shown a picture taken of Bea's own screen, said to be
/// look-only. A name none of the person's Bots has is refused with the names they do have.
#[tokio::test]
async fn a_bot_looks_at_another_bots_screen_by_its_name() {
    let database_url = database_or_skip!();
    let email = format!("look-screen-{}@og.local", uuid::Uuid::now_v7().simple());
    let door = Arc::new(LookDoor);
    let h = harness_with_door(&database_url, &email, door).await;
    let token = h.access_token(&email);
    let (ada, bea) = (h.hire(&token, "Ada").await, h.hire(&token, "Bea").await);

    let sse = h.turn(&token, &ada, "look at bea").await;
    let shots = h.stub.shot_screens.lock().expect("shot screens").clone();
    assert_eq!(
        shots.last(),
        Some(&opengrok_box::Screen::Own(bea.to_string())),
        "the picture is of Bea's own screen: {shots:?}"
    );
    assert!(sse.contains("cannot click or type there"), "{sse}");
    assert!(
        sse.contains("iVBORw0KGgo="),
        "the picture rides the result: {sse}"
    );

    let before = h.stub.shot_screens.lock().expect("shot screens").len();
    let sse = h.turn_on(&token, &ada, "look-zed", "look at zed").await;
    assert!(sse.contains("no Bot called Zed"), "{sse}");
    assert!(sse.contains("Ada") && sse.contains("Bea"), "{sse}");
    assert_eq!(
        h.stub.shot_screens.lock().expect("shot screens").len(),
        before,
        "no picture is taken for a Bot that is not there"
    );
}
