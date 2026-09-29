use super::*;
use serde_json::json;

#[test]
fn egress_tunnel_is_off_by_default() {
    let settings = default_settings();
    assert_eq!(settings["egressTunnelEnabled"], false);
    assert!(!egress_tunnel_from(&settings, None, None));
}

#[test]
fn egress_tunnel_env_or_setting_turns_it_on() {
    let settings = default_settings();
    assert!(egress_tunnel_from(&settings, Some("1"), None));
    assert!(egress_tunnel_from(&settings, None, Some("1")));
    assert!(
        !egress_tunnel_from(&settings, Some("true"), None),
        "Grok host parity is strictly === \"1\""
    );
    assert!(egress_tunnel_from(
        &json!({ "egressTunnelEnabled": true }),
        None,
        None
    ));
}

#[test]
fn advertised_needs_the_box_ready_when_info_is_present() {
    let ready = opengrok_box::EgressTunnel {
        enabled: true,
        ready: true,
    };
    let waiting = opengrok_box::EgressTunnel {
        enabled: true,
        ready: false,
    };
    assert!(!opengrok_box::EgressTunnel::advertised(true, None));
    assert!(opengrok_box::EgressTunnel::advertised(true, Some(ready)));
    assert!(!opengrok_box::EgressTunnel::advertised(true, Some(waiting)));
    assert!(!opengrok_box::EgressTunnel::advertised(false, Some(ready)));
}
