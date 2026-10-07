use elefante_mem::{MemError, actor::Actor};

#[test]
fn serialises_requests_and_counts_them() {
    let actor = Actor::spawn("test", 0u32);
    for _ in 0..5 {
        actor
            .call_blocking(|n| {
                *n += 1;
            })
            .unwrap();
    }
    assert_eq!(actor.call_blocking(|n| *n).unwrap(), 5);
    assert_eq!(actor.calls(), 6);
}

#[test]
fn a_panicking_request_does_not_kill_the_actor() {
    let actor = Actor::spawn("test", 7u32);
    let outcome = actor.call_blocking(|_| -> u32 { panic!("boom") });
    assert!(matches!(outcome, Err(MemError::ActorPanicked)));
    assert_eq!(actor.call_blocking(|n| *n).unwrap(), 7);
}

#[tokio::test]
async fn async_calls_complete_without_blocking_the_runtime() {
    let actor = Actor::spawn("test", String::new());
    actor.call(|s| s.push_str("ab")).await.unwrap();
    assert_eq!(actor.call(|s| s.clone()).await.unwrap(), "ab");
}

#[test]
fn dropping_the_actor_stops_its_thread() {
    // `Drop` joins the thread; a hang here would fail the test run by timeout.
    drop(Actor::spawn("test", ()));
}
