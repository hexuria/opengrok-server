use super::*;

fn grant() -> Grant {
    Grant {
        account: "acc_1".into(),
        coworker: "cw_1".into(),
        box_id: "og-box-1".into(),
    }
}

/// A code is a key for one Bot's screen: unguessable, plain enough for a username, good until it
/// expires and no longer, and never another code's grant.
#[test]
fn a_code_reaches_its_grant_until_it_expires() {
    let codes = Codes::default();
    let code = codes.mint(grant()).unwrap();
    assert!(code.starts_with("og_") && code.len() == 29, "{code}");
    assert!(
        code[3..]
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    );
    assert_eq!(codes.redeem(&code), Some(grant()));
    assert_eq!(codes.redeem(&code), Some(grant()), "a reconnect is let in");
    assert_eq!(codes.redeem("og_aaaaaaaaaaaaaaaaaaaaaaaaaa"), None);
    assert_eq!(codes.redeem(&code[..20]), None, "compared whole");
    let other = codes
        .mint(Grant {
            coworker: "cw_2".into(),
            ..grant()
        })
        .unwrap();
    assert_ne!(other, code);
    assert_eq!(codes.redeem(&other).unwrap().coworker, "cw_2");
    codes.expire(&code);
    assert_eq!(codes.redeem(&code), None, "an expired code is no key");
}

#[test]
fn base32_is_rfc_4648_lowercase_without_padding() {
    assert_eq!(base32(b""), "");
    assert_eq!(base32(b"f"), "my");
    assert_eq!(base32(b"foobar"), "mzxw6ytboi");
}
