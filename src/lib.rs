//! The OMDb metadata provider: a `scryer:metadata/metadata-provider@1.0.0`
//! component built for `wasm32-wasip2`.
//!
//! Scryer calls it only for movies SMG cannot resolve by IMDb id. Each lookup
//! is answered from the plugin's one-day cache when it can be, and otherwise
//! spends one request from the daily budget. The logic lives in `omdb-core`;
//! this crate moves bytes between it and the host.

use omdb_core::{AnswerCache, BUDGET_STATE_KEY, CACHE_STATE_KEY};
use scryer::runtime::host::{self, Header, HttpRequest, LogLevel, TransportError};
use scryer_plugin_sdk::command::{
    PluginCommand, PluginCommandRequest, PluginCommandResponse, PluginCommandResult,
    PluginMetadataCommand, PluginMetadataCommandResult,
};
use scryer_plugin_sdk::{
    MetadataPluginHealthResponse, MetadataPluginLookupResponse, MetadataPluginTitle, PluginError,
    PluginErrorCode, PluginResult,
};

wit_bindgen::generate!({
    world: "metadata-provider",
    path: "wit",
    generate_all,
});

/// Compare-and-swap attempts before a state write is given up.
const STATE_WRITE_ATTEMPTS: usize = 8;

struct Omdb;

impl Guest for Omdb {
    fn describe() -> Vec<u8> {
        serde_json::to_vec(&omdb_core::descriptor()).expect("the OMDb descriptor serializes")
    }

    async fn process(request: Vec<u8>) -> Result<Vec<u8>, InvocationError> {
        let request: PluginCommandRequest =
            serde_json::from_slice(&request).map_err(|_| InvocationError::InvalidResponse)?;
        let PluginCommand::Metadata(command) = request.command else {
            return Err(InvocationError::Failed);
        };
        let result = match command {
            PluginMetadataCommand::Lookup(request) => {
                PluginMetadataCommandResult::Lookup(lookup(&request.imdb_id).await)
            }
            PluginMetadataCommand::Search(_) => PluginMetadataCommandResult::Search(
                PluginResult::Err(omdb_core::title_search_unsupported()),
            ),
            PluginMetadataCommand::Health(_) => PluginMetadataCommandResult::Health(health().await),
        };
        serde_json::to_vec(&PluginCommandResponse::new(PluginCommandResult::Metadata(
            result,
        )))
        .map_err(|_| InvocationError::Failed)
    }
}

async fn lookup(raw_imdb_id: &str) -> PluginResult<MetadataPluginLookupResponse> {
    let Some(imdb_id) = scryer_plugin_sdk::normalize_imdb_id(raw_imdb_id) else {
        return PluginResult::Ok(MetadataPluginLookupResponse::default());
    };
    let now_ms = host::wall_now_ms();
    let cache = AnswerCache::decode(host::state_get(CACHE_STATE_KEY).as_deref());
    if let Some(title) = cache.get(&imdb_id, now_ms) {
        return PluginResult::Ok(MetadataPluginLookupResponse { title });
    }
    match fetch(&imdb_id, now_ms).await {
        Ok(title) => {
            remember(&imdb_id, title.clone(), now_ms);
            PluginResult::Ok(MetadataPluginLookupResponse { title })
        }
        Err(error) => {
            host::log(
                LogLevel::Warn,
                &format!("OMDb lookup for {imdb_id} failed: {}", error.public_message),
            );
            PluginResult::Err(error)
        }
    }
}

async fn health() -> PluginResult<MetadataPluginHealthResponse> {
    let now_ms = host::wall_now_ms();
    match fetch(omdb_core::HEALTH_PROBE_IMDB_ID, now_ms).await {
        Ok(Some(_)) => PluginResult::Ok(MetadataPluginHealthResponse {
            healthy: true,
            message: None,
        }),
        Ok(None) => PluginResult::Ok(MetadataPluginHealthResponse {
            healthy: false,
            message: Some("OMDb answered, but without the probe title.".to_string()),
        }),
        Err(error) => PluginResult::Ok(MetadataPluginHealthResponse {
            healthy: false,
            message: Some(error.public_message),
        }),
    }
}

/// One OMDb request, paid for from the daily budget.
async fn fetch(imdb_id: &str, now_ms: u64) -> Result<Option<MetadataPluginTitle>, PluginError> {
    let api_key = omdb_core::api_key(host::config_get(&omdb_core::setting_config_key(
        omdb_core::SETTING_API_KEY,
    )))?;
    let limit = omdb_core::daily_limit(
        host::config_get(&omdb_core::setting_config_key(
            omdb_core::SETTING_DAILY_LIMIT,
        ))
        .as_deref(),
    );
    spend_budget(now_ms, limit)?;
    let response = host::http(HttpRequest {
        method: "GET".to_string(),
        url: omdb_core::lookup_url(imdb_id, &api_key),
        headers: vec![Header {
            name: "Accept".to_string(),
            value: "application/json".to_string(),
        }],
        body: Vec::new(),
    })
    .await
    .map_err(transport_error)?;
    omdb_core::parse_lookup(response.status, &response.body, now_ms)
}

fn spend_budget(now_ms: u64, limit: u32) -> Result<(), PluginError> {
    for _ in 0..STATE_WRITE_ATTEMPTS {
        let current = host::state_get(BUDGET_STATE_KEY);
        let next = omdb_core::spend_budget(current.as_deref(), now_ms, limit)?;
        if host::state_cas(BUDGET_STATE_KEY, current.as_deref(), Some(&next)) {
            return Ok(());
        }
    }
    // Never spend a request the budget could not record.
    Err(omdb_core::plugin_error(
        PluginErrorCode::Temporary,
        "The plugin could not record its OMDb request budget.",
    ))
}

/// Cache an answer. A write that keeps losing its race is skipped: the next
/// lookup simply asks OMDb again.
fn remember(imdb_id: &str, title: Option<MetadataPluginTitle>, now_ms: u64) {
    for _ in 0..STATE_WRITE_ATTEMPTS {
        let current = host::state_get(CACHE_STATE_KEY);
        let mut cache = AnswerCache::decode(current.as_deref());
        cache.put(imdb_id, title.clone(), now_ms);
        if host::state_cas(CACHE_STATE_KEY, current.as_deref(), Some(&cache.encode())) {
            return;
        }
    }
}

fn transport_error(error: TransportError) -> PluginError {
    let (code, message) = match error {
        TransportError::Timeout => (PluginErrorCode::Temporary, "OMDb did not answer in time."),
        TransportError::ForbiddenOrigin => (
            PluginErrorCode::InvalidConfig,
            "Scryer did not allow the request to OMDb.",
        ),
        TransportError::Capacity => (
            PluginErrorCode::Temporary,
            "Scryer is busy; the OMDb request was not sent.",
        ),
        TransportError::Cancelled => (
            PluginErrorCode::Temporary,
            "The OMDb request was cancelled.",
        ),
        TransportError::ResponseTooLarge => (
            PluginErrorCode::UpstreamUnavailable,
            "OMDb sent an answer larger than Scryer accepts.",
        ),
        TransportError::InvalidRequest | TransportError::Transport => (
            PluginErrorCode::UpstreamUnavailable,
            "OMDb could not be reached.",
        ),
    };
    omdb_core::plugin_error(code, message)
}

export!(Omdb);
