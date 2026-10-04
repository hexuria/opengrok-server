#![allow(clippy::unwrap_used)]
use super::*;
use proptest::prelude::*;
proptest! {
    #[test]
    fn accepted_paths_never_contain_traversal_or_url_controls(path in ".{0,150}") {
        if path_ok(&path) {
            prop_assert!(!path.starts_with('/'));
            prop_assert!(!path.contains(['\\','?','#','%']));
            prop_assert!(path.split('/').all(|s| !s.is_empty() && s != "." && s != ".."));
        }
    }
    #[test]
    fn accepted_revisions_are_only_full_commit_hashes(sha in ".{0,70}") {
        if revision_ok(&sha) { prop_assert_eq!(sha.len(),40); prop_assert!(sha.bytes().all(|b| b.is_ascii_hexdigit())); }
    }
}
#[test]
fn source_urls_cannot_change_the_fetch_host() {
    assert_eq!(
        github_repo("https://github.com/owner/repo.git"),
        Some("owner/repo".into())
    );
    for url in [
        "https://github.com.evil/owner/repo",
        "https://localhost/owner/repo",
        "https://github.com/owner/repo/../other",
        "https://github.com/owner/repo?token=secret",
    ] {
        assert!(github_repo(url).is_none());
    }
}
