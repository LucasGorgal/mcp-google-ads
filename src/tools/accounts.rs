use std::collections::HashSet;

use crate::client::GoogleAdsClient;
use crate::error::Result;

/// List all accessible Google Ads accounts.
///
/// If `login_customer_id` is set in config (MCC account), queries against it.
/// Otherwise queries the single configured `customer_id`. Accounts the login
/// was invited to directly sit outside that tree, so they are listed
/// separately under `direct_access_accounts`.
pub async fn list_accounts(client: &GoogleAdsClient, customer_id: &str) -> Result<String> {
    let query = "\
        SELECT \
            customer_client.id, \
            customer_client.descriptive_name, \
            customer_client.status, \
            customer_client.manager \
        FROM customer_client";

    let rows = client.search(customer_id, query).await?;

    let mut result = serde_json::json!({
        "accounts": rows,
        "total_count": rows.len(),
    });

    // Discovery must not cost the manager listing that already succeeded.
    match direct_access_accounts(client, customer_id, &rows).await {
        Ok(direct) => result["direct_access_accounts"] = serde_json::Value::Array(direct),
        Err(e) => result["direct_access_error"] = serde_json::Value::String(e.to_string()),
    }

    serde_json::to_string_pretty(&result).map_err(Into::into)
}

/// Accounts the OAuth user opens on their own login that are missing from the
/// listed tree: direct invitations, not linked under the manager. Requests to
/// them go out with their own ID as `login-customer-id`.
async fn direct_access_accounts(
    client: &GoogleAdsClient,
    customer_id: &str,
    tree: &[serde_json::Value],
) -> Result<Vec<serde_json::Value>> {
    let mut listed: HashSet<String> = tree
        .iter()
        .filter_map(|row| row["customerClient"]["id"].as_str().map(String::from))
        .collect();
    listed.insert(GoogleAdsClient::normalize_customer_id(customer_id));

    let query = "\
        SELECT \
            customer.id, \
            customer.descriptive_name, \
            customer.status, \
            customer.manager \
        FROM customer \
        LIMIT 1";

    let mut accounts = Vec::new();
    for id in client.list_accessible_customers().await? {
        if listed.contains(&id) {
            continue;
        }
        let account = match client.search(&id, query).await {
            Ok(rows) => rows
                .into_iter()
                .next()
                .unwrap_or_else(|| serde_json::json!({ "customer": { "id": id } })),
            Err(e) => serde_json::json!({ "customer": { "id": id }, "error": e.to_string() }),
        };
        accounts.push(account);
    }

    Ok(accounts)
}

/// Get detailed information about a specific Google Ads account.
///
/// Returns account metadata including currency, timezone, tagging settings,
/// manager status, and account status.
pub async fn get_account_info(client: &GoogleAdsClient, customer_id: &str) -> Result<String> {
    let query = "\
        SELECT \
            customer.id, \
            customer.descriptive_name, \
            customer.currency_code, \
            customer.time_zone, \
            customer.auto_tagging_enabled, \
            customer.manager, \
            customer.status \
        FROM customer \
        LIMIT 1";

    let rows = client.search(customer_id, query).await?;

    let account = rows.first().cloned();

    let result = serde_json::json!({
        "account": account,
    });

    serde_json::to_string_pretty(&result).map_err(Into::into)
}
