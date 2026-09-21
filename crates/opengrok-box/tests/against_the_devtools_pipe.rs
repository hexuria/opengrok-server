//! The DevTools pipe against a real box: Chromium answers on the pipe, a page can be
//! attached, a platform authenticator with a loaded key comes and goes, and nothing listens
//! on the box's DevTools port.
//!
//! Needs a running box made from `grok-box:local` with no Chromium of its own
//! (`BOX_CHROME=0`); set `OG_BOX_PIPE_TEST` to its container name. Skips loudly otherwise.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::time::Duration;

use opengrok_box::devtools::{DevTools, Passkey};

/// A P-256 private key in PKCS#8, base64 — a test key, made for this test and nothing else.
const TEST_KEY_B64: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgElnbPGAaHzFSffkTGToWNxzpJ1PoC78RVyfEXCPwDkWhRANCAAQc6RyiU4Kr4XvZG72jVZTcKBOWNPAYAKr0Wc0LdRPgkFxcxn6NtGXq+5KwjxOrBlcLz5N6GUQRYsrASDfUi0AZ";

#[tokio::test]
async fn chromium_answers_on_the_pipe_and_holds_a_passkey_for_one_sign_in() {
    let Ok(container) = std::env::var("OG_BOX_PIPE_TEST") else {
        eprintln!("skipping: OG_BOX_PIPE_TEST is not set");
        return;
    };
    let devtools = DevTools::spawn(&container, "about:blank")
        .await
        .expect("spawn chromium on the pipe");
    let urls = devtools.page_urls().await.expect("pages");
    assert!(urls.iter().any(|u| u == "about:blank"), "{urls:?}");

    // Nothing listens on the port the old model used.
    let port = std::process::Command::new("docker")
        .args([
            "exec",
            &container,
            "sh",
            "-c",
            "cat /proc/net/tcp /proc/net/tcp6 2>/dev/null | grep -ci ':2406 ' || true",
        ])
        .output()
        .expect("docker exec");
    assert_eq!(
        String::from_utf8_lossy(&port.stdout).trim(),
        "0",
        "9222 (0x2406) is not open"
    );

    let session = devtools
        .attach_to_page(Some("about:blank"))
        .await
        .expect("attach");
    let authenticator = devtools
        .add_platform_authenticator(&session)
        .await
        .expect("authenticator");
    let passkey = Passkey {
        rp_id: "webauthn.io".to_string(),
        credential_id_b64: "AQIDBAUGBwgJCgsMDQ4PEA==".to_string(),
        user_handle_b64: "dXNlci0x".to_string(),
        private_key_b64: TEST_KEY_B64.to_string(),
        user_name: "ada".to_string(),
    };
    devtools
        .add_credential(&session, &authenticator, &passkey)
        .await
        .expect("add credential");
    let held = devtools
        .credentials(&session, &authenticator)
        .await
        .expect("credentials");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["rpId"], "webauthn.io");
    devtools
        .remove_credential(&session, &authenticator, &passkey.credential_id_b64)
        .await
        .expect("remove credential");
    let held = devtools
        .credentials(&session, &authenticator)
        .await
        .expect("credentials");
    assert!(held.is_empty(), "the key is gone: {held:?}");
    devtools
        .remove_authenticator(&session, &authenticator)
        .await
        .expect("remove authenticator");

    // A tab opened by the dock launcher lands in this instance.
    let opened = std::process::Command::new("docker")
        .args([
            "exec",
            "-d",
            &container,
            "box-chromium",
            "https://example.com/",
        ])
        .status()
        .expect("box-chromium");
    assert!(opened.success());
    let mut seen = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if devtools
            .page_urls()
            .await
            .expect("pages")
            .iter()
            .any(|u| u.contains("example.com"))
        {
            seen = true;
            break;
        }
    }
    assert!(seen, "the dock's tab joined the piped instance");
    drop(devtools);
}
