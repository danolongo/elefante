//! The daemon's `web_search` backend: one provider request with the web plugin per search.
use std::sync::{Arc, Mutex};

use elefante_core::tools::{WebSearch, WebSearchRequest};
use elefante_provider::{Event, Message, ModelId, Provider, Request, Role, Usage};
use futures_util::{StreamExt as _, future::BoxFuture};

/// Selects the search model; unset, searches use the session's author model.
pub(super) const ENV_MODEL: &str = "ELEFANTE_SEARCH_MODEL";
const MAX_TOKENS: u32 = 1024;
const SYSTEM: &str = "Answer the search query briefly from the web results. List each source as a numbered plain URL on its own line.";

type ProviderSource = Box<dyn Fn() -> Option<Arc<dyn Provider>> + Send + Sync>;

/// Runs each search on the current provider and records its usage for the session.
pub(super) struct ProviderSearch {
    provider: ProviderSource,
    model: ModelId,
    usage: Arc<Mutex<Usage>>,
}

impl std::fmt::Debug for ProviderSearch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderSearch")
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl ProviderSearch {
    /// `provider` is read at each search, so a key set after the session started still works.
    pub(super) fn new(
        provider: impl Fn() -> Option<Arc<dyn Provider>> + Send + Sync + 'static,
        model: ModelId,
        usage: Arc<Mutex<Usage>>,
    ) -> Self {
        Self {
            provider: Box::new(provider),
            model,
            usage,
        }
    }
}

/// `ELEFANTE_SEARCH_MODEL` when set and non-empty, otherwise the author's model.
pub(super) fn search_model(author: &ModelId, get: &dyn Fn(&str) -> Option<String>) -> ModelId {
    get(ENV_MODEL)
        .map(|model| model.trim().to_owned())
        .filter(|model| !model.is_empty())
        .map_or_else(|| author.clone(), ModelId::from)
}

impl WebSearch for ProviderSearch {
    fn search(&self, request: WebSearchRequest) -> BoxFuture<'_, Result<String, String>> {
        Box::pin(async move {
            let provider = (self.provider)()
                .ok_or("web_search is unavailable: OPENROUTER_API_KEY is not set")?;
            let mut call = Request::new(
                self.model.clone(),
                MAX_TOKENS,
                vec![Message::text(Role::User, request.query)],
            );
            call.system = vec![SYSTEM.to_owned()];
            call.web_search = Some(elefante_provider::WebSearch {
                max_results: request.max_results,
            });
            let mut stream = provider.stream(call);
            let mut text = String::new();
            let mut usage = Usage::default();
            let mut failure = None;
            while let Some(event) = stream.next().await {
                match event {
                    Ok(Event::TextDelta(delta)) => text.push_str(&delta),
                    Ok(Event::Usage(report)) => usage.accumulate(report),
                    Ok(_) => {}
                    Err(error) => {
                        failure = Some(format!("web_search failed: {error}"));
                        break;
                    }
                }
            }
            // A failed search still cost what the provider reported.
            self.usage
                .lock()
                .expect("search usage poisoned")
                .accumulate(usage);
            if let Some(failure) = failure {
                return Err(failure);
            }
            if text.trim().is_empty() {
                return Err("web_search returned no results".to_owned());
            }
            Ok(text)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use elefante_provider::{Capabilities, ProviderError, StopReason};
    use futures_util::stream::{self, BoxStream};

    struct Recording {
        requests: Mutex<Vec<Request>>,
        events: Vec<Result<Event, ProviderError>>,
    }

    impl Provider for Recording {
        fn capabilities(&self, _: &ModelId) -> Capabilities {
            Capabilities::default()
        }

        fn stream(&self, request: Request) -> BoxStream<'static, Result<Event, ProviderError>> {
            self.requests.lock().unwrap().push(request);
            let events: Vec<_> = self
                .events
                .iter()
                .map(|event| match event {
                    Ok(event) => Ok(event.clone()),
                    Err(_) => Err(ProviderError::Protocol("cut".into())),
                })
                .collect();
            Box::pin(stream::iter(events))
        }
    }

    fn usage(input: u64, cost: f64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: 2,
            cost: Some(cost),
            ..Usage::default()
        }
    }

    fn search(
        events: Vec<Result<Event, ProviderError>>,
    ) -> (Arc<Recording>, ProviderSearch, Arc<Mutex<Usage>>) {
        let provider = Arc::new(Recording {
            requests: Mutex::default(),
            events,
        });
        let total = Arc::new(Mutex::new(usage(100, 0.5)));
        let source: Arc<dyn Provider> = provider.clone();
        let search = ProviderSearch::new(
            move || Some(Arc::clone(&source)),
            ModelId::from("search/model"),
            Arc::clone(&total),
        );
        (provider, search, total)
    }

    #[tokio::test]
    async fn one_request_carries_the_web_plugin_and_its_usage_joins_the_session() {
        let (provider, search, total) = search(vec![
            Ok(Event::TextDelta("1. https://example.com/a".into())),
            Ok(Event::Usage(usage(7, 0.25))),
            Ok(Event::Stop(StopReason::EndTurn)),
        ]);
        let result = search
            .search(WebSearchRequest {
                query: "rust release".into(),
                max_results: 3,
            })
            .await;
        assert_eq!(result.as_deref(), Ok("1. https://example.com/a"));
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].model, ModelId::from("search/model"));
        assert_eq!(
            requests[0].web_search,
            Some(elefante_provider::WebSearch { max_results: 3 })
        );
        assert_eq!(requests[0].tools, []);
        let total = *total.lock().unwrap();
        assert_eq!(total.input_tokens, 107);
        assert_eq!(total.cost, Some(0.75));
    }

    #[tokio::test]
    async fn a_failed_search_reports_the_error_and_still_counts_its_usage() {
        let (_, search, total) = search(vec![
            Ok(Event::Usage(usage(5, 0.125))),
            Err(ProviderError::Protocol("cut".into())),
        ]);
        let result = search
            .search(WebSearchRequest {
                query: "rust".into(),
                max_results: 1,
            })
            .await;
        assert!(result.unwrap_err().starts_with("web_search failed"));
        assert_eq!(total.lock().unwrap().input_tokens, 105);
    }

    #[test]
    fn the_search_model_falls_back_to_the_author() {
        let author = ModelId::from("author/model");
        assert_eq!(search_model(&author, &|_| None), author);
        assert_eq!(search_model(&author, &|_| Some("  ".into())), author);
        assert_eq!(
            search_model(&author, &|name| (name == ENV_MODEL).then(|| " s/m ".into())),
            ModelId::from("s/m")
        );
    }
}
