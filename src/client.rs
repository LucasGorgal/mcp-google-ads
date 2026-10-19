use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::auth;
use crate::config::Config;
use crate::error::{McpGoogleAdsError, Result};

/// Default Google Ads REST API base URL.
///
/// Overridable at construction via [`GoogleAdsClient::with_base_url`] or via
/// the `GOOGLE_ADS_API_BASE_URL` environment variable. The override is the
/// hook used by integration tests to point the client at a `wiremock` server.
pub const DEFAULT_BASE_URL: &str = "https://googleads.googleapis.com/v25";

/// Environment variable that, when set, overrides the API base URL.
///
/// Setting this to `__test__` activates a stubbed access token path that
/// bypasses the OAuth2 flow entirely — used by `wiremock`-driven tests so
/// they don't need credentials files.
pub const BASE_URL_ENV: &str = "GOOGLE_ADS_API_BASE_URL";

const LOGIN_CUSTOMER_ID_HEADER: &str = "login-customer-id";

/// Accounts this process reaches through their own login instead of the
/// configured manager: ones the OAuth user was invited to directly, not linked
/// under that manager. Keyed `"{base_url}|{customer_id}"` so wiremock-backed
/// tests never share entries.
///
/// Process-wide because a client is built per tool call; without it every call
/// to such an account would pay a refused request first.
fn direct_access_accounts() -> &'static Mutex<HashSet<String>> {
    static ACCOUNTS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    ACCOUNTS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Whitelist of top-level `MutateOperation` keys accepted by Google Ads v25.
///
/// `mutate_operations` payloads with any key not in this list are rejected
/// client-side before any HTTP traffic — this is the guard that catches
/// `dismissRecommendationOperation` / `applyRecommendationOperation` mistakes
/// (those operations live on dedicated RPCs, not on `googleAds:mutate`).
///
/// The feed-based extension operations (`feedOperation`, `campaignFeedOperation`,
/// `extensionFeedItemOperation`, the `*ExtensionSettingOperation` family, …) are
/// deliberately absent: Google removed them with the sunset of feed-based
/// extensions, and probing the live API with `validateOnly` answers "Unknown
/// name" for each on both v23 and v25. This server writes extensions through
/// `assetOperation` / `campaignAssetOperation` instead.
///
/// Source: Google Ads API v25 `MutateOperation.operation` oneof definition.
pub const VALID_MUTATE_OPERATION_KEYS: &[&str] = &[
    "adGroupAdLabelOperation",
    "adGroupAdOperation",
    "adGroupAssetOperation",
    "adGroupBidModifierOperation",
    "adGroupCriterionCustomizerOperation",
    "adGroupCriterionLabelOperation",
    "adGroupCriterionOperation",
    "adGroupCustomizerOperation",
    "adGroupLabelOperation",
    "adGroupOperation",
    "adOperation",
    "adParameterOperation",
    "assetGroupAssetOperation",
    "assetGroupListingGroupFilterOperation",
    "assetGroupOperation",
    "assetGroupSignalOperation",
    "assetOperation",
    "assetSetAssetOperation",
    "assetSetOperation",
    "audienceOperation",
    "biddingDataExclusionOperation",
    "biddingSeasonalityAdjustmentOperation",
    "biddingStrategyOperation",
    "campaignAssetOperation",
    "campaignAssetSetOperation",
    "campaignBidModifierOperation",
    "campaignBudgetOperation",
    "campaignConversionGoalOperation",
    "campaignCriterionOperation",
    "campaignCustomizerOperation",
    "campaignDraftOperation",
    "campaignGroupOperation",
    "campaignLabelOperation",
    "campaignOperation",
    "campaignSharedSetOperation",
    "conversionActionOperation",
    "conversionCustomVariableOperation",
    "conversionGoalCampaignConfigOperation",
    "conversionValueRuleOperation",
    "conversionValueRuleSetOperation",
    "customConversionGoalOperation",
    "customerAssetOperation",
    "customerConversionGoalOperation",
    "customerCustomizerOperation",
    "customerLabelOperation",
    "customerNegativeCriterionOperation",
    "customerOperation",
    "customizerAttributeOperation",
    "experimentArmOperation",
    "experimentOperation",
    "keywordPlanAdGroupKeywordOperation",
    "keywordPlanAdGroupOperation",
    "keywordPlanCampaignKeywordOperation",
    "keywordPlanCampaignOperation",
    "keywordPlanOperation",
    "labelOperation",
    "recommendationSubscriptionOperation",
    "remarketingActionOperation",
    "sharedCriterionOperation",
    "sharedSetOperation",
    "smartCampaignSettingOperation",
    "userListOperation",
];

/// Services written through their own `customers/{cid}/{service}:mutate` RPC
/// rather than a key on `googleAds:mutate`, and which [`GoogleAdsClient::service_mutate`]
/// may address.
///
/// `customAudiences` is here because a custom segment (`CustomAudience`) has no
/// `MutateOperation` key: it is the building block of Demand Gen and Video
/// audience targeting, and without it the only segments reachable are Google's
/// predefined ones.
pub const VALID_SERVICE_MUTATES: &[&str] = &["customAudiences"];

/// Check a service mutate before any HTTP traffic: a known service, at least
/// one operation, and each operation a single `create` / `update` / `remove`.
pub fn validate_service_mutate(
    service: &str,
    operations: &[serde_json::Value],
) -> std::result::Result<(), String> {
    if !VALID_SERVICE_MUTATES.contains(&service) {
        return Err(format!(
            "Unknown mutate service '{service}'. Allowed: {}. Everything else goes \
             through googleAds:mutate (leave `service` unset).",
            VALID_SERVICE_MUTATES.join(", ")
        ));
    }
    if operations.is_empty() {
        return Err("No operations to send".to_string());
    }
    for (idx, op) in operations.iter().enumerate() {
        let Some(obj) = op.as_object() else {
            return Err(format!("Operation at index {idx} is not a JSON object"));
        };
        let kinds: Vec<&str> = obj
            .keys()
            .map(String::as_str)
            .filter(|k| matches!(*k, "create" | "update" | "remove"))
            .collect();
        if kinds.len() != 1 {
            return Err(format!(
                "Operation at index {idx} must carry exactly one of create, update or remove"
            ));
        }
        if let Some(key) = obj
            .keys()
            .find(|k| !matches!(k.as_str(), "create" | "update" | "remove" | "updateMask"))
        {
            return Err(format!("Unknown key '{key}' in operation at index {idx}"));
        }
    }
    Ok(())
}

/// What `upload_youtube_video` sends: the local file and the YouTube metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct YouTubeVideoUploadRequest {
    pub file_path: String,
    pub video_title: String,
    pub video_description: String,
    /// `UNLISTED`, `PUBLIC` or `PRIVATE` (`YouTubeVideoPrivacy`).
    pub video_privacy: String,
    /// Brand channel to upload to; `None` = the Google-managed channel.
    pub channel_id: Option<String>,
}

/// Host prefix for media uploads: `https://googleads.googleapis.com/v25` becomes
/// `https://googleads.googleapis.com/resumable/upload/v25`.
pub fn resumable_upload_base(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some((host, version)) if version.starts_with('v') => {
            format!("{host}/resumable/upload/{version}")
        }
        _ => format!("{trimmed}/resumable/upload"),
    }
}

/// An answer from Google that is successful but not shaped as documented.
fn unexpected_response(message: String) -> McpGoogleAdsError {
    McpGoogleAdsError::GoogleAds {
        message,
        error_code: None,
        details: Vec::new(),
    }
}

/// The `apply_parameters` oneof on `ApplyRecommendationOperation`, in the
/// camelCase spelling the REST surface expects.
///
/// Several recommendation types cannot be applied from a bare `resourceName`:
/// the operation needs the parameters that say *what* to apply — which assets
/// to attach for `SITELINK_ASSET`, which amount for `CAMPAIGN_BUDGET`, and so
/// on. Sending the operation without them returns an error whose code Google
/// cannot even name back at us, so the payload is checked here rather than
/// spent on a round trip.
///
/// Source: Google Ads API v25 `ApplyRecommendationOperation.apply_parameters`
/// oneof. `AdAssetApplyParameters` is deliberately absent — it is the shared
/// inner type of the three asset variants, not a variant itself.
pub const APPLY_RECOMMENDATION_PARAMETER_KEYS: &[&str] = &[
    "callAsset",
    "callExtension",
    "calloutAsset",
    "calloutExtension",
    "campaignBudget",
    "forecastingSetTargetCpa",
    "forecastingSetTargetRoas",
    "keyword",
    "leadFormAsset",
    "lowerTargetRoas",
    "moveUnusedBudget",
    "raiseTargetCpa",
    "raiseTargetCpaBidTooLow",
    "responsiveSearchAd",
    "responsiveSearchAdAsset",
    "responsiveSearchAdImproveAdStrength",
    "sitelinkAsset",
    "sitelinkExtension",
    "targetCpaOptIn",
    "targetRoasOptIn",
    "textAd",
    "useBroadMatchKeyword",
];

/// An `apply_parameters` payload is bounded so a malformed caller cannot push
/// an unbounded blob at the API. Well above the largest documented variant,
/// which tops out at a handful of assets.
const MAX_APPLY_PARAMETERS_BYTES: usize = 64 * 1024;

/// Check an `apply_parameters` payload against the oneof contract: an object
/// carrying exactly one documented key.
///
/// `oneof` means exactly one — zero says nothing, and two would leave Google
/// to pick, which is not a decision to delegate silently.
pub fn validate_apply_parameters(parameters: &serde_json::Value) -> Result<()> {
    let Some(object) = parameters.as_object() else {
        return Err(McpGoogleAdsError::Validation(format!(
            "apply_parameters must be an object carrying exactly one of: {}",
            APPLY_RECOMMENDATION_PARAMETER_KEYS.join(", ")
        )));
    };

    match object.len() {
        1 => {}
        n => {
            return Err(McpGoogleAdsError::Validation(format!(
                "apply_parameters is a oneof and takes exactly one key, got {n}. Valid keys: {}",
                APPLY_RECOMMENDATION_PARAMETER_KEYS.join(", ")
            )));
        }
    }

    for key in object.keys() {
        if !APPLY_RECOMMENDATION_PARAMETER_KEYS.contains(&key.as_str()) {
            return Err(McpGoogleAdsError::Validation(format!(
                "`{key}` is not an apply_parameters key on ApplyRecommendationOperation. \
                 Valid keys: {}",
                APPLY_RECOMMENDATION_PARAMETER_KEYS.join(", ")
            )));
        }
    }

    let size = serde_json::to_string(parameters)
        .map(|s| s.len())
        .unwrap_or(0);
    if size > MAX_APPLY_PARAMETERS_BYTES {
        return Err(McpGoogleAdsError::Validation(format!(
            "apply_parameters is {size} bytes, over the {MAX_APPLY_PARAMETERS_BYTES} byte limit"
        )));
    }

    Ok(())
}

/// A single mutate operation for the Google Ads API.
#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MutateOperation {
    #[serde(flatten)]
    pub operation: serde_json::Value,
}

/// Response from a mutate request.
#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MutateResponse {
    #[serde(default)]
    pub mutate_operation_responses: Vec<serde_json::Value>,
    #[serde(default)]
    pub partial_failure_error: Option<serde_json::Value>,
}

/// Response from `recommendations:apply`.
#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ApplyRecommendationResponse {
    #[serde(default)]
    pub results: Vec<serde_json::Value>,
    #[serde(default)]
    pub partial_failure_error: Option<serde_json::Value>,
}

/// Response from `recommendations:dismiss`.
#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DismissRecommendationResponse {
    #[serde(default)]
    pub results: Vec<serde_json::Value>,
}

/// Google Ads REST API client.
pub struct GoogleAdsClient {
    http: reqwest::Client,
    config: Config,
    base_url: String,
}

impl GoogleAdsClient {
    /// Create a new Google Ads API client using the configured base URL.
    ///
    /// The base URL is resolved from the `GOOGLE_ADS_API_BASE_URL` env var if
    /// set, otherwise falls back to [`DEFAULT_BASE_URL`].
    pub fn new(config: &Config) -> Result<Self> {
        let base_url = std::env::var(BASE_URL_ENV).unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        Self::with_base_url(config, base_url)
    }

    /// Create a new client with an explicit base URL — used by tests to
    /// point at a `wiremock` server.
    pub fn with_base_url(config: &Config, base_url: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder().build().map_err(|e| {
            McpGoogleAdsError::Config(format!("Failed to build HTTP client: {}", e))
        })?;

        Ok(Self {
            http,
            config: config.clone(),
            base_url: base_url.into(),
        })
    }

    /// The base URL this client is targeting (for tests/diagnostics).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Strip dashes from a customer ID (e.g., "123-456-7890" -> "1234567890").
    pub fn normalize_customer_id(id: &str) -> String {
        id.replace('-', "")
    }

    /// Build common headers for a Google Ads API request on `customer_id`.
    ///
    /// `login-customer-id` is the configured manager, or the account itself
    /// once it is known to be reached directly (see [`Self::post`]).
    async fn build_headers(&self, customer_id: &str) -> Result<reqwest::header::HeaderMap> {
        let mut headers = self.auth_headers().await?;

        let login_customer_id = if self.is_direct_access(customer_id) {
            Some(Self::normalize_customer_id(customer_id))
        } else {
            self.config
                .ads
                .login_customer_id
                .as_deref()
                .map(Self::normalize_customer_id)
        };

        if let Some(login_customer_id) = login_customer_id {
            headers.insert(
                LOGIN_CUSTOMER_ID_HEADER,
                login_customer_id.parse().map_err(|e| {
                    McpGoogleAdsError::Config(format!("Invalid login customer ID: {}", e))
                })?,
            );
        }

        Ok(headers)
    }

    /// Authorization and developer-token headers, without `login-customer-id`.
    ///
    /// When the base URL points anywhere other than the real Google Ads host
    /// (e.g. a `wiremock` server) and no credentials file exists, this falls
    /// back to a static dummy bearer token so tests don't need an OAuth2 flow.
    async fn auth_headers(&self) -> Result<reqwest::header::HeaderMap> {
        let token = if self.is_test_base_url() && !self.config.google.credentials_path.exists() {
            "test-access-token".to_string()
        } else {
            auth::get_access_token(&self.config).await?
        };

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", token)
                .parse()
                .map_err(|e| McpGoogleAdsError::Auth(format!("Invalid token header: {}", e)))?,
        );
        let dev_token = if self.config.ads.developer_token.is_empty() {
            "test-developer-token".to_string()
        } else {
            self.config.ads.developer_token.clone()
        };
        headers.insert(
            "developer-token",
            dev_token.parse().map_err(|e| {
                McpGoogleAdsError::Config(format!("Invalid developer token: {}", e))
            })?,
        );

        Ok(headers)
    }

    /// Whether `customer_id` is reached through its own login rather than the
    /// configured manager (see [`Self::post`]).
    fn is_direct_access(&self, customer_id: &str) -> bool {
        let key = format!("{}|{}", self.base_url, Self::normalize_customer_id(customer_id));
        direct_access_accounts()
            .lock()
            .map(|accounts| accounts.contains(&key))
            .unwrap_or(false)
    }

    fn remember_direct_access(&self, customer_id: &str) {
        let key = format!("{}|{}", self.base_url, Self::normalize_customer_id(customer_id));
        if let Ok(mut accounts) = direct_access_accounts().lock() {
            accounts.insert(key);
        }
    }

    /// POST `body` to `url` for `customer_id`, returning the successful
    /// response or the parsed Google Ads error.
    ///
    /// An account the OAuth user was invited to directly, instead of linked
    /// under the configured manager, refuses the manager's `login-customer-id`
    /// with USER_PERMISSION_DENIED. The request is then retried once with the
    /// account's own ID as `login-customer-id`, and on success that route is
    /// remembered for the rest of the process and written back into `headers`
    /// (so the next pages of a search skip the refused attempt). A refused
    /// request wrote nothing, so retrying a mutate is safe.
    async fn post(
        &self,
        url: &str,
        customer_id: &str,
        headers: &mut reqwest::header::HeaderMap,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response> {
        let response = self
            .http
            .post(url)
            .headers(headers.clone())
            .json(body)
            .send()
            .await?;

        if response.status().is_success() {
            return Ok(response);
        }

        let status = response.status();
        let error_body = response.text().await.unwrap_or_default();

        let own_login = Self::normalize_customer_id(customer_id);
        let can_switch = match self.config.ads.login_customer_id.as_deref() {
            Some(manager) => {
                Self::normalize_customer_id(manager) != own_login
                    && !self.is_direct_access(customer_id)
            }
            None => false,
        };
        if !can_switch || !is_user_permission_denied(status, &error_body) {
            return Err(parse_google_ads_error(status, &error_body));
        }

        let mut direct_headers = headers.clone();
        direct_headers.insert(
            LOGIN_CUSTOMER_ID_HEADER,
            own_login.parse().map_err(|e| {
                McpGoogleAdsError::Config(format!("Invalid customer ID: {}", e))
            })?,
        );

        let retry = self
            .http
            .post(url)
            .headers(direct_headers.clone())
            .json(body)
            .send()
            .await?;

        if retry.status().is_success() {
            self.remember_direct_access(customer_id);
            *headers = direct_headers;
            return Ok(retry);
        }

        let retry_status = retry.status();
        let retry_body = retry.text().await.unwrap_or_default();
        if is_user_permission_denied(retry_status, &retry_body) {
            // Neither login reaches the account; the manager route's error
            // is the one that explains it.
            Err(parse_google_ads_error(status, &error_body))
        } else {
            // The account's own login got through; the request itself failed.
            Err(parse_google_ads_error(retry_status, &retry_body))
        }
    }

    /// IDs of every account the OAuth user can open on their own login,
    /// direct invitations included (`customers:listAccessibleCustomers`).
    ///
    /// A manager's `customer_client` listing never shows an account the user
    /// was invited to directly, so this is the only way to discover one.
    pub async fn list_accessible_customers(&self) -> Result<Vec<String>> {
        let url = format!("{}/customers:listAccessibleCustomers", self.base_url);
        let headers = self.auth_headers().await?;

        let response = self.http.get(&url).headers(headers).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            return Err(parse_google_ads_error(status, &error_body));
        }

        let response_json: serde_json::Value = response.json().await?;
        let ids = response_json
            .get("resourceNames")
            .and_then(|r| r.as_array())
            .map(|names| {
                names
                    .iter()
                    .filter_map(|n| n.as_str())
                    .map(|n| n.trim_start_matches("customers/").to_string())
                    .collect()
            })
            .unwrap_or_default();
        Ok(ids)
    }

    fn is_test_base_url(&self) -> bool {
        !self
            .base_url
            .starts_with("https://googleads.googleapis.com")
    }

    /// Execute a GAQL query via the Google Ads search endpoint.
    /// Handles pagination automatically.
    pub async fn search(&self, customer_id: &str, query: &str) -> Result<Vec<serde_json::Value>> {
        let normalized_id = Self::normalize_customer_id(customer_id);
        let url = format!(
            "{}/customers/{}/googleAds:search",
            self.base_url, normalized_id
        );
        let mut headers = self.build_headers(&normalized_id).await?;

        let mut all_results = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut body = serde_json::json!({
                "query": query,
            });

            if let Some(ref token) = page_token {
                body.as_object_mut()
                    .ok_or_else(|| {
                        McpGoogleAdsError::Json(serde_json::Error::io(std::io::Error::other(
                            "Failed to build request body",
                        )))
                    })?
                    .insert(
                        "pageToken".to_string(),
                        serde_json::Value::String(token.clone()),
                    );
            }

            let response = self
                .post(&url, &normalized_id, &mut headers, &body)
                .await?;

            let response_json: serde_json::Value = response.json().await?;

            if let Some(results) = response_json.get("results").and_then(|r| r.as_array()) {
                all_results.extend(results.iter().cloned());
            }

            match response_json.get("nextPageToken").and_then(|t| t.as_str()) {
                Some(next_token) => {
                    page_token = Some(next_token.to_string());
                }
                None => break,
            }
        }

        Ok(all_results)
    }

    /// Call the Keyword Planner generateKeywordIdeas endpoint.
    ///
    /// `language_id` is a bare language constant ID (`1014` = Portuguese) and
    /// `geo_target_ids` are bare geo target constant IDs (`2076` = Brazil); an
    /// empty geo list means worldwide volumes.
    pub async fn generate_keyword_ideas(
        &self,
        customer_id: &str,
        seed_keywords: Vec<String>,
        language_id: &str,
        geo_target_ids: &[String],
        page_size: Option<u32>,
    ) -> Result<Vec<serde_json::Value>> {
        let normalized_id = Self::normalize_customer_id(customer_id);
        let url = format!(
            "{}/customers/{}:generateKeywordIdeas",
            self.base_url, normalized_id
        );

        let body = serde_json::json!({
            "keywordSeed": {
                "keywords": seed_keywords
            },
            "language": format!("languageConstants/{}", language_id),
            "geoTargetConstants": Self::geo_target_resource_names(geo_target_ids),
            "pageSize": page_size.unwrap_or(50),
            "keywordPlanNetwork": "GOOGLE_SEARCH"
        });

        self.post_keyword_plan_request(&url, &normalized_id, &body)
            .await
    }

    /// Call the Keyword Planner generateKeywordHistoricalMetrics endpoint.
    ///
    /// Unlike a `keyword_view` report this reads Google's own keyword database,
    /// so it returns volumes for keywords that have never run in the account.
    pub async fn generate_keyword_historical_metrics(
        &self,
        customer_id: &str,
        keywords: Vec<String>,
        language_id: &str,
        geo_target_ids: &[String],
    ) -> Result<Vec<serde_json::Value>> {
        let normalized_id = Self::normalize_customer_id(customer_id);
        let url = format!(
            "{}/customers/{}:generateKeywordHistoricalMetrics",
            self.base_url, normalized_id
        );

        let body = serde_json::json!({
            "keywords": keywords,
            "language": format!("languageConstants/{}", language_id),
            "geoTargetConstants": Self::geo_target_resource_names(geo_target_ids),
            "keywordPlanNetwork": "GOOGLE_SEARCH"
        });

        self.post_keyword_plan_request(&url, &normalized_id, &body)
            .await
    }

    fn geo_target_resource_names(geo_target_ids: &[String]) -> Vec<String> {
        geo_target_ids
            .iter()
            .map(|id| format!("geoTargetConstants/{}", id))
            .collect()
    }

    /// POST a Keyword Planner request and return its `results` array.
    async fn post_keyword_plan_request(
        &self,
        url: &str,
        customer_id: &str,
        body: &serde_json::Value,
    ) -> Result<Vec<serde_json::Value>> {
        let mut headers = self.build_headers(customer_id).await?;

        let response = self.post(url, customer_id, &mut headers, body).await?;

        let response_json: serde_json::Value = response.json().await?;
        let results = response_json
            .get("results")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(results)
    }

    /// Validate that every operation in `operations` uses a top-level key
    /// from [`VALID_MUTATE_OPERATION_KEYS`]. Returns the first offending key.
    pub fn validate_mutate_operations(
        operations: &[MutateOperation],
    ) -> std::result::Result<(), String> {
        for (idx, op) in operations.iter().enumerate() {
            let obj = match op.operation.as_object() {
                Some(o) => o,
                None => {
                    return Err(format!(
                        "MutateOperation at index {} is not a JSON object",
                        idx
                    ));
                }
            };
            for key in obj.keys() {
                if !VALID_MUTATE_OPERATION_KEYS.contains(&key.as_str()) {
                    return Err(format!(
                        "Unknown MutateOperation key '{}' at index {}. Recommendation operations \
                         must use apply_recommendations / dismiss_recommendations — they are NOT \
                         valid keys on googleAds:mutate in v25.",
                        key, idx
                    ));
                }
            }
        }
        Ok(())
    }

    /// Execute a mutate request against the Google Ads API.
    ///
    /// Rejects unknown top-level operation keys client-side before any HTTP
    /// traffic (see [`Self::validate_mutate_operations`]).
    pub async fn mutate(
        &self,
        customer_id: &str,
        operations: Vec<MutateOperation>,
    ) -> Result<MutateResponse> {
        self.mutate_inner(customer_id, operations, false).await
    }

    /// Send a mutate with `validateOnly: true`: Google checks every operation
    /// exactly as it would for real (field values, policy, references between
    /// temporary IDs) and writes nothing.
    pub async fn mutate_validate_only(
        &self,
        customer_id: &str,
        operations: Vec<MutateOperation>,
    ) -> Result<MutateResponse> {
        self.mutate_inner(customer_id, operations, true).await
    }

    async fn mutate_inner(
        &self,
        customer_id: &str,
        operations: Vec<MutateOperation>,
        validate_only: bool,
    ) -> Result<MutateResponse> {
        Self::validate_mutate_operations(&operations).map_err(McpGoogleAdsError::Validation)?;

        let normalized_id = Self::normalize_customer_id(customer_id);
        let url = format!(
            "{}/customers/{}/googleAds:mutate",
            self.base_url, normalized_id
        );
        let mut headers = self.build_headers(&normalized_id).await?;

        let body = serde_json::json!({
            "mutateOperations": operations,
            // Atomic on purpose: with partialFailure=true Google COMMITS the
            // operations that succeed and only reports errors for the rest, so a
            // failed multi-operation plan (e.g. draft_campaign's budget + campaign
            // + ad group chain) leaves orphans behind — and since we surface any
            // partial failure as a full error and keep the plan for retry, each
            // retry would commit another orphan. With partialFailure=false any
            // failing operation aborts the whole request and nothing is written.
            "partialFailure": false,
            "validateOnly": validate_only,
        });

        let response = self
            .post(&url, &normalized_id, &mut headers, &body)
            .await?;

        let mutate_response: MutateResponse = response.json().await?;
        Ok(mutate_response)
    }

    /// Mutate through a service that has its own `:mutate` RPC instead of a
    /// key on `googleAds:mutate` (e.g. `customAudiences`).
    ///
    /// `service` must be one of [`VALID_SERVICE_MUTATES`]; `operations` are the
    /// service's own operation objects (`{"create": {...}}`, `{"update": ...,
    /// "updateMask": ...}`, `{"remove": "..."}`). Atomic, like [`Self::mutate`].
    pub async fn service_mutate(
        &self,
        customer_id: &str,
        service: &str,
        operations: &[serde_json::Value],
        validate_only: bool,
    ) -> Result<serde_json::Value> {
        validate_service_mutate(service, operations).map_err(McpGoogleAdsError::Validation)?;

        let normalized_id = Self::normalize_customer_id(customer_id);
        let url = format!(
            "{}/customers/{}/{}:mutate",
            self.base_url, normalized_id, service
        );
        let mut headers = self.build_headers(&normalized_id).await?;

        // No `partialFailure` here: the service RPCs (MutateCustomAudiencesRequest)
        // have no such field, and Google answers 400 "Unknown name
        // \"partialFailure\"". They are all-or-nothing anyway.
        let body = serde_json::json!({
            "operations": operations,
            "validateOnly": validate_only,
        });

        let response = self
            .post(&url, &normalized_id, &mut headers, &body)
            .await?;

        // A validate-only call answers with an empty body.
        let text = response.text().await?;
        if text.trim().is_empty() {
            return Ok(serde_json::json!({}));
        }
        serde_json::from_str(&text).map_err(|e| {
            unexpected_response(format!("Unreadable {service}:mutate response: {e}"))
        })
    }

    /// Upload a local video to YouTube through the Google Ads API
    /// (`YouTubeVideoUploadService`, v23+), using the resumable protocol.
    ///
    /// Without `channel_id` the video lands on the Google-managed channel linked
    /// to the account, where UNLISTED is the only privacy allowed. The returned
    /// `resourceName` is a `you_tube_video_upload`; its `video_id` only appears
    /// once the upload reaches `PROCESSED` (poll it with GAQL).
    pub async fn upload_youtube_video(
        &self,
        customer_id: &str,
        upload: &YouTubeVideoUploadRequest,
    ) -> Result<serde_json::Value> {
        let bytes = tokio::fs::read(&upload.file_path).await.map_err(|e| {
            McpGoogleAdsError::Validation(format!(
                "Cannot read video file '{}': {e}",
                upload.file_path
            ))
        })?;
        if bytes.is_empty() {
            return Err(McpGoogleAdsError::Validation(format!(
                "Video file '{}' is empty",
                upload.file_path
            )));
        }

        let normalized_id = Self::normalize_customer_id(customer_id);
        let url = format!(
            "{}/customers/{}/youTubeVideoUploads:create",
            resumable_upload_base(&self.base_url),
            normalized_id
        );

        let mut video = serde_json::json!({
            "videoTitle": upload.video_title,
            "videoDescription": upload.video_description,
            "videoPrivacy": upload.video_privacy,
        });
        if let Some(channel_id) = upload.channel_id.as_deref() {
            video["channelId"] = serde_json::json!(channel_id);
        }
        let body = serde_json::json!({
            "customerId": normalized_id,
            "youTubeVideoUpload": video,
        });

        let mut headers = self.build_headers(&normalized_id).await?;
        for (name, value) in [
            ("x-goog-upload-protocol", "resumable".to_string()),
            ("x-goog-upload-command", "start".to_string()),
            ("x-goog-upload-header-content-length", bytes.len().to_string()),
        ] {
            headers.insert(
                name,
                value.parse().map_err(|e| {
                    McpGoogleAdsError::Config(format!("Invalid {name} header: {e}"))
                })?,
            );
        }

        let start = self
            .post(&url, &normalized_id, &mut headers, &body)
            .await?;
        let upload_url = start
            .headers()
            .get("x-goog-upload-url")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| {
                unexpected_response(
                    "youTubeVideoUploads:create answered without an x-goog-upload-url header"
                        .to_string(),
                )
            })?;

        let mut put_headers = self.build_headers(&normalized_id).await?;
        if let Some(login) = headers.get(LOGIN_CUSTOMER_ID_HEADER) {
            // Keep the login that the start request settled on.
            put_headers.insert(LOGIN_CUSTOMER_ID_HEADER, login.clone());
        }
        put_headers.insert("x-goog-upload-offset", reqwest::header::HeaderValue::from_static("0"));
        put_headers.insert(
            "x-goog-upload-command",
            reqwest::header::HeaderValue::from_static("upload, finalize"),
        );

        let response = self
            .http
            .put(&upload_url)
            .headers(put_headers)
            .body(bytes)
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(parse_google_ads_error(status, &text));
        }
        serde_json::from_str(&text).map_err(|e| {
            unexpected_response(format!("Unreadable video upload response: {e}: {text}"))
        })
    }

    /// Apply recommendations via the dedicated `recommendations:apply` RPC.
    ///
    /// `resource_names` must be full resource paths:
    /// `customers/{cid}/recommendations/{recommendation_id}`.
    pub async fn apply_recommendations(
        &self,
        customer_id: &str,
        resource_names: Vec<String>,
        apply_parameters: Option<serde_json::Value>,
    ) -> Result<ApplyRecommendationResponse> {
        if let Some(ref parameters) = apply_parameters {
            validate_apply_parameters(parameters)?;
        }

        let normalized_id = Self::normalize_customer_id(customer_id);
        let url = format!(
            "{}/customers/{}/recommendations:apply",
            self.base_url, normalized_id
        );
        let mut headers = self.build_headers(&normalized_id).await?;

        let operations: Vec<serde_json::Value> = resource_names
            .iter()
            .map(|rn| {
                let mut operation = serde_json::json!({ "resourceName": rn });
                // The parameters sit beside `resourceName` on the operation, not
                // under a wrapper — `apply_parameters` is a oneof on
                // ApplyRecommendationOperation itself.
                if let (Some(target), Some(source)) = (
                    operation.as_object_mut(),
                    apply_parameters.as_ref().and_then(|p| p.as_object()),
                ) {
                    for (key, value) in source {
                        target.insert(key.clone(), value.clone());
                    }
                }
                operation
            })
            .collect();

        let body = serde_json::json!({
            "operations": operations,
            "partialFailure": true,
        });

        let response = self
            .post(&url, &normalized_id, &mut headers, &body)
            .await?;

        let parsed: ApplyRecommendationResponse = response.json().await?;
        Ok(parsed)
    }

    /// Dismiss recommendations via the dedicated `recommendations:dismiss` RPC.
    pub async fn dismiss_recommendations(
        &self,
        customer_id: &str,
        resource_names: Vec<String>,
    ) -> Result<DismissRecommendationResponse> {
        let normalized_id = Self::normalize_customer_id(customer_id);
        let url = format!(
            "{}/customers/{}/recommendations:dismiss",
            self.base_url, normalized_id
        );
        let mut headers = self.build_headers(&normalized_id).await?;

        let operations: Vec<serde_json::Value> = resource_names
            .iter()
            .map(|rn| serde_json::json!({ "resourceName": rn }))
            .collect();

        let body = serde_json::json!({
            "operations": operations,
        });

        let response = self
            .post(&url, &normalized_id, &mut headers, &body)
            .await?;

        let parsed: DismissRecommendationResponse = response.json().await?;
        Ok(parsed)
    }
}

/// The refusal Google gives when the `login-customer-id` cannot reach the
/// account: no manager link from that login, or no access at all.
fn is_user_permission_denied(status: reqwest::StatusCode, body: &str) -> bool {
    status == reqwest::StatusCode::FORBIDDEN && body.contains("USER_PERMISSION_DENIED")
}

/// Parse a Google Ads API error response into a McpGoogleAdsError.
fn parse_google_ads_error(status: reqwest::StatusCode, body: &str) -> McpGoogleAdsError {
    let parsed: std::result::Result<serde_json::Value, _> = serde_json::from_str(body);

    match parsed {
        Ok(json) => {
            let error_obj = json.get("error");

            let message = error_obj
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("Unknown Google Ads API error")
                .to_string();

            let error_code = error_obj
                .and_then(|e| e.get("status"))
                .and_then(|s| s.as_str())
                .map(|s| s.to_string());

            let details = error_obj
                .and_then(|e| e.get("details"))
                .and_then(|d| d.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|d| serde_json::to_string(d).ok())
                        .collect()
                })
                .unwrap_or_default();

            McpGoogleAdsError::GoogleAds {
                message: format!("[{}] {}", status, message),
                error_code,
                details,
            }
        }
        Err(_) => McpGoogleAdsError::GoogleAds {
            message: format!("[{}] {}", status, body),
            error_code: None,
            details: Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_customer_id() {
        assert_eq!(
            GoogleAdsClient::normalize_customer_id("123-456-7890"),
            "1234567890"
        );
        assert_eq!(
            GoogleAdsClient::normalize_customer_id("1234567890"),
            "1234567890"
        );
    }

    #[test]
    fn test_parse_google_ads_error_json() {
        let body = r#"{"error":{"message":"Request had invalid authentication credentials.","status":"UNAUTHENTICATED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo"}]}}"#;
        let err = parse_google_ads_error(reqwest::StatusCode::UNAUTHORIZED, body);
        match err {
            McpGoogleAdsError::GoogleAds {
                message,
                error_code,
                details,
            } => {
                assert!(message.contains("invalid authentication"));
                assert_eq!(error_code, Some("UNAUTHENTICATED".to_string()));
                assert_eq!(details.len(), 1);
            }
            _ => panic!("Expected GoogleAds error"),
        }
    }

    #[test]
    fn test_parse_google_ads_error_plain() {
        let err = parse_google_ads_error(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "something broke",
        );
        match err {
            McpGoogleAdsError::GoogleAds {
                message,
                error_code,
                details,
            } => {
                assert!(message.contains("something broke"));
                assert!(error_code.is_none());
                assert!(details.is_empty());
            }
            _ => panic!("Expected GoogleAds error"),
        }
    }

    #[test]
    fn test_validate_mutate_operations_accepts_known_keys() {
        let ops = vec![
            MutateOperation {
                operation: serde_json::json!({"adGroupOperation": {"create": {}}}),
            },
            MutateOperation {
                operation: serde_json::json!({"campaignBudgetOperation": {"create": {}}}),
            },
        ];
        assert!(GoogleAdsClient::validate_mutate_operations(&ops).is_ok());
    }

    #[test]
    fn test_validate_mutate_operations_rejects_recommendation_ops() {
        let ops = vec![MutateOperation {
            operation: serde_json::json!({"dismissRecommendationOperation": {"resourceName": "x"}}),
        }];
        let err = GoogleAdsClient::validate_mutate_operations(&ops).unwrap_err();
        assert!(err.contains("dismissRecommendationOperation"));
        assert!(err.contains("apply_recommendations / dismiss_recommendations"));
    }

    #[test]
    fn test_validate_mutate_operations_rejects_apply_recommendation() {
        let ops = vec![MutateOperation {
            operation: serde_json::json!({"applyRecommendationOperation": {"resourceName": "x"}}),
        }];
        assert!(GoogleAdsClient::validate_mutate_operations(&ops).is_err());
    }

    #[test]
    fn test_with_base_url() {
        let cfg = Config::default();
        let client = GoogleAdsClient::with_base_url(&cfg, "http://localhost:9999/v25").unwrap();
        assert_eq!(client.base_url(), "http://localhost:9999/v25");
    }
}
