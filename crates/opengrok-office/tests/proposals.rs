//! A PPTX story ref is built from three ids that each carry their own colons
//! (`sp:slide:0:256:slide:0:256:shape:2:story:slide:0:256:shape:2:0`), so nothing may
//! ever split it back apart. The regression this file guards once shipped as exactly
//! that split: every propose and ref-read the outline had just produced refused with a
//! truncated "story … is gone". The fix never parses — it recognises the ref whole
//! against the stories it could name.

use opengrok_office::{Error, Kind, Session, TextEditInput};

const DEMO_DECK: &[u8] = include_bytes!("fixtures/betteroffice-demo.pptx");

#[test]
fn a_pptx_ref_the_outline_made_proposes_and_reads() -> Result<(), Error> {
    let session = Session::open(DEMO_DECK, Kind::Pptx)?;
    let outline = session.outline(None, false, 0, 50)?;
    let story = &outline["stories"][0];
    let ref_ = story["ref"].as_str().unwrap_or_default().to_string();
    assert!(ref_.starts_with("sp:"), "not a story ref: {ref_}");

    // Read by the ref itself: pre-fix this fell through to "unreadable pptx ref".
    let page = session.read(&ref_, 0, 16000, None)?;
    assert_eq!(page["ref"], ref_);
    let text = page["text"].as_str().unwrap_or_default().to_string();
    assert!(!text.is_empty(), "a real story reads empty");

    // Grep a word the story actually carries, then propose over its match handle:
    // pre-fix this refused with a truncated story id.
    let word = text
        .split_whitespace()
        .find(|w| w.chars().filter(|c: &char| c.is_alphanumeric()).count() >= 3)
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
        .unwrap_or_default();
    assert!(
        !word.is_empty(),
        "demo deck stories carry no greppable word"
    );
    let hits = session.grep(&word, false, None, 10, None)?;
    assert_eq!(hits["matches"][0]["ref"], ref_);
    let handle = hits["matches"][0]["match"].as_str().unwrap_or_default();

    let proposal = session.propose(
        "bot",
        "regression",
        &[TextEditInput {
            match_: handle.to_string(),
            new_text: "PROBED".to_string(),
        }],
        0,
    )?;
    assert_eq!(proposal.status, opengrok_office::ProposalStatus::Pending);
    Ok(())
}

#[test]
fn a_pptx_ref_no_story_owns_still_refuses() -> Result<(), Error> {
    let session = Session::open(DEMO_DECK, Kind::Pptx)?;
    let missing = "sp:slide:9:999:slide:9:999:shape:9:story:slide:9:999:shape:9:0";
    assert!(session.read(missing, 0, 10, None).is_err());
    Ok(())
}
