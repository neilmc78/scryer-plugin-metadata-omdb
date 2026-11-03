//! OMDb answers as Scryer metadata.
//!
//! The plugin's logic lives here, free of the component bindings, so it builds
//! and tests natively: the descriptor, the request URL, reading OMDb's
//! answers, the answer cache and the daily request budget. The component in
//! the crate root only moves bytes between these functions and the host.
//!
//! Scryer asks a metadata provider only for movies SMG has no record of, by
//! IMDb id, so every OMDb request is spent on a genuine gap. On top of that the
//! plugin keeps answers (found or not) for a day and stops at a daily limit
//! below OMDb's free 1,000.

use std::collections::BTreeMap;

use scryer_plugin_sdk::{
    ConfigFieldDef, ConfigFieldType, MetadataMediaKind, MetadataPluginTitle,
    MetadataProviderCapabilities, MetadataProviderDescriptor, PluginDescriptor, PluginError,
    PluginErrorCode, PluginSettingField, ProviderDescriptor, SDK_VERSION, current_sdk_constraint,
};
use serde::{Deserialize, Serialize};

pub const PROVIDER_TYPE: &str = "omdb";
pub const BASE_URL: &str = "https://www.omdbapi.com/";
pub const HOST: &str = "www.omdbapi.com";

pub const SETTING_API_KEY: &str = "api_key";
pub const SETTING_DAILY_LIMIT: &str = "daily_limit";

/// The host hands installation settings to the plugin's config under
/// `plugin.settings.<key>`.
pub fn setting_config_key(setting: &str) -> String {
    format!("plugin.settings.{setting}")
}
pub const DEFAULT_DAILY_LIMIT: u32 = 900;
const MAX_DAILY_LIMIT: u32 = 100_000;

/// Plugin-state keys.
pub const CACHE_STATE_KEY: &str = "cache";
pub const BUDGET_STATE_KEY: &str = "budget";

/// How long an answer, found or not, is reused.
pub const CACHE_TTL_MS: u64 = DAY_MS;
/// The most answers kept, so the cache stays well inside the host's 1 MiB of
/// plugin state.
pub const CACHE_MAX_ENTRIES: usize = 150;

const DAY_MS: u64 = 86_400_000;

/// The title Check Connection asks for: always in OMDb.
pub const HEALTH_PROBE_IMDB_ID: &str = "tt0111161";

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PROVIDER_TYPE.to_string(),
        name: "OMDb".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: SDK_VERSION.to_string(),
        sdk_constraint: current_sdk_constraint(),
        socket_permissions: Vec::new(),
        settings: vec![
            PluginSettingField {
                field: ConfigFieldDef {
                    key: SETTING_API_KEY.to_string(),
                    label: "API key".to_string(),
                    field_type: ConfigFieldType::Password,
                    required: true,
                    help_text: Some(
                        "Your OMDb API key, from https://www.omdbapi.com/apikey.aspx.".to_string(),
                    ),
                    ..ConfigFieldDef::default()
                },
                sensitive: true,
            },
            PluginSettingField {
                field: ConfigFieldDef {
                    key: SETTING_DAILY_LIMIT.to_string(),
                    label: "Daily request limit".to_string(),
                    field_type: ConfigFieldType::Number,
                    default_value: Some(DEFAULT_DAILY_LIMIT.to_string()),
                    help_text: Some(
                        "The plugin stops calling OMDb after this many requests in a day (UTC). \
                         The free key allows 1,000."
                            .to_string(),
                    ),
                    ..ConfigFieldDef::default()
                },
                sensitive: false,
            },
        ],
        provider: ProviderDescriptor::MetadataProvider(MetadataProviderDescriptor {
            provider_type: PROVIDER_TYPE.to_string(),
            provider_aliases: Vec::new(),
            display_name: "OMDb".to_string(),
            summary: Some(
                "Movies SMG has no record of, such as fan edits, looked up on OMDb by IMDb id."
                    .to_string(),
            ),
            coverage: vec![MetadataMediaKind::Movie],
            capabilities: MetadataProviderCapabilities {
                search: false,
                health: true,
            },
            config_fields: Vec::new(),
            default_base_url: Some(BASE_URL.to_string()),
            allowed_hosts: vec![HOST.to_string()],
        }),
    }
}

// ── Errors ─────────────────────────────────────────────────────────────────

pub fn plugin_error(code: PluginErrorCode, message: &str) -> PluginError {
    PluginError {
        code,
        public_message: message.to_string(),
        debug_message: None,
        retry_after_seconds: None,
        details: None,
    }
}

pub fn missing_api_key() -> PluginError {
    plugin_error(
        PluginErrorCode::InvalidConfig,
        "Add your OMDb API key in the plugin's settings.",
    )
}

pub fn title_search_unsupported() -> PluginError {
    plugin_error(
        PluginErrorCode::Unsupported,
        "The OMDb plugin looks movies up by IMDb id only.",
    )
}

fn seconds_to_utc_midnight(now_ms: u64) -> i64 {
    let next_day_ms = (now_ms / DAY_MS + 1) * DAY_MS;
    i64::try_from((next_day_ms - now_ms).div_ceil(1000)).unwrap_or(i64::MAX)
}

fn rate_limited(message: &str, now_ms: u64) -> PluginError {
    PluginError {
        retry_after_seconds: Some(seconds_to_utc_midnight(now_ms)),
        ..plugin_error(PluginErrorCode::RateLimited, message)
    }
}

// ── Settings ───────────────────────────────────────────────────────────────

pub fn api_key(raw: Option<String>) -> Result<String, PluginError> {
    raw.map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
        .ok_or_else(missing_api_key)
}

/// The configured daily limit, or the default when unset or unreadable.
pub fn daily_limit(raw: Option<&str>) -> u32 {
    raw.map(str::trim)
        .and_then(|value| {
            value
                .parse::<u32>()
                .ok()
                .or_else(|| value.parse::<f64>().ok().map(|value| value as u32))
        })
        .filter(|limit| *limit > 0)
        .map_or(DEFAULT_DAILY_LIMIT, |limit| limit.min(MAX_DAILY_LIMIT))
}

// ── Request ────────────────────────────────────────────────────────────────

/// The OMDb URL for one title. OMDb takes its key only as a query parameter.
pub fn lookup_url(imdb_id: &str, api_key: &str) -> String {
    format!(
        "{BASE_URL}?i={}&plot=full&apikey={}",
        encode_query_value(imdb_id),
        encode_query_value(api_key)
    )
}

fn encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

// ── Response ───────────────────────────────────────────────────────────────

/// OMDb's record for one title. Missing values are `"N/A"`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OmdbTitle {
    #[serde(rename = "Response")]
    response: String,
    #[serde(rename = "Error")]
    error: String,
    #[serde(rename = "Title")]
    title: String,
    #[serde(rename = "Year")]
    year: String,
    #[serde(rename = "Rated")]
    rated: String,
    #[serde(rename = "Released")]
    released: String,
    #[serde(rename = "Runtime")]
    runtime: String,
    #[serde(rename = "Genre")]
    genre: String,
    #[serde(rename = "Director")]
    director: String,
    #[serde(rename = "Actors")]
    actors: String,
    #[serde(rename = "Plot")]
    plot: String,
    #[serde(rename = "Country")]
    country: String,
    #[serde(rename = "Poster")]
    poster: String,
    #[serde(rename = "imdbRating")]
    imdb_rating: String,
    #[serde(rename = "imdbVotes")]
    imdb_votes: String,
    #[serde(rename = "imdbID")]
    imdb_id: String,
    #[serde(rename = "Type")]
    kind: String,
}

/// Read OMDb's answer to a lookup. `Ok(None)` means OMDb has no movie under
/// that id; an error is one Scryer should not cache.
pub fn parse_lookup(
    status: u16,
    body: &[u8],
    now_ms: u64,
) -> Result<Option<MetadataPluginTitle>, PluginError> {
    let parsed = serde_json::from_slice::<OmdbTitle>(body);
    if !(200..300).contains(&status) && status != 401 {
        return Err(plugin_error(
            PluginErrorCode::UpstreamUnavailable,
            &format!("OMDb answered HTTP {status}."),
        ));
    }
    let record = parsed.map_err(|_| {
        plugin_error(
            PluginErrorCode::UpstreamUnavailable,
            "OMDb sent an answer the plugin could not read.",
        )
    })?;
    if !record.response.eq_ignore_ascii_case("true") {
        return match record.error.trim() {
            "Movie not found!" | "Incorrect IMDb ID." | "Error getting data." => Ok(None),
            "Request limit reached!" => Err(rate_limited(
                "OMDb's daily request limit for this key is reached.",
                now_ms,
            )),
            "Invalid API key!" | "No API key provided." => Err(plugin_error(
                PluginErrorCode::AuthFailed,
                "OMDb refused the API key.",
            )),
            _ if status == 401 => Err(plugin_error(
                PluginErrorCode::AuthFailed,
                "OMDb refused the API key.",
            )),
            other => Err(plugin_error(
                PluginErrorCode::UpstreamUnavailable,
                &format!("OMDb returned an error: {other}"),
            )),
        };
    }
    // Movies only: a series or episode under the id is no answer.
    if !record.kind.is_empty() && !record.kind.eq_ignore_ascii_case("movie") {
        return Ok(None);
    }
    let Some(imdb_id) = scryer_plugin_sdk::normalize_imdb_id(&record.imdb_id) else {
        return Ok(None);
    };
    let title = known(&record.title).map(str::to_string);
    let Some(title) = title else {
        return Ok(None);
    };
    Ok(Some(MetadataPluginTitle {
        imdb_id,
        title,
        year: leading_number(&record.year).and_then(|year| i32::try_from(year).ok()),
        release_date: known(&record.released).and_then(release_date),
        overview: known(&record.plot).map(str::to_string),
        genres: list(&record.genre),
        runtime_minutes: leading_number(&record.runtime)
            .and_then(|minutes| i32::try_from(minutes).ok())
            .filter(|minutes| *minutes > 0),
        poster_url: known(&record.poster)
            .filter(|poster| poster.starts_with("https://") || poster.starts_with("http://"))
            .map(str::to_string),
        content_rating: known(&record.rated).map(str::to_string),
        imdb_rating: known(&record.imdb_rating).and_then(|rating| rating.parse::<f64>().ok()),
        imdb_votes: known(&record.imdb_votes)
            .and_then(|votes| votes.replace(',', "").parse::<i64>().ok()),
        directors: list(&record.director),
        cast: list(&record.actors),
        original_language: None,
        country: list(&record.country).into_iter().next(),
    }))
}

fn known(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty() && !value.eq_ignore_ascii_case("N/A")).then_some(value)
}

fn list(value: &str) -> Vec<String> {
    known(value)
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn leading_number(value: &str) -> Option<u32> {
    let digits: String = value
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// `16 May 2005` as `2005-05-16`.
fn release_date(value: &str) -> Option<String> {
    let mut parts = value.split_whitespace();
    let day: u32 = parts.next()?.parse().ok()?;
    let month = match parts.next()?.to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    let year: u32 = parts.next()?.parse().ok()?;
    ((1..=31).contains(&day) && (1000..=9999).contains(&year))
        .then(|| format!("{year:04}-{month:02}-{day:02}"))
}

// ── Answer cache ───────────────────────────────────────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheEntry {
    at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<MetadataPluginTitle>,
}

/// Every cached answer, in one state value so the plugin can prune it.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AnswerCache {
    entries: BTreeMap<String, CacheEntry>,
}

impl AnswerCache {
    /// The stored cache; an unreadable value starts empty.
    pub fn decode(bytes: Option<&[u8]>) -> Self {
        bytes
            .and_then(|bytes| serde_json::from_slice(bytes).ok())
            .unwrap_or_default()
    }

    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// A fresh answer: `Some(None)` is a remembered "OMDb has no such movie".
    pub fn get(&self, imdb_id: &str, now_ms: u64) -> Option<Option<MetadataPluginTitle>> {
        self.entries
            .get(imdb_id)
            .filter(|entry| now_ms.saturating_sub(entry.at) < CACHE_TTL_MS)
            .map(|entry| entry.title.clone())
    }

    /// Remember an answer, dropping expired entries and then the oldest.
    pub fn put(&mut self, imdb_id: &str, title: Option<MetadataPluginTitle>, now_ms: u64) {
        self.entries
            .retain(|_, entry| now_ms.saturating_sub(entry.at) < CACHE_TTL_MS);
        self.entries
            .insert(imdb_id.to_string(), CacheEntry { at: now_ms, title });
        while self.entries.len() > CACHE_MAX_ENTRIES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.at)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }
}

// ── Daily budget ───────────────────────────────────────────────────────────

/// Spend one request from today's (UTC) budget. Returns the state value to
/// store, or a rate-limit error once `limit` requests went out today.
pub fn spend_budget(
    current: Option<&[u8]>,
    now_ms: u64,
    limit: u32,
) -> Result<Vec<u8>, PluginError> {
    let today = now_ms / DAY_MS;
    let spent = current
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|value| value.split_once(':'))
        .and_then(|(day, count)| Some((day.parse::<u64>().ok()?, count.parse::<u32>().ok()?)))
        .filter(|(day, _)| *day == today)
        .map_or(0, |(_, count)| count);
    if spent >= limit {
        return Err(rate_limited(
            "The plugin's daily OMDb request limit is reached; it resets at midnight UTC.",
            now_ms,
        ));
    }
    Ok(format!("{today}:{}", spent + 1).into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_793_000_000_000;

    const FAN_EDIT: &str = r#"{"Title":"Star Wars: Episode III - The Fan Edit","Year":"2021","Rated":"N/A","Released":"16 May 2021","Runtime":"128 min","Genre":"Action, Adventure, Sci-Fi","Director":"Some Editor","Writer":"N/A","Actors":"Ewan McGregor, Natalie Portman","Plot":"A recut.","Language":"English","Country":"United States, United Kingdom","Awards":"N/A","Poster":"https://m.media-amazon.com/images/M/x.jpg","Ratings":[],"Metascore":"N/A","imdbRating":"8.2","imdbVotes":"1,234","imdbID":"tt18183916","Type":"movie","Response":"True"}"#;

    #[test]
    fn a_movie_is_read_into_plugin_metadata() {
        let title = parse_lookup(200, FAN_EDIT.as_bytes(), NOW)
            .unwrap()
            .unwrap();
        assert_eq!(title.imdb_id, "tt18183916");
        assert_eq!(title.title, "Star Wars: Episode III - The Fan Edit");
        assert_eq!(title.year, Some(2021));
        assert_eq!(title.release_date.as_deref(), Some("2021-05-16"));
        assert_eq!(title.runtime_minutes, Some(128));
        assert_eq!(title.genres, ["Action", "Adventure", "Sci-Fi"]);
        assert_eq!(title.cast, ["Ewan McGregor", "Natalie Portman"]);
        assert_eq!(title.content_rating, None);
        assert_eq!(title.imdb_rating, Some(8.2));
        assert_eq!(title.imdb_votes, Some(1234));
        assert_eq!(title.country.as_deref(), Some("United States"));
        assert!(title.poster_url.is_some());
    }

    #[test]
    fn missing_titles_and_series_are_no_answer() {
        let not_found = br#"{"Response":"False","Error":"Incorrect IMDb ID."}"#;
        assert_eq!(parse_lookup(200, not_found, NOW).unwrap(), None);
        let series = FAN_EDIT.replace(r#""Type":"movie""#, r#""Type":"series""#);
        assert_eq!(parse_lookup(200, series.as_bytes(), NOW).unwrap(), None);
    }

    #[test]
    fn key_and_limit_failures_are_classified() {
        let invalid = br#"{"Response":"False","Error":"Invalid API key!"}"#;
        assert_eq!(
            parse_lookup(401, invalid, NOW).unwrap_err().code,
            PluginErrorCode::AuthFailed
        );
        let limit = br#"{"Response":"False","Error":"Request limit reached!"}"#;
        let error = parse_lookup(401, limit, NOW).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::RateLimited);
        assert!(error.retry_after_seconds.is_some_and(|seconds| seconds > 0));
        assert_eq!(
            parse_lookup(503, b"oops", NOW).unwrap_err().code,
            PluginErrorCode::UpstreamUnavailable
        );
    }

    #[test]
    fn the_url_carries_the_id_and_an_encoded_key() {
        assert_eq!(
            lookup_url("tt18183916", "ab c&d"),
            "https://www.omdbapi.com/?i=tt18183916&plot=full&apikey=ab%20c%26d"
        );
    }

    #[test]
    fn the_budget_stops_at_the_limit_and_resets_each_day() {
        let mut state = None;
        for _ in 0..3 {
            state = Some(spend_budget(state.as_deref(), NOW, 3).unwrap());
        }
        assert_eq!(
            spend_budget(state.as_deref(), NOW, 3).unwrap_err().code,
            PluginErrorCode::RateLimited
        );
        assert!(spend_budget(state.as_deref(), NOW + DAY_MS, 3).is_ok());
    }

    #[test]
    fn cached_answers_expire_after_a_day_and_the_cache_is_bounded() {
        let mut cache = AnswerCache::default();
        cache.put("tt1", None, NOW);
        let cache = AnswerCache::decode(Some(&cache.encode()));
        assert_eq!(cache.get("tt1", NOW + 1), Some(None));
        assert_eq!(cache.get("tt1", NOW + CACHE_TTL_MS), None);

        let mut cache = AnswerCache::default();
        for index in 0..(CACHE_MAX_ENTRIES as u64 + 10) {
            cache.put(&format!("tt{index}"), None, NOW + index);
        }
        assert_eq!(cache.entries.len(), CACHE_MAX_ENTRIES);
        assert_eq!(cache.get("tt0", NOW + 500), None);
    }

    #[test]
    fn daily_limit_falls_back_to_the_default() {
        assert_eq!(daily_limit(None), DEFAULT_DAILY_LIMIT);
        assert_eq!(daily_limit(Some("0")), DEFAULT_DAILY_LIMIT);
        assert_eq!(daily_limit(Some("250")), 250);
        assert_eq!(daily_limit(Some("250.0")), 250);
    }

    #[test]
    fn the_descriptor_is_a_movie_metadata_provider() {
        let descriptor = descriptor();
        let provider = descriptor.metadata_provider().unwrap();
        assert_eq!(provider.allowed_hosts, [HOST]);
        assert!(
            descriptor
                .settings
                .iter()
                .any(|setting| setting.field.key == SETTING_API_KEY && setting.sensitive)
        );
    }
}
