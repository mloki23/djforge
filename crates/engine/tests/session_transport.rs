use serde_json::json;
use soundcraft_engine::{Engine, TransportRequest, TransportStatus};
use soundcraft_model::Session;
use soundcraft_time::Range;

#[test]
fn replacing_session_resets_status_and_discards_old_transport_requests() {
    let mut engine = Engine::default();
    engine.transport = TransportStatus { playing: true, recording: true, position: 48_000 };
    engine.execute("transport.play", &json!({"from": 96_000})).unwrap();
    engine.execute("transport.record", &json!({})).unwrap();
    let mut session = Session::default();
    session.edit.selection = Range::point(12_000);
    engine.replace_session(session);
    assert_eq!(engine.transport, TransportStatus { position: 12_000, ..TransportStatus::default() });
    assert_eq!(engine.transport_requests, vec![TransportRequest::ResetSession]);
    assert!(!engine.is_dirty());
}

#[test]
fn failed_new_session_keeps_the_current_transport() {
    let mut engine = Engine::default();
    engine.transport = TransportStatus { playing: true, position: 48_000, ..TransportStatus::default() };
    engine.execute("transport.play", &json!({})).unwrap();
    let status = engine.transport;
    let requests = engine.transport_requests.clone();
    assert!(engine.execute("session.new", &json!({"sample_rate": 0})).is_err());
    assert_eq!(engine.transport, status);
    assert_eq!(engine.transport_requests, requests);
}

#[test]
fn a_new_session_starts_with_an_empty_clipboard() {
    // Copied clips name the old session's audio files and clip ids, which mean nothing in the next.
    let mut engine = soundcraft_engine::demo::demo_engine();
    let kick = engine.session().track_by_name("Kick").unwrap().id;
    engine.execute("edit.copy", &json!({"tracks": [kick.0], "start": 0, "end": 480_000})).unwrap();
    assert!(!engine.clipboard.tracks.is_empty());
    engine.replace_session(Session::default());
    assert!(engine.clipboard.tracks.is_empty() && engine.clipboard.length == 0);
    assert!(engine.execute("edit.paste", &json!({})).is_err());
}
