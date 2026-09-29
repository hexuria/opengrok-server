use opengrok_tools::credential::offer_save_payload;

#[test]
fn offer_save_payload_is_origin_username_and_entry_only() {
    let payload = offer_save_payload("example.com", "ada", "e_1");
    assert_eq!(payload["origin"], "example.com");
    assert_eq!(payload["username"], "ada");
    assert_eq!(payload["formEntryId"], "e_1");
    assert_eq!(payload.as_object().map(|o| o.len()), Some(3));
}
