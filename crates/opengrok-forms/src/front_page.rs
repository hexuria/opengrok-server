//! Whether a fill may type into the page in front of the computer's browser.
//!
//! A fill clicks where the model said the fields are, or types into the focused field: it
//! types into whatever page is in front. On a computer several Bots share, another Bot may
//! have brought its own page forward since the card was raised (9 Oct 2026: one Bot opened
//! YouTube, and every Bot's screen showed YouTube). The box reports the front page
//! (hexuria/box `GET /v1/chrome/active-tab`); a fill whose card names a site goes ahead only
//! when that page is on it.

use opengrok_box::devtools::url_is_on_host;

use crate::user_form::FormRequest;

/// The wire word NativeChat reads when a fill was refused for the page in front.
pub const WRONG_PAGE: &str = "wrong-page";

/// Why nothing may be typed now, in the person's words; `None` when the fill may go ahead.
/// `front` is the page in front as the box reported it, `None` when it could not say.
/// `unknown_refuses`: a box that cannot say is refused too (a saved login on a shared
/// computer, where a wrong page is likely); otherwise it goes ahead as fills always did.
#[must_use]
pub fn front_page_refusal(
    form: &FormRequest,
    front: Option<&str>,
    unknown_refuses: bool,
) -> Option<String> {
    let site = form
        .live_host
        .as_deref()
        .or(form.domain.as_deref())
        .map(host_of)
        .filter(|h| !h.is_empty());
    // A card that names no site has nothing to check the page against: as good as a box that
    // cannot say.
    let Some(site) = site else {
        return unknown_refuses.then(|| {
            "This card doesn't say which site it is for, and this computer is shared with your \
             other Bots, so nothing was typed. Type the login by hand."
                .to_string()
        });
    };
    // `www.` is a page of the site, not the site: a card raised on www.facebook.com fills the
    // login on facebook.com and m.facebook.com as well.
    let site = site.strip_prefix("www.").unwrap_or(&site).to_string();
    match front {
        Some(url) if url_is_on_host(url, &site) => None,
        Some(url) => Some(format!(
            "The page in front is {}, not {site}, so nothing was typed. Bring {site} to the \
             front and try again.",
            shown(url)
        )),
        None if unknown_refuses => Some(format!(
            "This computer can't say which page is in front, and it is shared with your other \
             Bots, so nothing was typed. Type the login by hand on {site}."
        )),
        None => None,
    }
}

/// `https://Accounts.Google.com:443/x` → `accounts.google.com`.
fn host_of(raw: &str) -> String {
    let rest = raw.trim().split_once("://").map_or(raw.trim(), |(_, r)| r);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let host = host.split(':').next().unwrap_or(host);
    host.trim_matches(['[', ']']).to_ascii_lowercase()
}

/// A page as the person would name it: its host, or "a blank page".
fn shown(url: &str) -> String {
    let host = host_of(url);
    if host.is_empty() || !url.contains("://") {
        "a blank page".to_string()
    } else {
        host
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(host: &str) -> FormRequest {
        crate::user_form::form_request_from(&serde_json::json!({
            "title": "Log in", "liveHost": host,
            "fields": [{"id": "email", "label": "Email"}]
        }))
    }

    #[test]
    fn a_fill_types_only_into_the_cards_own_site() {
        let fb = card("www.facebook.com");
        assert_eq!(
            front_page_refusal(&fb, Some("https://www.facebook.com/login"), true),
            None
        );
        assert_eq!(
            front_page_refusal(&fb, Some("https://m.facebook.com/"), true),
            None
        );
        // Another Bot's page is refused, whatever the computer.
        let youtube = front_page_refusal(&fb, Some("https://www.youtube.com/"), false);
        assert!(
            youtube
                .as_deref()
                .is_some_and(|why| why.contains("www.youtube.com")),
            "{youtube:?}"
        );
        // A look-alike is not the site.
        assert!(
            front_page_refusal(&fb, Some("https://facebook.com.evil.example/"), false).is_some()
        );
        assert!(front_page_refusal(&fb, Some("chrome://newtab/"), false).is_some());
    }

    #[test]
    fn a_box_that_cannot_say_refuses_only_where_asked() {
        let fb = card("www.facebook.com");
        assert!(front_page_refusal(&fb, None, true).is_some());
        assert_eq!(front_page_refusal(&fb, None, false), None);
        // A card that names no site has nothing to check against: refused where asked.
        let bare = crate::user_form::form_request_from(&serde_json::json!({"title": "Code"}));
        assert!(front_page_refusal(&bare, Some("https://www.youtube.com/"), true).is_some());
        assert_eq!(
            front_page_refusal(&bare, Some("https://www.youtube.com/"), false),
            None
        );
    }
}
