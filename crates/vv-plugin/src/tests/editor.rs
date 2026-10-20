use std::sync::Arc;

use super::*;
use crate::HostedPlugin;
use crate::testing::{fixture_effect, fixture_instance};

#[test]
fn editor_open_fails_cleanly_without_view() {
    let instance: Arc<dyn HostedPlugin> = fixture_instance(&fixture_effect(&[]));
    // The fixture's createView returns null: nothing to attach, no window made.
    let err = Editor::open(&instance, "Venturi Test Gain", Arc::new(|| {})).err();
    // On macOS the test thread is refused first: only the main thread may open windows.
    let expected = if cfg!(target_os = "macos") {
        "plugin editors open on the main thread"
    } else {
        "the plugin has no editor"
    };
    assert_eq!(err.as_deref(), Some(expected));
}

#[test]
fn a_refused_constrained_size_is_requested_once_and_the_plugin_hears_the_constrained_one() {
    let mut requested = None;
    let first = resize_decision((1000, 700), (900, 700), &mut requested);
    assert_eq!(first.request, Some((900, 700)));
    assert_eq!(first.notify, (900, 700));
    let refused = resize_decision((1000, 700), (900, 700), &mut requested);
    assert_eq!(refused.request, None);
    assert_eq!(refused.notify, (900, 700));
}

#[test]
fn a_user_resize_after_a_refusal_is_constrained_again() {
    let mut requested = None;
    resize_decision((1000, 700), (900, 700), &mut requested);
    resize_decision((1000, 700), (900, 700), &mut requested);
    let user = resize_decision((1100, 700), (900, 700), &mut requested);
    assert_eq!(user.request, Some((900, 700)));
}

#[test]
fn a_different_answer_to_a_request_is_not_asked_for_again() {
    let mut requested = None;
    resize_decision((1000, 700), (900, 700), &mut requested);
    let answer = resize_decision((1200, 700), (900, 700), &mut requested);
    assert_eq!(answer.request, None);
}

#[test]
fn a_report_that_matches_the_constraint_requests_nothing() {
    let mut requested = Some((900, 700));
    let ok = resize_decision((900, 700), (900, 700), &mut requested);
    assert_eq!(ok.request, None);
    assert_eq!(requested, None);
}
