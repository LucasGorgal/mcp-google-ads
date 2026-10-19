//! An account the OAuth user was invited to directly, without any manager
//! link, refuses the manager's `login-customer-id` with USER_PERMISSION_DENIED.
//! The client must retry once with the account's own ID, remember that route
//! for the rest of the process, and leave accounts under the manager alone.

mod common;

use mcp_google_ads::client::{GoogleAdsClient, MutateOperation};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MANAGER: &str = "9999999999";

fn permission_denied() -> ResponseTemplate {
    ResponseTemplate::new(403).set_body_json(json!({
        "error": {
            "code": 403,
            "message": "The caller does not have permission",
            "status": "PERMISSION_DENIED",
            "details": [{
                "@type": "type.googleapis.com/google.ads.googleads.v25.errors.GoogleAdsFailure",
                "errors": [{
                    "errorCode": { "authorizationError": "USER_PERMISSION_DENIED" },
                    "message": "User doesn't have permission to access customer. Note: If you're accessing a client customer, the manager's customer id must be set in the 'login-customer-id' header."
                }]
            }]
        }
    }))
}

/// A client whose config routes through `MANAGER`, the way the server runs
/// with `GOOGLE_ADS_LOGIN_CUSTOMER_ID` set.
fn manager_client(mock: &MockServer) -> GoogleAdsClient {
    let mut config = common::test_config();
    config.ads.login_customer_id = Some("999-999-9999".to_string());
    GoogleAdsClient::with_base_url(&config, format!("{}/v25", mock.uri())).unwrap()
}

fn search_path(customer_id: &str) -> String {
    format!("/v25/customers/{customer_id}/googleAds:search")
}

#[tokio::test]
async fn direct_account_retries_with_its_own_login_and_remembers_it() {
    let mock = MockServer::start().await;
    let direct = "5555555501";

    Mock::given(method("POST"))
        .and(path(search_path(direct)))
        .and(header("login-customer-id", MANAGER))
        .respond_with(permission_denied())
        .expect(1)
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(path(search_path(direct)))
        .and(header("login-customer-id", direct))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{ "customer": { "id": direct, "descriptiveName": "Direct" } }]
        })))
        .expect(2)
        .mount(&mock)
        .await;

    let rows = manager_client(&mock)
        .search(direct, "SELECT customer.id FROM customer")
        .await
        .expect("retry through the account's own login should succeed");
    assert_eq!(rows[0]["customer"]["id"], direct);

    // A fresh client, as the next tool call builds, goes straight to the
    // remembered route: the manager mock above expects exactly one hit.
    let rows = manager_client(&mock)
        .search(direct, "SELECT customer.id FROM customer")
        .await
        .expect("remembered direct route should succeed");
    assert_eq!(rows.len(), 1);
}

#[tokio::test]
async fn account_under_the_manager_keeps_the_manager_login() {
    let mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path(search_path("1111111111")))
        .and(header("login-customer-id", MANAGER))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": [] })))
        .expect(1)
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(path(search_path("1111111111")))
        .and(header("login-customer-id", "1111111111"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&mock)
        .await;

    manager_client(&mock)
        .search("1111111111", "SELECT customer.id FROM customer")
        .await
        .expect("manager route should succeed untouched");
}

#[tokio::test]
async fn no_access_through_either_login_surfaces_the_manager_error_and_is_not_remembered() {
    let mock = MockServer::start().await;
    let direct = "5555555503";

    Mock::given(method("POST"))
        .and(path(search_path(direct)))
        .and(header("login-customer-id", MANAGER))
        .respond_with(permission_denied())
        .expect(2)
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(path(search_path(direct)))
        .and(header("login-customer-id", direct))
        .respond_with(permission_denied())
        .expect(2)
        .mount(&mock)
        .await;

    for _ in 0..2 {
        let err = manager_client(&mock)
            .search(direct, "SELECT customer.id FROM customer")
            .await
            .expect_err("no login reaches the account");
        assert!(err.to_string().contains("403"), "unexpected error: {err}");
    }
}

#[tokio::test]
async fn other_errors_do_not_trigger_the_retry() {
    let mock = MockServer::start().await;
    let direct = "5555555504";

    Mock::given(method("POST"))
        .and(path(search_path(direct)))
        .and(header("login-customer-id", MANAGER))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": { "code": 400, "message": "Request contains an invalid argument.", "status": "INVALID_ARGUMENT" }
        })))
        .expect(1)
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(path(search_path(direct)))
        .and(header("login-customer-id", direct))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&mock)
        .await;

    manager_client(&mock)
        .search(direct, "SELECT nonsense FROM customer")
        .await
        .expect_err("a bad query is not a login problem");
}

#[tokio::test]
async fn mutate_on_a_direct_account_is_resent_with_its_own_login() {
    let mock = MockServer::start().await;
    let direct = "5555555505";
    let mutate_path = format!("/v25/customers/{direct}/googleAds:mutate");

    Mock::given(method("POST"))
        .and(path(mutate_path.clone()))
        .and(header("login-customer-id", MANAGER))
        .respond_with(permission_denied())
        .expect(1)
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(path(mutate_path))
        .and(header("login-customer-id", direct))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "mutateOperationResponses": [{ "campaignResult": { "resourceName": format!("customers/{direct}/campaigns/1") } }]
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let response = manager_client(&mock)
        .mutate(
            direct,
            vec![MutateOperation {
                operation: json!({ "campaignOperation": { "update": { "resourceName": format!("customers/{direct}/campaigns/1"), "status": "PAUSED" }, "updateMask": "status" } }),
            }],
        )
        .await
        .expect("mutate should go through the account's own login");
    assert_eq!(response.mutate_operation_responses.len(), 1);

    // The refused attempt and the retry carried the same operations.
    let requests = mock.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body, requests[1].body);
}

#[tokio::test]
async fn list_accounts_reports_direct_access_accounts() {
    let mock = MockServer::start().await;
    let direct = "5555555506";

    Mock::given(method("POST"))
        .and(path(search_path(MANAGER)))
        .and(header("login-customer-id", MANAGER))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                { "customerClient": { "id": MANAGER, "descriptiveName": "Manager", "manager": true } },
                { "customerClient": { "id": "1111111111", "descriptiveName": "Linked", "manager": false } }
            ]
        })))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/v25/customers:listAccessibleCustomers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "resourceNames": [format!("customers/{MANAGER}"), format!("customers/{direct}")]
        })))
        .expect(1)
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(path(search_path(direct)))
        .and(header("login-customer-id", MANAGER))
        .respond_with(permission_denied())
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(path(search_path(direct)))
        .and(header("login-customer-id", direct))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{ "customer": { "id": direct, "descriptiveName": "Invited directly", "manager": false } }]
        })))
        .mount(&mock)
        .await;

    let client = manager_client(&mock);
    let out = mcp_google_ads::tools::accounts::list_accounts(&client, MANAGER)
        .await
        .expect("list_accounts should succeed");
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();

    assert_eq!(parsed["total_count"], 2);
    let found = parsed["direct_access_accounts"].as_array().unwrap();
    assert_eq!(found.len(), 1, "only the account outside the tree: {parsed}");
    assert_eq!(found[0]["customer"]["id"], direct);
    assert_eq!(found[0]["customer"]["descriptiveName"], "Invited directly");
}

#[tokio::test]
async fn list_accounts_survives_a_failed_discovery() {
    let mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path(search_path(MANAGER)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{ "customerClient": { "id": MANAGER } }]
        })))
        .mount(&mock)
        .await;

    // listAccessibleCustomers is left unmocked, so wiremock answers 404.
    let client = manager_client(&mock);
    let out = mcp_google_ads::tools::accounts::list_accounts(&client, MANAGER)
        .await
        .expect("the manager listing must still come back");
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();

    assert_eq!(parsed["total_count"], 1);
    assert!(parsed["direct_access_error"].is_string());
}
