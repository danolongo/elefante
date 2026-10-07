//! The repository index as a source of context and as the backing for `memory_search`.

use std::sync::Arc;

use elefante_mem::{
    Store, brief,
    retrieve::{DEFAULT_BUDGET_TOKENS, Query, Retriever, SearchKind as MemSearchKind},
};
use elefante_tools::{MemorySearch, MemorySearchRequest, SearchKind};
use futures_util::future::BoxFuture;

use crate::ContextSource;

/// Context drawn from a repository's index.
///
/// A failure to read the index is a missing block, never a failed turn: memory is an aid, and a
/// corrupt or busy index must not stop the agent from working.
pub struct MemContext {
    retriever: Arc<Retriever>,
    store: Arc<Store>,
    scope: Option<String>,
}

impl MemContext {
    /// `scope` is the session's working directory relative to the repository root, so a session
    /// started in a subdirectory is not told about files its tools cannot open.
    #[must_use]
    pub fn new(store: Arc<Store>, retriever: Arc<Retriever>, scope: Option<String>) -> Self {
        Self {
            retriever,
            store,
            scope,
        }
    }
}

impl ContextSource for MemContext {
    fn turn_block<'a>(&'a self, prompt: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let query = Query {
                text: prompt.to_owned(),
                files: Vec::new(),
                scope: self.scope.clone(),
                budget_tokens: DEFAULT_BUDGET_TOKENS,
            };
            let block = self.retriever.retrieve(query).await.ok().flatten()?;
            Some(block.rendered.clone())
        })
    }

    fn preamble(&self) -> BoxFuture<'_, Option<String>> {
        Box::pin(async move { brief::get_or_render_async(&self.store).await.ok().flatten() })
    }
}

/// The `memory_search` tool's backing, over the same index and scope as the session's context.
#[derive(Debug)]
pub struct MemSearchTool {
    retriever: Arc<Retriever>,
    scope: Option<String>,
}

impl MemSearchTool {
    #[must_use]
    pub fn new(retriever: Arc<Retriever>, scope: Option<String>) -> Self {
        Self { retriever, scope }
    }
}

impl MemorySearch for MemSearchTool {
    fn search(&self, request: MemorySearchRequest) -> BoxFuture<'_, Result<String, String>> {
        Box::pin(async move {
            let query = Query {
                text: request.query,
                files: Vec::new(),
                scope: self.scope.clone(),
                budget_tokens: DEFAULT_BUDGET_TOKENS,
            };
            let kind = request.kind.map(|kind| match kind {
                SearchKind::File => MemSearchKind::File,
                SearchKind::Symbol => MemSearchKind::Symbol,
            });
            self.retriever
                .search(query, kind, request.limit)
                .await
                .map_err(|error| format!("repository search failed: {error}"))
        })
    }
}
