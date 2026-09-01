use serde_json::json;

use crate::client::GoogleAdsClient;
use crate::config::Config;
use crate::error::{McpGoogleAdsError, Result};
use crate::gaql;
use crate::models::ProximityRadiusUnits;
use crate::safety::guards::check_blocked_operation;
use crate::safety::preview::{store_plan, ChangePlan};

/// Maximum number of digits accepted for a geo target constant ID.
/// Google geo target constant IDs are short numeric identifiers; anything
/// longer is a malformed input rather than a real location.
const MAX_GEO_TARGET_ID_LEN: usize = 12;

/// Strip diacritics from a search term, mapping accented Latin letters to their
/// ASCII base (`São` -> `Sao`, `Ceará` -> `Ceara`, `Zürich` -> `Zurich`).
///
/// Google stores every `geo_target_constant.name` unaccented — verified live:
/// `LIKE '%ã%'` and `LIKE '%é%'` both match zero rows across the whole table —
/// so an accented search term can never match anything. Since the stored names
/// are pure ASCII this is a safe one-way normalization; there is no accented
/// row that stripping could hide.
fn deaccent(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => 'a',
            'Á' | 'À' | 'Â' | 'Ã' | 'Ä' | 'Å' | 'Ā' => 'A',
            'é' | 'è' | 'ê' | 'ë' | 'ē' => 'e',
            'É' | 'È' | 'Ê' | 'Ë' | 'Ē' => 'E',
            'í' | 'ì' | 'î' | 'ï' | 'ī' => 'i',
            'Í' | 'Ì' | 'Î' | 'Ï' | 'Ī' => 'I',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' => 'o',
            'Ó' | 'Ò' | 'Ô' | 'Õ' | 'Ö' | 'Ø' | 'Ō' => 'O',
            'ú' | 'ù' | 'û' | 'ü' | 'ū' => 'u',
            'Ú' | 'Ù' | 'Û' | 'Ü' | 'Ū' => 'U',
            'ç' => 'c',
            'Ç' => 'C',
            'ñ' => 'n',
            'Ñ' => 'N',
            'ý' | 'ÿ' => 'y',
            'Ý' => 'Y',
            other => other,
        })
        .collect()
}

/// Search for geo target constants by name.
///
/// Useful for finding location IDs for geo-targeting campaigns.
pub async fn search_geo_targets(
    client: &GoogleAdsClient,
    customer_id: &str,
    query: &str,
) -> Result<String> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "Location search query is empty. Pass a place name, e.g. \"Fortaleza\".".to_string(),
        ));
    }

    let normalized = deaccent(trimmed);

    let gaql_query = format!(
        "SELECT \
            geo_target_constant.id, \
            geo_target_constant.name, \
            geo_target_constant.canonical_name, \
            geo_target_constant.country_code, \
            geo_target_constant.target_type \
        FROM geo_target_constant \
        WHERE geo_target_constant.name LIKE '%{}%'",
        normalized.replace('\'', "\\'")
    );

    let rows = client.search(customer_id, &gaql_query).await?;

    let mut result = serde_json::json!({
        "geo_targets": rows,
        "total_count": rows.len(),
    });

    // Surface the normalization so a caller who searched "São Paulo" can see
    // that "Sao Paulo" is what actually went to the API.
    if normalized != trimmed {
        if let Some(obj) = result.as_object_mut() {
            obj.insert(
                "searched_as".to_string(),
                serde_json::Value::String(normalized),
            );
        }
    }

    serde_json::to_string_pretty(&result).map_err(Into::into)
}

/// Get geographic performance data for campaigns.
pub async fn get_geo_performance(
    client: &GoogleAdsClient,
    customer_id: &str,
    date_start: Option<&str>,
    date_end: Option<&str>,
) -> Result<String> {
    let date_clause = match (date_start, date_end) {
        (Some(s), Some(e)) => gaql::date_clause(s, e),
        _ => "segments.date DURING LAST_30_DAYS".to_string(),
    };

    let query = format!(
        "SELECT \
            campaign.name, \
            geographic_view.country_criterion_id, \
            geographic_view.location_type, \
            metrics.impressions, \
            metrics.clicks, \
            metrics.cost_micros, \
            metrics.conversions \
        FROM geographic_view \
        WHERE {} \
        ORDER BY metrics.cost_micros DESC",
        date_clause
    );

    let mut rows = client.search(customer_id, &query).await?;
    gaql::enrich_cost_fields(&mut rows);

    let result = serde_json::json!({
        "geo_performance": rows,
        "total_count": rows.len(),
    });

    serde_json::to_string_pretty(&result).map_err(Into::into)
}

/// Parse a geo target id into its bare numeric form.
///
/// Accepts the bare numeric id ("2276") or the full resource name
/// ("geoTargetConstants/2276"). Rejects empty, non-numeric, or over-long ids.
/// For LOCATION campaign criteria the criterion id equals this geo target
/// constant id, so the bare form is what builds both the `geoTargetConstant`
/// reference and the `campaignCriteria/{campaign}~{id}` resource name.
fn parse_geo_target_id(geo_target_id: &str) -> Result<String> {
    let trimmed = geo_target_id.trim();
    let id = trimmed
        .strip_prefix("geoTargetConstants/")
        .unwrap_or(trimmed);

    if id.is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "geo_target_id must not be empty".to_string(),
        ));
    }
    if id.len() > MAX_GEO_TARGET_ID_LEN {
        return Err(McpGoogleAdsError::Validation(format!(
            "geo_target_id is too long (max {} digits), got '{}'",
            MAX_GEO_TARGET_ID_LEN, geo_target_id
        )));
    }
    if !id.chars().all(|c| c.is_ascii_digit()) {
        return Err(McpGoogleAdsError::Validation(format!(
            "geo_target_id must be numeric (e.g. '2276' or 'geoTargetConstants/2276'), got '{}'",
            geo_target_id
        )));
    }

    Ok(id.to_string())
}

/// Normalise a geo target id into a `geoTargetConstants/{id}` resource name.
fn normalize_geo_target_constant(geo_target_id: &str) -> Result<String> {
    Ok(format!(
        "geoTargetConstants/{}",
        parse_geo_target_id(geo_target_id)?
    ))
}

/// Exclude a geographic location from a campaign.
///
/// Creates a negative `campaignCriterionOperation` carrying a `location`
/// criterion — the inverse of the positive geo targeting that `update_campaign`
/// adds. Ads in `campaign_id` stop serving to users in `geo_target_id`.
///
/// `geo_target_id` accepts either the bare numeric ID ("2276") or the full
/// resource name ("geoTargetConstants/2276"); both normalise to the same
/// criterion. Discover IDs with `search_geo_targets`.
///
/// Returns a [`ChangePlan`] preview that must be confirmed via
/// `confirm_and_apply`. The exclusion is reversible (remove the resulting
/// campaign criterion) and therefore does not require double confirmation.
pub fn exclude_geo_target(
    config: &Config,
    customer_id: &str,
    campaign_id: &str,
    geo_target_id: &str,
) -> Result<serde_json::Value> {
    check_blocked_operation("exclude_geo_target", &config.safety)?;

    let geo_target_constant = normalize_geo_target_constant(geo_target_id)?;
    let cid = GoogleAdsClient::normalize_customer_id(customer_id);
    let campaign_resource = format!("customers/{}/campaigns/{}", cid, campaign_id);

    let operations = vec![json!({
        "campaignCriterionOperation": {
            "create": {
                "campaign": campaign_resource,
                "negative": true,
                "location": { "geoTargetConstant": geo_target_constant }
            }
        }
    })];

    let changes = json!({
        "campaign_id": campaign_id,
        "geo_target_constant": geo_target_constant,
        "negative": true,
    });

    let plan = ChangePlan::new(
        "exclude_geo_target".to_string(),
        "campaign_criterion".to_string(),
        campaign_id.to_string(),
        cid,
        changes,
        false,
        operations,
    );

    let preview = plan.to_preview();
    store_plan(plan);
    Ok(preview)
}

/// Remove a positively-targeted geographic location from a campaign.
///
/// Use this when a location is currently an *included* target (a positive
/// `campaign_criterion`) and you want to stop serving there — the common case
/// for trimming a country out of a multi-country campaign. It removes the
/// existing criterion rather than layering a negative one.
///
/// This is the necessary counterpart to [`exclude_geo_target`]: for LOCATION
/// criteria the criterion id equals the geo target constant id, so a campaign
/// that already targets `geo_target_id` positively cannot also receive a
/// negative criterion for the same id (it would collide on
/// `campaignCriteria/{campaign}~{id}`). Removing the positive criterion is the
/// correct operation in that case.
///
/// `geo_target_id` accepts the bare numeric ID ("2276") or the full resource
/// name ("geoTargetConstants/2276"). Returns a [`ChangePlan`] preview that must
/// be confirmed via `confirm_and_apply`; like the other removals it requires
/// double confirmation.
pub fn remove_geo_target(
    config: &Config,
    customer_id: &str,
    campaign_id: &str,
    geo_target_id: &str,
) -> Result<serde_json::Value> {
    check_blocked_operation("remove_geo_target", &config.safety)?;

    let geo_id = parse_geo_target_id(geo_target_id)?;
    let cid = GoogleAdsClient::normalize_customer_id(customer_id);
    let criterion_resource = format!(
        "customers/{}/campaignCriteria/{}~{}",
        cid, campaign_id, geo_id
    );

    let operations = vec![json!({
        "campaignCriterionOperation": {
            "remove": criterion_resource
        }
    })];

    let changes = json!({
        "campaign_id": campaign_id,
        "geo_target_id": geo_id,
        "removed_criterion": format!("{}~{}", campaign_id, geo_id),
        "warning": "Removes the campaign's positive location target. Reversible by re-adding it.",
    });

    let plan = ChangePlan::new(
        "remove_geo_target".to_string(),
        "campaign_criterion".to_string(),
        campaign_id.to_string(),
        cid,
        changes,
        true,
        operations,
    );

    let preview = plan.to_preview();
    store_plan(plan);
    Ok(preview)
}

/// Upper bound on radii accepted in one `add_proximity_targets` call.
///
/// The method these tools serve layers 1/3/5/10 km rings; anything approaching
/// this many is a malformed caller rather than a targeting plan.
const MAX_PROXIMITY_RADII: usize = 20;

/// Inclusive bid modifier range Google accepts on a campaign criterion.
/// Verified live against v25: 0.0 fails `TOO_LOW`, 10.1 fails `TOO_HIGH`.
const BID_MODIFIER_BOUNDS: (f64, f64) = (0.1, 10.0);

/// Convert decimal degrees to the micro degrees the API stores.
fn to_micro_degrees(degrees: f64) -> i64 {
    (degrees * 1_000_000.0).round() as i64
}

/// Convert stored micro degrees back to decimal degrees for display.
fn from_micro_degrees(micro: i64) -> f64 {
    micro as f64 / 1_000_000.0
}

fn validate_campaign_id(campaign_id: &str) -> Result<String> {
    let trimmed = campaign_id.trim();
    if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_ascii_digit()) {
        return Err(McpGoogleAdsError::Validation(format!(
            "campaign_id must be a bare numeric ID, got '{}'",
            campaign_id
        )));
    }
    Ok(trimmed.to_string())
}

/// Validate a latitude/longitude pair in decimal degrees.
fn validate_geo_point(latitude: f64, longitude: f64) -> Result<()> {
    if !latitude.is_finite() || !(-90.0..=90.0).contains(&latitude) {
        return Err(McpGoogleAdsError::Validation(format!(
            "latitude must be between -90 and 90 degrees, got {}",
            latitude
        )));
    }
    if !longitude.is_finite() || !(-180.0..=180.0).contains(&longitude) {
        return Err(McpGoogleAdsError::Validation(format!(
            "longitude must be between -180 and 180 degrees, got {}",
            longitude
        )));
    }
    Ok(())
}

/// Validate the radius list against the unit's live bounds, dropping repeats.
///
/// Returns the accepted radii in the order given and the count de-duplicated,
/// so a caller that passes `[1, 3, 3, 5]` is told rather than silently given
/// three rings where it asked for four.
fn validate_radii(radii: &[f64], units: ProximityRadiusUnits) -> Result<(Vec<f64>, usize)> {
    if radii.is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "radii is empty — pass at least one radius, e.g. [1.0, 3.0, 5.0, 10.0].".to_string(),
        ));
    }
    if radii.len() > MAX_PROXIMITY_RADII {
        return Err(McpGoogleAdsError::Validation(format!(
            "radii has {} entries, over the {} limit",
            radii.len(),
            MAX_PROXIMITY_RADII
        )));
    }

    let (min, max) = units.radius_bounds();
    let mut accepted: Vec<f64> = Vec::with_capacity(radii.len());
    let mut duplicates = 0usize;

    for radius in radii {
        if !radius.is_finite() || *radius < min || *radius > max {
            return Err(McpGoogleAdsError::Validation(format!(
                "radius {} is outside the {} range Google accepts ({}–{}). \
                 Verified live: anything outside fails INVALID_PROXIMITY_RADIUS.",
                radius, units, min, max
            )));
        }
        // Two identical rings would be two criteria covering the same area,
        // which Google accepts and silently double-bills attention on.
        if accepted.iter().any(|r| (r - radius).abs() < f64::EPSILON) {
            duplicates += 1;
            continue;
        }
        accepted.push(*radius);
    }

    Ok((accepted, duplicates))
}

fn validate_bid_modifier(bid_modifier: Option<f64>) -> Result<()> {
    let Some(modifier) = bid_modifier else {
        return Ok(());
    };
    let (min, max) = BID_MODIFIER_BOUNDS;
    if !modifier.is_finite() || modifier < min || modifier > max {
        return Err(McpGoogleAdsError::Validation(format!(
            "bid_modifier must be between {} and {}, got {}. Google answers \
             TOO_LOW / TOO_HIGH outside that range.",
            min, max, modifier
        )));
    }
    Ok(())
}

/// Add one or more radius (proximity) targets to a campaign.
///
/// Google has no "radius" field on a campaign — a radius is a `PROXIMITY`
/// campaign criterion, one per ring. Passing several radii creates the
/// overlapping rings that let bids be tuned per distance band later, which is
/// the whole reason to layer them rather than target one wide circle.
///
/// The rings are created in a single atomic mutate: a partially applied set
/// reads as complete targeting while leaving the outer bands uncovered.
///
/// `latitude`/`longitude` are decimal degrees (the form Google Maps shows) and
/// are converted to the micro degrees the API stores. Radius bounds are
/// enforced per unit — 1–800 km, 1–500 miles — verified live against v25.
///
/// Returns a [`ChangePlan`] preview that must be confirmed via
/// `confirm_and_apply`. Adding targeting is reversible (remove the criteria),
/// so it does not require double confirmation.
#[allow(clippy::too_many_arguments)]
pub fn add_proximity_targets(
    config: &Config,
    customer_id: &str,
    campaign_id: &str,
    latitude: f64,
    longitude: f64,
    radii: &[f64],
    radius_units: ProximityRadiusUnits,
    bid_modifier: Option<f64>,
) -> Result<serde_json::Value> {
    check_blocked_operation("add_proximity_targets", &config.safety)?;

    let campaign = validate_campaign_id(campaign_id)?;
    validate_geo_point(latitude, longitude)?;
    let (accepted_radii, duplicates_dropped) = validate_radii(radii, radius_units)?;
    validate_bid_modifier(bid_modifier)?;

    let cid = GoogleAdsClient::normalize_customer_id(customer_id);
    let campaign_resource = format!("customers/{}/campaigns/{}", cid, campaign);

    let operations: Vec<serde_json::Value> = accepted_radii
        .iter()
        .map(|radius| {
            let mut create = json!({
                "campaign": campaign_resource,
                "proximity": {
                    "geoPoint": {
                        "latitudeInMicroDegrees": to_micro_degrees(latitude),
                        "longitudeInMicroDegrees": to_micro_degrees(longitude)
                    },
                    "radius": radius,
                    "radiusUnits": radius_units.as_api_str()
                }
            });
            if let Some(modifier) = bid_modifier {
                create["bidModifier"] = json!(modifier);
            }
            json!({ "campaignCriterionOperation": { "create": create } })
        })
        .collect();

    let mut changes = json!({
        "campaign_id": campaign,
        "latitude": latitude,
        "longitude": longitude,
        "radii": accepted_radii,
        "radius_units": radius_units.as_api_str(),
        "criteria_created": accepted_radii.len(),
        "bid_modifier": bid_modifier,
    });
    if duplicates_dropped > 0 {
        changes["duplicates_dropped"] = json!(duplicates_dropped);
    }

    let plan = ChangePlan::new(
        "add_proximity_targets".to_string(),
        "campaign_criterion".to_string(),
        campaign,
        cid,
        changes,
        false,
        operations,
    );

    let preview = plan.to_preview();
    store_plan(plan);
    Ok(preview)
}

/// List the radius (proximity) targets on a campaign, or across the account.
///
/// Returns each ring's `criterion_id`, which is what
/// [`remove_proximity_target`] needs — a proximity criterion ID is assigned by
/// Google rather than derived from the target like a location criterion's is,
/// so it cannot be constructed by hand.
pub async fn get_proximity_targets(
    client: &GoogleAdsClient,
    customer_id: &str,
    campaign_id: Option<&str>,
) -> Result<String> {
    let mut query = "SELECT \
            campaign.id, \
            campaign.name, \
            campaign_criterion.criterion_id, \
            campaign_criterion.bid_modifier, \
            campaign_criterion.proximity.radius, \
            campaign_criterion.proximity.radius_units, \
            campaign_criterion.proximity.geo_point.latitude_in_micro_degrees, \
            campaign_criterion.proximity.geo_point.longitude_in_micro_degrees, \
            campaign_criterion.proximity.address.city_name, \
            campaign_criterion.proximity.address.province_name, \
            campaign_criterion.proximity.address.postal_code \
        FROM campaign_criterion \
        WHERE campaign_criterion.type = 'PROXIMITY'"
        .to_string();

    if let Some(id) = campaign_id {
        let campaign = validate_campaign_id(id)?;
        query.push_str(&format!(" AND campaign.id = {}", campaign));
    }

    let rows = client.search(customer_id, &query).await?;

    let targets: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let criterion = row.get("campaignCriterion");
            let proximity = criterion.and_then(|c| c.get("proximity"));
            let geo_point = proximity.and_then(|p| p.get("geoPoint"));

            let latitude = geo_point
                .and_then(|g| g.get("latitudeInMicroDegrees"))
                .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
                .map(from_micro_degrees);
            let longitude = geo_point
                .and_then(|g| g.get("longitudeInMicroDegrees"))
                .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
                .map(from_micro_degrees);

            json!({
                "campaign_id": row.get("campaign").and_then(|c| c.get("id")),
                "campaign_name": row.get("campaign").and_then(|c| c.get("name")),
                "criterion_id": criterion.and_then(|c| c.get("criterionId")),
                "radius": proximity.and_then(|p| p.get("radius")),
                "radius_units": proximity.and_then(|p| p.get("radiusUnits")),
                "latitude": latitude,
                "longitude": longitude,
                "address": proximity.and_then(|p| p.get("address")),
                "bid_modifier": criterion.and_then(|c| c.get("bidModifier")),
            })
        })
        .collect();

    let result = json!({
        "proximity_targets": targets,
        "total_count": targets.len(),
    });

    serde_json::to_string_pretty(&result).map_err(Into::into)
}

/// Remove one radius (proximity) target from a campaign.
///
/// `criterion_id` comes from [`get_proximity_targets`]; unlike a location
/// criterion it is not derivable from the target itself.
///
/// Requires double confirmation: dropping a ring silently widens or narrows
/// where the campaign serves, and the change is invisible in day-to-day
/// reporting until the spend pattern shifts.
pub fn remove_proximity_target(
    config: &Config,
    customer_id: &str,
    campaign_id: &str,
    criterion_id: &str,
) -> Result<serde_json::Value> {
    check_blocked_operation("remove_proximity_target", &config.safety)?;

    let campaign = validate_campaign_id(campaign_id)?;
    let criterion = criterion_id.trim();
    if criterion.is_empty() || !criterion.chars().all(|c| c.is_ascii_digit()) {
        return Err(McpGoogleAdsError::Validation(format!(
            "criterion_id must be a bare numeric ID from get_proximity_targets, got '{}'",
            criterion_id
        )));
    }

    let cid = GoogleAdsClient::normalize_customer_id(customer_id);
    let criterion_resource = format!(
        "customers/{}/campaignCriteria/{}~{}",
        cid, campaign, criterion
    );

    let operations = vec![json!({
        "campaignCriterionOperation": { "remove": criterion_resource }
    })];

    let changes = json!({
        "campaign_id": campaign,
        "criterion_id": criterion,
        "removed_criterion": format!("{}~{}", campaign, criterion),
        "warning": "Removes one radius target. If it was the campaign's only \
                    location criterion the campaign falls back to serving \
                    everywhere its other settings allow.",
    });

    let plan = ChangePlan::new(
        "remove_proximity_target".to_string(),
        "campaign_criterion".to_string(),
        campaign,
        cid,
        changes,
        true,
        operations,
    );

    let preview = plan.to_preview();
    store_plan(plan);
    Ok(preview)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::safety::preview::{get_plan, remove_plan};

    /// Extract the single `campaignCriterionOperation.create` object from a stored plan.
    fn created_criterion(preview: &serde_json::Value) -> serde_json::Value {
        let plan_id = preview["plan_id"].as_str().expect("plan_id present");
        let plan = get_plan(plan_id).expect("plan stored");
        let op = plan
            .mutate_operations
            .first()
            .expect("one mutate operation")
            .clone();
        remove_plan(plan_id);
        op["campaignCriterionOperation"]["create"].clone()
    }

    #[test]
    fn test_exclude_geo_target_bare_id() {
        let config = Config::default();
        let preview =
            exclude_geo_target(&config, "123-456-7890", "23812937408", "2276").expect("ok");

        assert_eq!(preview["operation"], "exclude_geo_target");
        assert_eq!(preview["status"], "PENDING_CONFIRMATION");
        assert_eq!(preview["requires_double_confirm"], false);

        let create = created_criterion(&preview);
        // The whole point: it is a NEGATIVE location criterion on the campaign.
        assert_eq!(create["negative"], true);
        assert_eq!(
            create["location"]["geoTargetConstant"],
            "geoTargetConstants/2276"
        );
        assert_eq!(
            create["campaign"],
            "customers/1234567890/campaigns/23812937408"
        );
    }

    #[test]
    fn test_exclude_geo_target_full_resource_name_normalised() {
        let config = Config::default();
        let preview = exclude_geo_target(&config, "1234567890", "111", "geoTargetConstants/2276")
            .expect("ok");
        let create = created_criterion(&preview);
        // A full resource name is accepted and not double-prefixed.
        assert_eq!(
            create["location"]["geoTargetConstant"],
            "geoTargetConstants/2276"
        );
    }

    #[test]
    fn test_exclude_geo_target_empty_id_rejected() {
        let config = Config::default();
        let err =
            exclude_geo_target(&config, "1234567890", "111", "   ").expect_err("empty id rejected");
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn test_exclude_geo_target_non_numeric_rejected() {
        let config = Config::default();
        let err = exclude_geo_target(&config, "1234567890", "111", "Germany")
            .expect_err("non-numeric id rejected");
        assert!(err.to_string().contains("numeric"));
    }

    #[test]
    fn test_exclude_geo_target_too_long_rejected() {
        let config = Config::default();
        let long_id = "1".repeat(MAX_GEO_TARGET_ID_LEN + 1);
        let err = exclude_geo_target(&config, "1234567890", "111", &long_id)
            .expect_err("over-long id rejected");
        assert!(err.to_string().contains("too long"));
    }

    #[test]
    fn test_exclude_geo_target_blocked_operation_rejected() {
        let mut config = Config::default();
        config.safety.blocked_operations = vec!["exclude_geo_target".to_string()];
        let err = exclude_geo_target(&config, "1234567890", "111", "2276")
            .expect_err("blocked operation rejected");
        assert!(err.to_string().contains("blocked"));
    }

    /// Extract the `campaignCriterionOperation.remove` resource string from a stored plan.
    fn removed_criterion(preview: &serde_json::Value) -> serde_json::Value {
        let plan_id = preview["plan_id"].as_str().expect("plan_id present");
        let plan = get_plan(plan_id).expect("plan stored");
        let op = plan
            .mutate_operations
            .first()
            .expect("one mutate operation")
            .clone();
        remove_plan(plan_id);
        op["campaignCriterionOperation"]["remove"].clone()
    }

    #[test]
    fn test_remove_geo_target_builds_criterion_resource() {
        let config = Config::default();
        let preview =
            remove_geo_target(&config, "123-456-7890", "23812937408", "2276").expect("ok");

        assert_eq!(preview["operation"], "remove_geo_target");
        assert_eq!(preview["status"], "PENDING_CONFIRMATION");
        // Removals require double confirmation, like the other remove_* tools.
        assert_eq!(preview["requires_double_confirm"], true);

        // For LOCATION criteria the criterion id equals the geo target id.
        assert_eq!(
            removed_criterion(&preview),
            "customers/1234567890/campaignCriteria/23812937408~2276"
        );
    }

    #[test]
    fn test_remove_geo_target_accepts_full_resource_name() {
        let config = Config::default();
        let preview =
            remove_geo_target(&config, "1234567890", "111", "geoTargetConstants/2276").expect("ok");
        assert_eq!(
            removed_criterion(&preview),
            "customers/1234567890/campaignCriteria/111~2276"
        );
    }

    #[test]
    fn test_remove_geo_target_non_numeric_rejected() {
        let config = Config::default();
        let err = remove_geo_target(&config, "1234567890", "111", "DE")
            .expect_err("non-numeric id rejected");
        assert!(err.to_string().contains("numeric"));
    }

    #[test]
    fn test_remove_geo_target_blocked_operation_rejected() {
        let mut config = Config::default();
        config.safety.blocked_operations = vec!["remove_geo_target".to_string()];
        let err = remove_geo_target(&config, "1234567890", "111", "2276")
            .expect_err("blocked operation rejected");
        assert!(err.to_string().contains("blocked"));
    }

    #[test]
    fn test_deaccent_strips_portuguese_diacritics() {
        assert_eq!(deaccent("São Benedito"), "Sao Benedito");
        assert_eq!(deaccent("Ceará"), "Ceara");
        assert_eq!(deaccent("Goiânia"), "Goiania");
        assert_eq!(deaccent("Brasília"), "Brasilia");
        assert_eq!(deaccent("Maceió"), "Maceio");
        assert_eq!(deaccent("Açores"), "Acores");
    }

    #[test]
    fn test_deaccent_covers_other_latin_scripts() {
        assert_eq!(deaccent("Zürich"), "Zurich");
        assert_eq!(deaccent("Málaga"), "Malaga");
        assert_eq!(deaccent("Orléans"), "Orleans");
        assert_eq!(deaccent("España"), "Espana");
        assert_eq!(deaccent("Ålesund"), "Alesund");
    }

    #[test]
    fn test_deaccent_leaves_ascii_untouched() {
        assert_eq!(deaccent("Fortaleza"), "Fortaleza");
        assert_eq!(deaccent("New York"), "New York");
        assert_eq!(deaccent(""), "");
    }

    // ── Proximity targeting ─────────────────────────────────────────────

    /// All create payloads from a stored plan, in order.
    fn created_criteria(preview: &serde_json::Value) -> Vec<serde_json::Value> {
        let plan_id = preview["plan_id"].as_str().expect("plan_id present");
        let plan = get_plan(plan_id).expect("plan stored");
        let ops: Vec<serde_json::Value> = plan
            .mutate_operations
            .iter()
            .map(|op| op["campaignCriterionOperation"]["create"].clone())
            .collect();
        remove_plan(plan_id);
        ops
    }

    #[test]
    fn test_add_proximity_creates_one_criterion_per_radius() {
        let config = Config::default();
        let preview = add_proximity_targets(
            &config,
            "123-456-7890",
            "23945180054",
            -4.048158,
            -40.864613,
            &[1.0, 3.0, 5.0, 10.0],
            ProximityRadiusUnits::Kilometers,
            None,
        )
        .expect("ok");

        assert_eq!(preview["operation"], "add_proximity_targets");
        assert_eq!(preview["requires_double_confirm"], false);

        let creates = created_criteria(&preview);
        assert_eq!(creates.len(), 4, "one criterion per ring");

        // Degrees are stored as micro degrees; the sign must survive.
        let first = &creates[0];
        assert_eq!(
            first["proximity"]["geoPoint"]["latitudeInMicroDegrees"],
            -4048158
        );
        assert_eq!(
            first["proximity"]["geoPoint"]["longitudeInMicroDegrees"],
            -40864613
        );
        assert_eq!(first["proximity"]["radius"], 1.0);
        assert_eq!(first["proximity"]["radiusUnits"], "KILOMETERS");
        assert_eq!(
            first["campaign"],
            "customers/1234567890/campaigns/23945180054"
        );
        // radiusUnits is REQUIRED by the API on every ring, not just the first.
        for create in &creates {
            assert_eq!(create["proximity"]["radiusUnits"], "KILOMETERS");
        }
        assert_eq!(creates[3]["proximity"]["radius"], 10.0);
        // No bid modifier requested: the field must be absent, not null.
        assert!(first.get("bidModifier").is_none());
    }

    #[test]
    fn test_add_proximity_attaches_bid_modifier_to_every_ring() {
        let config = Config::default();
        let preview = add_proximity_targets(
            &config,
            "123-456-7890",
            "111",
            -23.55,
            -46.63,
            &[5.0, 10.0],
            ProximityRadiusUnits::Miles,
            Some(1.25),
        )
        .expect("ok");

        let creates = created_criteria(&preview);
        assert_eq!(creates.len(), 2);
        for create in &creates {
            assert_eq!(create["bidModifier"], 1.25);
            assert_eq!(create["proximity"]["radiusUnits"], "MILES");
        }
    }

    #[test]
    fn test_add_proximity_drops_duplicate_radii_and_reports_it() {
        let config = Config::default();
        let preview = add_proximity_targets(
            &config,
            "123-456-7890",
            "111",
            0.0,
            0.0,
            &[1.0, 3.0, 3.0, 5.0],
            ProximityRadiusUnits::Kilometers,
            None,
        )
        .expect("ok");

        assert_eq!(preview["changes"]["criteria_created"], 3);
        assert_eq!(preview["changes"]["duplicates_dropped"], 1);
        assert_eq!(created_criteria(&preview).len(), 3);
    }

    #[test]
    fn test_add_proximity_rejects_radii_outside_the_live_bounds() {
        let config = Config::default();
        // Verified live: 0.99 km and 801 km both fail INVALID_PROXIMITY_RADIUS.
        for bad in [0.99_f64, 801.0] {
            let err = add_proximity_targets(
                &config,
                "123-456-7890",
                "111",
                0.0,
                0.0,
                &[bad],
                ProximityRadiusUnits::Kilometers,
                None,
            )
            .expect_err("radius out of range");
            assert!(err.to_string().contains("INVALID_PROXIMITY_RADIUS"));
        }

        // Miles top out lower than kilometres do.
        let err = add_proximity_targets(
            &config,
            "123-456-7890",
            "111",
            0.0,
            0.0,
            &[501.0],
            ProximityRadiusUnits::Miles,
            None,
        )
        .expect_err("miles out of range");
        assert!(err.to_string().contains("501"));

        // 500 miles is still inside.
        assert!(add_proximity_targets(
            &config,
            "123-456-7890",
            "111",
            0.0,
            0.0,
            &[500.0],
            ProximityRadiusUnits::Miles,
            None,
        )
        .is_ok());
    }

    #[test]
    fn test_add_proximity_rejects_bad_coordinates_and_empty_radii() {
        let config = Config::default();
        let bad_lat = add_proximity_targets(
            &config,
            "123-456-7890",
            "111",
            91.0,
            0.0,
            &[5.0],
            ProximityRadiusUnits::Kilometers,
            None,
        )
        .expect_err("latitude out of range");
        assert!(bad_lat.to_string().contains("latitude"));

        let bad_lon = add_proximity_targets(
            &config,
            "123-456-7890",
            "111",
            0.0,
            -181.0,
            &[5.0],
            ProximityRadiusUnits::Kilometers,
            None,
        )
        .expect_err("longitude out of range");
        assert!(bad_lon.to_string().contains("longitude"));

        let empty = add_proximity_targets(
            &config,
            "123-456-7890",
            "111",
            0.0,
            0.0,
            &[],
            ProximityRadiusUnits::Kilometers,
            None,
        )
        .expect_err("no radii");
        assert!(empty.to_string().contains("radii is empty"));
    }

    #[test]
    fn test_add_proximity_rejects_bid_modifier_outside_google_bounds() {
        let config = Config::default();
        // Verified live: 0.0 answers TOO_LOW, 10.1 answers TOO_HIGH.
        for bad in [0.0_f64, 10.1] {
            let err = add_proximity_targets(
                &config,
                "123-456-7890",
                "111",
                0.0,
                0.0,
                &[5.0],
                ProximityRadiusUnits::Kilometers,
                Some(bad),
            )
            .expect_err("bid modifier out of range");
            assert!(err.to_string().contains("bid_modifier"));
        }
    }

    #[test]
    fn test_remove_proximity_target_is_double_confirm() {
        let config = Config::default();
        let preview =
            remove_proximity_target(&config, "123-456-7890", "23945180054", "2553801995517")
                .expect("ok");

        assert_eq!(preview["operation"], "remove_proximity_target");
        assert_eq!(preview["requires_double_confirm"], true);

        let plan_id = preview["plan_id"].as_str().expect("plan_id");
        let plan = get_plan(plan_id).expect("plan stored");
        assert_eq!(
            plan.mutate_operations[0]["campaignCriterionOperation"]["remove"],
            "customers/1234567890/campaignCriteria/23945180054~2553801995517"
        );
        remove_plan(plan_id);
    }

    #[test]
    fn test_remove_proximity_target_rejects_non_numeric_ids() {
        let config = Config::default();
        assert!(
            remove_proximity_target(&config, "123-456-7890", "111", "campaignCriteria/1~2")
                .is_err()
        );
        assert!(remove_proximity_target(&config, "123-456-7890", "abc", "123").is_err());
    }

    #[test]
    fn test_proximity_respects_blocked_operations() {
        let mut config = Config::default();
        config.safety.blocked_operations = vec!["add_proximity_targets".to_string()];
        let err = add_proximity_targets(
            &config,
            "123-456-7890",
            "111",
            0.0,
            0.0,
            &[5.0],
            ProximityRadiusUnits::Kilometers,
            None,
        )
        .expect_err("blocked");
        assert!(err.to_string().contains("blocked"));
    }

    #[test]
    fn test_micro_degree_round_trip() {
        assert_eq!(to_micro_degrees(-4.048158), -4048158);
        assert_eq!(to_micro_degrees(0.0), 0);
        assert!((from_micro_degrees(-40864613) - (-40.864613)).abs() < 1e-9);
    }
}
