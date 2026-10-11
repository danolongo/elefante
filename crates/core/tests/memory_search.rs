//! `memory_search` over a real store: the daemon's backing for the tool, not a fake index.

use std::{fs, sync::Arc};

use elefante_core::memory::MemSearchTool;
use elefante_mem::{
    MemKey, RepoId, Store,
    learn::{self, Candidate, IngestInput, Kind, Relation, Speaker, Turn},
    retrieve::Retriever,
};
use elefante_tools::{MemorySearch, MemorySearchRequest, SearchKind};

fn key() -> MemKey {
    MemKey::from_bytes([7; 32])
}

async fn teach(store: &Store, statement: &str, quote: &str, said: &str, speaker: Speaker) {
    let candidates = vec![Candidate {
        kind: Kind::Convention,
        label: "A convention".to_owned(),
        statement: statement.to_owned(),
        anchors: Vec::new(),
        quote: quote.to_owned(),
        relation: Relation::New,
    }];
    let excerpt = vec![Turn {
        speaker,
        text: said.to_owned(),
    }];
    // Notes decay with age, so they must be taught "now", not at the epoch.
    let now_ns = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    )
    .unwrap();
    store
        .writer()
        .call(move |s| {
            learn::ingest(
                &mut s.memory,
                &key(),
                &IngestInput {
                    candidates: &candidates,
                    excerpt: &excerpt,
                    shown: &[],
                },
                now_ns,
            )
            .unwrap();
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn a_note_search_returns_the_saved_note_framed_and_never_a_pending_one() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    let repo = RepoId::resolve_with_home(tmp.path(), None).unwrap();
    let store = Arc::new(Store::open_with_key(&repo, key()).unwrap());
    teach(
        &store,
        "Dependencies are managed with pnpm.",
        "always use pnpm",
        "Remember: always use pnpm.",
        Speaker::User,
    )
    .await;
    teach(
        &store,
        "The release pipeline signs artifacts with a hardware key.",
        "signs artifacts with a hardware key",
        "I believe the pipeline signs artifacts with a hardware key.",
        Speaker::Assistant,
    )
    .await;
    let stored = learn::list_async(&store, true).await.unwrap();
    assert!(
        stored.iter().any(|row| row.status == "pending"),
        "the assistant-only claim was stored, pending: {stored:?}"
    );

    let tool = MemSearchTool::new(Arc::new(Retriever::new(Arc::clone(&store))), None);
    let found = tool
        .search(MemorySearchRequest {
            query: "pnpm hardware key artifacts".to_owned(),
            limit: 20,
            kind: Some(SearchKind::Note),
        })
        .await
        .unwrap();
    assert!(
        found.starts_with("Notes recorded from earlier sessions."),
        "{found}"
    );
    assert!(
        found.contains("memory convention — Dependencies are managed with pnpm."),
        "{found}"
    );
    assert!(
        !found.contains("hardware key"),
        "pending never returned: {found}"
    );

    let code_only = tool
        .search(MemorySearchRequest {
            query: "pnpm".to_owned(),
            limit: 8,
            kind: Some(SearchKind::File),
        })
        .await
        .unwrap();
    assert!(!code_only.contains("memory convention"), "{code_only}");
}
