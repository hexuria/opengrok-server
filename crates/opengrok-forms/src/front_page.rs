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

/// The page in front of the computer's browser, as the box answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Front<'a> {
    /// This page is in front.
    Page(&'a str),
    /// The box answered that no browser page is open: a fill would type into whatever else has
    /// focus, a terminal or a file manager.
    NoBrowser,
    /// The box cannot say: no screen, or an image from before the report.
    Unknown,
}

impl<'a> Front<'a> {
    /// The box's answer (`active_tab_url`): a page, `None` for no browser page open, and an
    /// error for a box that cannot say.
    #[must_use]
    pub fn of<E>(answer: &'a Result<Option<String>, E>) -> Self {
        match answer {
            Ok(Some(url)) => Front::Page(url),
            Ok(None) => Front::NoBrowser,
            Err(_) => Front::Unknown,
        }
    }
}

/// A saved-login fill that does not name its login: neither its shares nor its site can be read.
pub const UNNAMED_LOGIN: &str = "a saved login names the login it is";

/// A saved login whose site could not be read: nothing is typed rather than guessing a site.
pub const UNREADABLE_LOGIN: &str = "Your saved login could not be read, so nothing was typed.";

/// The site a card names, as the model wrote it: its `liveHost`, else its `domain`, each only
/// when it names a host. For a card filled by hand. A saved login is checked against the site
/// it was saved for instead, which the model cannot write.
#[must_use]
pub fn card_site(form: &FormRequest) -> Option<String> {
    let named = |raw: &Option<String>| raw.as_deref().map(host_of).filter(|h| !h.is_empty());
    named(&form.live_host).or_else(|| named(&form.domain))
}

/// The site a saved login was saved for (its stored `origin`), as a host. A saved login is
/// checked against this, never the card's `liveHost` or `domain`: those are the model's words,
/// and a card naming a look-alike page with that page in front would pass a check against them.
#[must_use]
pub fn origin_site(origin: &str) -> Option<String> {
    Some(host_of(origin)).filter(|h| !h.is_empty())
}

/// Why nothing may be typed now, in the person's words; `None` when the fill may go ahead.
/// `site` is what the page must be on ([`card_site`], or [`origin_site`] for a saved login).
/// `unknown_refuses`: a box that cannot say is refused too (a saved login on a shared
/// computer, where a wrong page is likely); otherwise it goes ahead as fills always did. A box
/// that says no browser page is open is refused always: there is no page to type into.
#[must_use]
pub fn front_page_refusal(
    site: Option<&str>,
    front: Front<'_>,
    unknown_refuses: bool,
) -> Option<String> {
    if front == Front::NoBrowser {
        return Some(match site {
            Some(site) => format!(
                "No browser page is open on this computer, so nothing was typed. Open {} and \
                 try again.",
                site.strip_prefix("www.").unwrap_or(site)
            ),
            None => "No browser page is open on this computer, so nothing was typed. Open the \
                     page and try again."
                .to_string(),
        });
    }
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
    let site = site.strip_prefix("www.").unwrap_or(site);
    match front {
        Front::Page(url) if url_is_on_host(url, site) => None,
        Front::Page(url) => Some(format!(
            "The page in front is {}, not {site}, so nothing was typed. Bring {site} to the \
             front and try again.",
            shown(url)
        )),
        Front::Unknown if unknown_refuses => Some(format!(
            "This computer can't say which page is in front, and it is shared with your other \
             Bots, so nothing was typed. Type the login by hand on {site}."
        )),
        Front::Unknown | Front::NoBrowser => None,
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
        let fb = card_site(&card("www.facebook.com"));
        let fb = fb.as_deref();
        let at = |url| front_page_refusal(fb, Front::Page(url), true);
        assert_eq!(at("https://www.facebook.com/login"), None);
        assert_eq!(at("https://m.facebook.com/"), None);
        // Another Bot's page is refused, whatever the computer.
        let youtube = front_page_refusal(fb, Front::Page("https://www.youtube.com/"), false);
        assert!(
            youtube
                .as_deref()
                .is_some_and(|why| why.contains("www.youtube.com")),
            "{youtube:?}"
        );
        // A look-alike is not the site.
        assert!(at("https://facebook.com.evil.example/").is_some());
        assert!(at("chrome://newtab/").is_some());
    }

    #[test]
    fn a_box_that_cannot_say_refuses_only_where_asked() {
        let fb = card_site(&card("www.facebook.com"));
        let fb = fb.as_deref();
        assert!(front_page_refusal(fb, Front::Unknown, true).is_some());
        assert_eq!(front_page_refusal(fb, Front::Unknown, false), None);
        // A card that names no site has nothing to check against: refused where asked.
        let bare = crate::user_form::form_request_from(&serde_json::json!({"title": "Code"}));
        let youtube = Front::Page("https://www.youtube.com/");
        assert_eq!(card_site(&bare), None);
        assert!(front_page_refusal(None, youtube, true).is_some());
        assert_eq!(front_page_refusal(None, youtube, false), None);
    }

    /// No browser page open: the keys would go to a terminal or a file manager. Refused on
    /// every computer, for every card, where a box that cannot say goes ahead.
    #[test]
    fn no_browser_page_types_nothing() {
        let fb = card_site(&card("www.facebook.com"));
        assert!(front_page_refusal(fb.as_deref(), Front::NoBrowser, false).is_some());
        assert!(front_page_refusal(None, Front::NoBrowser, false).is_some());
    }

    /// A blank or scheme-only `liveHost` names no site: the card's `domain` is checked instead,
    /// and the check is not skipped.
    #[test]
    fn a_blank_live_host_falls_back_to_the_domain() {
        for blank in ["", "   ", "https://"] {
            let form = crate::user_form::form_request_from(&serde_json::json!({
                "title": "Log in", "liveHost": blank, "domain": "facebook.com",
                "fields": [{"id": "email", "label": "Email"}]
            }));
            assert_eq!(
                card_site(&form).as_deref(),
                Some("facebook.com"),
                "{blank:?}"
            );
        }
    }

    /// A saved login's site is the one it was saved for, whatever form its origin is kept in.
    #[test]
    fn a_saved_logins_site_is_its_origin() {
        assert_eq!(
            origin_site("https://www.facebook.com/").as_deref(),
            Some("www.facebook.com")
        );
        assert_eq!(origin_site("facebook.com").as_deref(), Some("facebook.com"));
        assert_eq!(origin_site(" "), None);
    }
}
