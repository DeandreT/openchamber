//! The exact chatPromptReadOnly.ts policy, independent of prompt dispatch.

use openchamber_native_core::ids::SessionId;
use openchamber_native_core::session::{SessionKind, SessionRecord};

#[test]
fn child_preference_and_review_exception_preserve_the_reference_policy() {
    let mut session = SessionRecord::new("ses_child", "/project");
    for allow in [false, true] {
        for surface in [false, true] {
            assert_eq!(session.prompt_read_only(allow, surface), surface);
        }
    }
    session.parent_id = Some(SessionId::from("ses_parent"));
    session.title = "Code review".into();
    session.agent = Some("review".into());
    for surface in [false, true] {
        assert!(
            session.prompt_read_only(false, surface),
            "title and agent do not mark a review"
        );
        assert!(
            !session.prompt_read_only(true, surface),
            "child preference overrides surface policy"
        );
    }
    session.kind = SessionKind::Review;
    for allow in [false, true] {
        for surface in [false, true] {
            assert_eq!(
                session.prompt_read_only(allow, surface),
                surface,
                "explicit review metadata overrides stale parent identity"
            );
        }
    }
}
