//! Full-text search over one session's messages, through the server's index
//! (`lib/messageSearch.ts`, `packages/web/server/lib/message-search`).

use openchamber_native_core::ids::{MessageId, SessionId};
use reqwest::StatusCode;
use serde::Deserialize;

use crate::client::{ServerClient, read_json};
use crate::error::TransportError;

/// One page of a session search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchPage {
    /// Matching messages, oldest first, and the cursor of the next page.
    Hits {
        hits: Vec<SearchHit>,
        next: Option<String>,
    },
    /// No word of the query is long enough for the index (3 characters).
    QueryTooShort,
    /// This server keeps no index, or has no search route.
    Unavailable,
}

/// A message that matched; `reasoning` when the match is in its reasoning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub message: MessageId,
    pub reasoning: bool,
}

#[derive(Deserialize)]
#[serde(tag = "status")]
enum Response {
    #[serde(rename = "ok")]
    Ok {
        hits: Vec<WireHit>,
        next: Option<String>,
    },
    #[serde(rename = "query-too-short")]
    QueryTooShort,
}

#[derive(Deserialize)]
struct WireHit {
    id: String,
    role: String,
}

impl ServerClient {
    /// One page of the session's messages matching `query`, oldest first
    /// (`searchMessages` with `order: 'asc'`). A failed request is an error,
    /// never an empty result.
    pub async fn search_session_messages(
        &self,
        session: &SessionId,
        query: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<SearchPage, TransportError> {
        let mut url = self.endpoint().url("api/openchamber/message-search");
        {
            let mut pairs = url.query_pairs_mut();
            pairs
                .append_pair("q", query)
                .append_pair("limit", &limit.to_string())
                .append_pair("session", session.as_str())
                .append_pair("order", "asc");
            if let Some(before) = before {
                pairs.append_pair("before", before);
            }
        }
        let response = self
            .get_request(url, None)
            .send()
            .await
            .map_err(TransportError::from_request)?;
        if matches!(
            response.status(),
            StatusCode::NOT_FOUND | StatusCode::SERVICE_UNAVAILABLE
        ) {
            return Ok(SearchPage::Unavailable);
        }
        Ok(match read_json::<Response>(response).await? {
            Response::QueryTooShort => SearchPage::QueryTooShort,
            Response::Ok { hits, next } => SearchPage::Hits {
                hits: hits
                    .into_iter()
                    .filter(|hit| !hit.id.is_empty())
                    .map(|hit| SearchHit {
                        message: MessageId::new(hit.id),
                        reasoning: hit.role == "reasoning",
                    })
                    .collect(),
                next,
            },
        })
    }
}
