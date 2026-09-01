use crate::client::GoogleAdsClient;
use crate::error::{McpGoogleAdsError, Result};

/// Google Ads language constant IDs keyed by ISO code, as returned by
/// `SELECT language_constant.id, language_constant.code FROM language_constant
/// WHERE language_constant.targetable = true`.
///
/// Keyword Planner matches ideas against this language: asking for Portuguese
/// seeds under the English constant returns the seed keyword and nothing else,
/// so getting this right is the difference between 1 result and a full
/// expansion set.
pub const LANGUAGE_CODES: &[(&str, &str)] = &[
    ("ar", "1019"),
    ("bg", "1020"),
    ("bn", "1056"),
    ("ca", "1038"),
    ("cs", "1021"),
    ("da", "1009"),
    ("de", "1001"),
    ("el", "1022"),
    ("en", "1000"),
    ("es", "1003"),
    ("et", "1043"),
    ("fa", "1064"),
    ("fi", "1011"),
    ("fr", "1002"),
    ("gu", "1072"),
    ("he", "1027"),
    ("hi", "1023"),
    ("hr", "1039"),
    ("hu", "1024"),
    ("id", "1025"),
    ("is", "1026"),
    ("it", "1004"),
    ("iw", "1027"),
    ("ja", "1005"),
    ("kn", "1086"),
    ("ko", "1012"),
    ("lt", "1029"),
    ("lv", "1028"),
    ("ml", "1098"),
    ("mr", "1101"),
    ("ms", "1102"),
    ("nl", "1010"),
    ("no", "1013"),
    ("pa", "1110"),
    ("pl", "1030"),
    ("pt", "1014"),
    ("ro", "1032"),
    ("ru", "1031"),
    ("sk", "1033"),
    ("sl", "1034"),
    ("sr", "1035"),
    ("sv", "1015"),
    ("ta", "1130"),
    ("te", "1131"),
    ("th", "1044"),
    ("tl", "1042"),
    ("tr", "1037"),
    ("uk", "1036"),
    ("ur", "1041"),
    ("vi", "1040"),
    ("zh_cn", "1017"),
    ("zh_tw", "1018"),
];

/// Resolve a language code (`pt`, `pt-BR`, `zh_CN`) or a raw language constant
/// ID (`1014`) to the numeric constant ID Keyword Planner expects.
pub fn resolve_language_id(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "Keyword Planner language is empty. Pass an ISO code (e.g. \"pt\") or a \
             language constant ID (e.g. \"1014\")."
                .to_string(),
        ));
    }

    // Raw constant ID, with or without the resource-name prefix.
    let bare = trimmed
        .strip_prefix("languageConstants/")
        .unwrap_or(trimmed);
    if bare.chars().all(|c| c.is_ascii_digit()) {
        return Ok(bare.to_string());
    }

    let normalized = bare.to_lowercase().replace('-', "_");
    if let Some((_, id)) = LANGUAGE_CODES.iter().find(|(code, _)| *code == normalized) {
        return Ok((*id).to_string());
    }

    // Regional variants ("pt-BR", "es-419") fall back to the base language.
    if let Some(base) = normalized.split('_').next() {
        if let Some((_, id)) = LANGUAGE_CODES.iter().find(|(code, _)| *code == base) {
            return Ok((*id).to_string());
        }
    }

    Err(McpGoogleAdsError::Validation(format!(
        "Unknown Keyword Planner language \"{}\". Pass an ISO code ({}) or a numeric \
         language constant ID.",
        trimmed,
        LANGUAGE_CODES
            .iter()
            .map(|(code, _)| *code)
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// Normalize a geo target to a bare constant ID, accepting either `2076` or
/// `geoTargetConstants/2076`.
fn normalize_geo_target(input: &str) -> Result<String> {
    let trimmed = input.trim();
    let bare = trimmed
        .strip_prefix("geoTargetConstants/")
        .unwrap_or(trimmed);
    if bare.is_empty() || !bare.chars().all(|c| c.is_ascii_digit()) {
        return Err(McpGoogleAdsError::Validation(format!(
            "Invalid geo target \"{}\". Use a numeric geo target constant ID \
             (e.g. \"2076\" for Brazil) — search_geo_targets returns these.",
            input
        )));
    }
    Ok(bare.to_string())
}

/// Resolve the language + geo targeting shared by both Keyword Planner tools.
fn resolve_targeting(language: &str, geo_target_ids: &[String]) -> Result<(String, Vec<String>)> {
    let language_id = resolve_language_id(language)?;
    let geo_targets = geo_target_ids
        .iter()
        .map(|g| normalize_geo_target(g))
        .collect::<Result<Vec<_>>>()?;
    Ok((language_id, geo_targets))
}

fn micros_to_currency(value: Option<&serde_json::Value>) -> Option<f64> {
    let raw = value?;
    let micros = raw
        .as_str()
        .and_then(|s| s.parse::<i64>().ok())
        .or_else(|| raw.as_i64())?;
    Some(micros as f64 / 1_000_000.0)
}

/// Google returns 64-bit metric fields as JSON strings; accept both forms.
fn as_number(value: Option<&serde_json::Value>) -> Option<i64> {
    let raw = value?;
    raw.as_str()
        .and_then(|s| s.parse::<i64>().ok())
        .or_else(|| raw.as_i64())
}

/// Flatten one Keyword Planner metrics block into a compact row.
///
/// The raw response carries twelve `monthlySearchVolumes` entries per keyword —
/// roughly 700 bytes of noise per idea — so the trend is only included when the
/// caller asks for it.
fn compact_row(
    text: &str,
    metrics: Option<&serde_json::Value>,
    include_trend: bool,
) -> serde_json::Value {
    let mut row = serde_json::json!({ "keyword": text });
    let Some(obj) = row.as_object_mut() else {
        return row;
    };

    // Google omits the metrics block entirely for keywords it has no data on.
    let Some(metrics) = metrics else {
        obj.insert("avg_monthly_searches".to_string(), serde_json::Value::Null);
        obj.insert(
            "note".to_string(),
            serde_json::Value::String("No Keyword Planner data for this keyword.".to_string()),
        );
        return row;
    };

    obj.insert(
        "avg_monthly_searches".to_string(),
        as_number(metrics.get("avgMonthlySearches"))
            .map(serde_json::Value::from)
            .unwrap_or(serde_json::Value::Null),
    );
    obj.insert(
        "competition".to_string(),
        metrics
            .get("competition")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    );
    obj.insert(
        "competition_index".to_string(),
        as_number(metrics.get("competitionIndex"))
            .map(serde_json::Value::from)
            .unwrap_or(serde_json::Value::Null),
    );
    obj.insert(
        "low_top_of_page_bid".to_string(),
        micros_to_currency(metrics.get("lowTopOfPageBidMicros"))
            .map(serde_json::Value::from)
            .unwrap_or(serde_json::Value::Null),
    );
    obj.insert(
        "high_top_of_page_bid".to_string(),
        micros_to_currency(metrics.get("highTopOfPageBidMicros"))
            .map(serde_json::Value::from)
            .unwrap_or(serde_json::Value::Null),
    );

    if include_trend {
        let trend: Vec<serde_json::Value> = metrics
            .get("monthlySearchVolumes")
            .and_then(|v| v.as_array())
            .map(|months| {
                months
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "month": m.get("month").and_then(|v| v.as_str()).unwrap_or(""),
                            "year": m.get("year").and_then(|v| v.as_str()).unwrap_or(""),
                            "searches": as_number(m.get("monthlySearches")),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        obj.insert("monthly_trend".to_string(), serde_json::Value::Array(trend));
    }

    row
}

/// Discover keyword ideas from seed keywords using the Keyword Planner API.
pub async fn discover_keywords(
    client: &GoogleAdsClient,
    customer_id: &str,
    seed_keywords: Vec<String>,
    language: &str,
    geo_target_ids: &[String],
    page_size: Option<u32>,
    include_monthly_trend: bool,
) -> Result<String> {
    if seed_keywords.is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "seed_keywords is empty — pass at least one seed keyword.".to_string(),
        ));
    }

    let (language_id, geo_targets) = resolve_targeting(language, geo_target_ids)?;

    let results = client
        .generate_keyword_ideas(
            customer_id,
            seed_keywords,
            &language_id,
            &geo_targets,
            page_size,
        )
        .await?;

    let ideas: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            compact_row(
                r.get("text").and_then(|t| t.as_str()).unwrap_or(""),
                r.get("keywordIdeaMetrics"),
                include_monthly_trend,
            )
        })
        .collect();

    let result = serde_json::json!({
        "keyword_ideas": ideas,
        "total_count": ideas.len(),
        "language": language_id,
        "geo_targets": geo_targets,
        "note": if geo_targets.is_empty() {
            "Volumes are worldwide for this language. Pass geo_target_ids (or set \
             GOOGLE_ADS_KEYWORD_PLAN_GEO_TARGETS) to scope them to a country or region."
        } else {
            "Volumes are scoped to the requested geo targets."
        },
    });

    serde_json::to_string_pretty(&result).map_err(Into::into)
}

/// Get historical Keyword Planner metrics — search volume, competition and
/// top-of-page bid range — for a specific list of keywords.
///
/// This reads Google's own keyword database, so it works for keywords that have
/// never run in the account.
pub async fn get_keyword_forecasts(
    client: &GoogleAdsClient,
    customer_id: &str,
    keyword_texts: Vec<String>,
    language: &str,
    geo_target_ids: &[String],
    include_monthly_trend: bool,
) -> Result<String> {
    if keyword_texts.is_empty() {
        let result = serde_json::json!({
            "keyword_forecasts": [],
            "total_count": 0,
            "message": "No keywords provided."
        });
        return serde_json::to_string_pretty(&result).map_err(Into::into);
    }

    let (language_id, geo_targets) = resolve_targeting(language, geo_target_ids)?;

    let results = client
        .generate_keyword_historical_metrics(customer_id, keyword_texts, &language_id, &geo_targets)
        .await?;

    let rows: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            compact_row(
                r.get("text").and_then(|t| t.as_str()).unwrap_or(""),
                r.get("keywordMetrics"),
                include_monthly_trend,
            )
        })
        .collect();

    let result = serde_json::json!({
        "keyword_forecasts": rows,
        "total_count": rows.len(),
        "language": language_id,
        "geo_targets": geo_targets,
    });

    serde_json::to_string_pretty(&result).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_iso_codes() {
        assert_eq!(resolve_language_id("pt").unwrap(), "1014");
        assert_eq!(resolve_language_id("PT").unwrap(), "1014");
        assert_eq!(resolve_language_id("en").unwrap(), "1000");
        assert_eq!(resolve_language_id("zh_CN").unwrap(), "1017");
        assert_eq!(resolve_language_id("zh-TW").unwrap(), "1018");
    }

    #[test]
    fn resolves_regional_variants_to_base_language() {
        assert_eq!(resolve_language_id("pt-BR").unwrap(), "1014");
        assert_eq!(resolve_language_id("es-419").unwrap(), "1003");
    }

    #[test]
    fn passes_through_raw_constant_ids() {
        assert_eq!(resolve_language_id("1014").unwrap(), "1014");
        assert_eq!(
            resolve_language_id("languageConstants/1014").unwrap(),
            "1014"
        );
    }

    #[test]
    fn rejects_unknown_languages() {
        assert!(resolve_language_id("klingon").is_err());
        assert!(resolve_language_id("").is_err());
    }

    #[test]
    fn normalizes_geo_targets() {
        assert_eq!(normalize_geo_target("2076").unwrap(), "2076");
        assert_eq!(
            normalize_geo_target("geoTargetConstants/2076").unwrap(),
            "2076"
        );
        assert!(normalize_geo_target("Brazil").is_err());
    }

    #[test]
    fn compacts_metrics_and_converts_micros() {
        let metrics = serde_json::json!({
            "avgMonthlySearches": "8100",
            "competition": "HIGH",
            "competitionIndex": "90",
            "lowTopOfPageBidMicros": "1916560",
            "highTopOfPageBidMicros": "6941470",
            "monthlySearchVolumes": [
                {"month": "AUGUST", "year": "2025", "monthlySearches": "5400"}
            ]
        });

        let row = compact_row("curso enem", Some(&metrics), false);
        assert_eq!(row["keyword"], "curso enem");
        assert_eq!(row["avg_monthly_searches"], 8100);
        assert_eq!(row["competition"], "HIGH");
        assert_eq!(row["low_top_of_page_bid"], 1.91656);
        assert!(row.get("monthly_trend").is_none());

        let with_trend = compact_row("curso enem", Some(&metrics), true);
        assert_eq!(with_trend["monthly_trend"][0]["searches"], 5400);
    }

    #[test]
    fn compacts_keywords_without_metrics() {
        let row = compact_row("no data", None, false);
        assert!(row["avg_monthly_searches"].is_null());
        assert!(row["note"].is_string());
    }
}
