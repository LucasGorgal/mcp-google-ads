//! `draft_mutate`: hand-built mutate operations behind the same preview →
//! `confirm_and_apply` gate as every purpose-built tool.
//!
//! The purpose-built tools cover Search, Display and Performance Max. Campaign
//! types they do not model (Demand Gen above all: its ad groups carry channel
//! controls and grouped audiences, its ads are `demandGenVideoResponsiveAd`)
//! would otherwise need a new tool per field. Here the caller writes the
//! operations in the REST shape; the server still owns the guards:
//!
//! - only whitelisted operation keys or services reach the API;
//! - a budget above `max_daily_budget` is refused, as in `draft_campaign`;
//! - anything that can start delivery (a campaign set ENABLED, or an ad group,
//!   ad or criterion set ENABLED under an entity that already exists) and any
//!   `remove` makes the plan `requires_double_confirm`;
//! - the caller can have Google validate the request (`validateOnly`) before
//!   the plan is stored, so a rejected field shows up in the preview instead of
//!   at apply time.

use std::collections::BTreeMap;

use serde_json::json;

use crate::client::{validate_service_mutate, GoogleAdsClient, MutateOperation};
use crate::config::Config;
use crate::error::{McpGoogleAdsError, Result};
use crate::safety::guards::{check_blocked_operation, check_budget_cap};
use crate::safety::preview::{ChangePlan, PlanDispatch};

/// Hard ceiling on operations per plan. Google accepts up to 10,000 per
/// request; a plan this tool drafts is read by a person before it is applied,
/// and nobody reviews thousands of hand-written operations.
pub const MAX_RAW_OPERATIONS: usize = 1000;

/// Operation keys whose `status: ENABLED` can make something serve.
const DELIVERY_KEYS: &[&str] = &[
    "campaignOperation",
    "adGroupOperation",
    "adGroupAdOperation",
    "adGroupCriterionOperation",
    "assetGroupOperation",
];

/// What the preview says about a drafted raw mutate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawMutateSummary {
    /// `"<operationKey>.<create|update|remove>"` -> count.
    pub kinds: BTreeMap<String, usize>,
    /// Human-readable reasons this plan could start or stop delivery.
    pub delivery_risks: Vec<String>,
    pub removes: usize,
}

impl RawMutateSummary {
    pub fn requires_double_confirm(&self) -> bool {
        !self.delivery_risks.is_empty() || self.removes > 0
    }
}

/// Build (but do not store) the plan for a raw mutate. Runs every client-side
/// guard; no HTTP traffic.
///
/// `service = None` sends `operations` as `MutateOperation`s to
/// `googleAds:mutate`; `Some(service)` sends them as that service's own
/// operations to `{service}:mutate`.
pub fn build_raw_mutate_plan(
    config: &Config,
    customer_id: &str,
    description: &str,
    service: Option<&str>,
    operations: Vec<serde_json::Value>,
) -> Result<(ChangePlan, RawMutateSummary)> {
    check_blocked_operation("draft_mutate", &config.safety)?;

    if description.trim().is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "description cannot be empty: say what this plan does".to_string(),
        ));
    }
    if operations.is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "operations cannot be empty".to_string(),
        ));
    }
    if operations.len() > MAX_RAW_OPERATIONS {
        return Err(McpGoogleAdsError::Validation(format!(
            "{} operations exceed the {MAX_RAW_OPERATIONS} per plan limit; split the plan",
            operations.len()
        )));
    }

    let service = service.map(str::trim).filter(|s| !s.is_empty() && *s != "googleAds");
    let cid = GoogleAdsClient::normalize_customer_id(customer_id);

    let summary = match service {
        None => summarize_google_ads_operations(config, &operations)?,
        Some(service) => {
            validate_service_mutate(service, &operations).map_err(McpGoogleAdsError::Validation)?;
            summarize_service_operations(service, &operations)
        }
    };

    let changes = json!({
        "description": description,
        "service": service.unwrap_or("googleAds"),
        "operations_count": operations.len(),
        "operation_kinds": summary.kinds,
        "delivery_risks": summary.delivery_risks,
        "removes": summary.removes,
        "operations": operations,
    });

    let mut plan = ChangePlan::new(
        "draft_mutate".to_string(),
        service.unwrap_or("mutate").to_string(),
        "various".to_string(),
        cid,
        changes,
        summary.requires_double_confirm(),
        operations,
    );
    if let Some(service) = service {
        plan = plan.with_dispatch(PlanDispatch::ServiceMutate {
            service: service.to_string(),
        });
    }

    Ok((plan, summary))
}

fn summarize_google_ads_operations(
    config: &Config,
    operations: &[serde_json::Value],
) -> Result<RawMutateSummary> {
    let wrapped: Vec<MutateOperation> = operations
        .iter()
        .map(|op| MutateOperation {
            operation: op.clone(),
        })
        .collect();
    GoogleAdsClient::validate_mutate_operations(&wrapped).map_err(McpGoogleAdsError::Validation)?;

    let mut summary = RawMutateSummary {
        kinds: BTreeMap::new(),
        delivery_risks: Vec::new(),
        removes: 0,
    };

    for (idx, op) in operations.iter().enumerate() {
        let obj = op.as_object().ok_or_else(|| {
            McpGoogleAdsError::Validation(format!("Operation at index {idx} is not a JSON object"))
        })?;
        if obj.len() != 1 {
            return Err(McpGoogleAdsError::Validation(format!(
                "Operation at index {idx} must hold exactly one operation key, found {}",
                obj.len()
            )));
        }
        let (key, inner) = obj.iter().next().expect("len checked above");
        let (kind, body) = single_kind(idx, inner)?;
        *summary.kinds.entry(format!("{key}.{kind}")).or_insert(0) += 1;

        if kind == "remove" {
            summary.removes += 1;
            continue;
        }

        if key == "campaignBudgetOperation" {
            if let Some(micros) = body.get("amountMicros").and_then(micros_value) {
                check_budget_cap(micros as f64 / 1_000_000.0, &config.safety)?;
            }
        }

        if DELIVERY_KEYS.contains(&key.as_str())
            && body.get("status").and_then(|s| s.as_str()) == Some("ENABLED")
            && can_deliver(key, kind, body)
        {
            summary.delivery_risks.push(format!(
                "operation {idx}: {key}.{kind} sets status ENABLED on {}",
                describe_target(key, kind, body)
            ));
        }
    }

    Ok(summary)
}

fn summarize_service_operations(
    service: &str,
    operations: &[serde_json::Value],
) -> RawMutateSummary {
    let mut summary = RawMutateSummary {
        kinds: BTreeMap::new(),
        delivery_risks: Vec::new(),
        removes: 0,
    };
    for op in operations {
        if let Some(kind) = ["create", "update", "remove"]
            .into_iter()
            .find(|k| op.get(*k).is_some())
        {
            *summary.kinds.entry(format!("{service}.{kind}")).or_insert(0) += 1;
            if kind == "remove" {
                summary.removes += 1;
            }
        }
    }
    summary
}

/// The single `create` / `update` / `remove` inside a `MutateOperation` value.
fn single_kind(
    idx: usize,
    inner: &serde_json::Value,
) -> Result<(&'static str, &serde_json::Value)> {
    let obj = inner.as_object().ok_or_else(|| {
        McpGoogleAdsError::Validation(format!("Operation at index {idx} is not a JSON object"))
    })?;
    let found: Vec<&'static str> = ["create", "update", "remove"]
        .into_iter()
        .filter(|k| obj.contains_key(*k))
        .collect();
    match found.as_slice() {
        [kind] => Ok((kind, &obj[*kind])),
        _ => Err(McpGoogleAdsError::Validation(format!(
            "Operation at index {idx} must carry exactly one of create, update or remove"
        ))),
    }
}

/// `amountMicros` arrives as a JSON string (int64 in REST) or a number.
fn micros_value(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::String(s) => s.parse().ok(),
        serde_json::Value::Number(n) => n.as_i64(),
        _ => None,
    }
}

/// Whether an ENABLED status in this operation can make something serve.
///
/// A campaign set ENABLED always can. A child created ENABLED under a parent
/// created in the same request (temporary negative ID) serves only once that
/// parent is enabled, which this rule catches on the parent itself. A child
/// updated to ENABLED, or created under an existing parent, can serve at once.
fn can_deliver(key: &str, kind: &str, body: &serde_json::Value) -> bool {
    if key == "campaignOperation" || kind == "update" {
        return true;
    }
    let parent_field = match key {
        "adGroupOperation" => "campaign",
        "adGroupAdOperation" | "adGroupCriterionOperation" => "adGroup",
        "assetGroupOperation" => "campaign",
        _ => return true,
    };
    let parent = body.get(parent_field).and_then(|p| p.as_str()).unwrap_or("");
    !is_temporary_resource(parent)
}

/// `customers/123/campaigns/-2` is a temporary ID minted inside the request.
fn is_temporary_resource(resource_name: &str) -> bool {
    resource_name
        .rsplit('/')
        .next()
        .is_some_and(|id| id.starts_with('-'))
}

fn describe_target(key: &str, kind: &str, body: &serde_json::Value) -> String {
    if kind == "update" {
        return body
            .get("resourceName")
            .and_then(|r| r.as_str())
            .unwrap_or("an existing entity")
            .to_string();
    }
    let name = body.get("name").and_then(|n| n.as_str());
    match (key, name) {
        ("campaignOperation", Some(name)) => format!("new campaign '{name}'"),
        (_, Some(name)) => format!("new entity '{name}' under an existing parent"),
        _ => "a new entity under an existing parent".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        let mut config = Config::default();
        config.safety.max_daily_budget = 50.0;
        config
    }

    fn paused_campaign_ops() -> Vec<serde_json::Value> {
        vec![
            json!({"campaignBudgetOperation": {"create": {
                "resourceName": "customers/1/campaignBudgets/-1",
                "amountMicros": "40000000", "explicitlyShared": false}}}),
            json!({"campaignOperation": {"create": {
                "resourceName": "customers/1/campaigns/-2", "name": "DG",
                "status": "PAUSED", "advertisingChannelType": "DEMAND_GEN",
                "campaignBudget": "customers/1/campaignBudgets/-1"}}}),
            json!({"adGroupOperation": {"create": {
                "resourceName": "customers/1/adGroups/-3", "name": "AG",
                "status": "ENABLED", "campaign": "customers/1/campaigns/-2"}}}),
        ]
    }

    #[test]
    fn paused_campaign_with_enabled_children_needs_no_double_confirm() {
        let (plan, summary) =
            build_raw_mutate_plan(&config(), "1", "new DG campaign", None, paused_campaign_ops())
                .unwrap();
        assert!(summary.delivery_risks.is_empty(), "{:?}", summary.delivery_risks);
        assert!(!plan.requires_double_confirm);
        assert_eq!(plan.dispatch, PlanDispatch::MutateOperations);
        assert_eq!(summary.kinds["campaignOperation.create"], 1);
        assert_eq!(plan.mutate_operations.len(), 3);
    }

    #[test]
    fn enabled_campaign_requires_double_confirm() {
        let mut ops = paused_campaign_ops();
        ops[1]["campaignOperation"]["create"]["status"] = json!("ENABLED");
        let (plan, summary) = build_raw_mutate_plan(&config(), "1", "x", None, ops).unwrap();
        assert_eq!(summary.delivery_risks.len(), 1);
        assert!(plan.requires_double_confirm);
    }

    #[test]
    fn enabled_ad_group_under_existing_campaign_requires_double_confirm() {
        let ops = vec![json!({"adGroupOperation": {"create": {
            "name": "AG", "status": "ENABLED", "campaign": "customers/1/campaigns/555"}}})];
        let (plan, _) = build_raw_mutate_plan(&config(), "1", "x", None, ops).unwrap();
        assert!(plan.requires_double_confirm);
    }

    #[test]
    fn remove_requires_double_confirm() {
        let ops = vec![json!({"adGroupOperation": {"remove": "customers/1/adGroups/9"}})];
        let (plan, summary) = build_raw_mutate_plan(&config(), "1", "x", None, ops).unwrap();
        assert_eq!(summary.removes, 1);
        assert!(plan.requires_double_confirm);
    }

    #[test]
    fn budget_over_cap_is_refused() {
        let mut ops = paused_campaign_ops();
        ops[0]["campaignBudgetOperation"]["create"]["amountMicros"] = json!("80000000");
        let err = build_raw_mutate_plan(&config(), "1", "x", None, ops).unwrap_err();
        assert!(matches!(err, McpGoogleAdsError::Safety(_)), "{err}");
    }

    #[test]
    fn unknown_operation_key_is_refused() {
        // Custom segments have their own service; the googleAds:mutate key that
        // looks right does not exist. (Built at runtime so the whitelist scan in
        // tests/emitted_operations_are_whitelisted_test.rs does not read it as
        // an emitted key.)
        let key = ["customAudience", "Operation"].concat();
        let mut op = serde_json::Map::new();
        op.insert(key.clone(), json!({"create": {}}));
        let err = build_raw_mutate_plan(&config(), "1", "x", None, vec![op.into()]).unwrap_err();
        assert!(err.to_string().contains(&key), "{err}");
    }

    #[test]
    fn two_kinds_in_one_operation_are_refused() {
        let ops = vec![json!({"adGroupOperation": {"create": {}, "remove": "x"}})];
        assert!(build_raw_mutate_plan(&config(), "1", "x", None, ops).is_err());
    }

    #[test]
    fn service_mutate_routes_to_the_service() {
        let ops = vec![json!({"create": {"name": "Seg", "type": "SEARCH",
            "members": [{"memberType": "KEYWORD", "keyword": "crm para clínicas"}]}})];
        let (plan, summary) =
            build_raw_mutate_plan(&config(), "1", "segment", Some("customAudiences"), ops).unwrap();
        assert_eq!(
            plan.dispatch,
            PlanDispatch::ServiceMutate {
                service: "customAudiences".to_string()
            }
        );
        assert_eq!(summary.kinds["customAudiences.create"], 1);
        assert!(!plan.requires_double_confirm);
    }

    #[test]
    fn unknown_service_is_refused() {
        let ops = vec![json!({"create": {}})];
        let err = build_raw_mutate_plan(&config(), "1", "x", Some("campaigns"), ops).unwrap_err();
        assert!(err.to_string().contains("Unknown mutate service"), "{err}");
    }

    #[test]
    fn empty_description_is_refused() {
        assert!(build_raw_mutate_plan(&config(), "1", " ", None, paused_campaign_ops()).is_err());
    }

    #[test]
    fn blocked_operation_is_refused() {
        let mut config = config();
        config.safety.blocked_operations = vec!["draft_mutate".to_string()];
        assert!(build_raw_mutate_plan(&config, "1", "x", None, paused_campaign_ops()).is_err());
    }
}
