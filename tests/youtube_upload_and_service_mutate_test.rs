//! The two routes `draft_mutate` and `draft_youtube_video_upload` add to
//! `confirm_and_apply`, against a stubbed Google Ads host.
//!
//! - A YouTube upload is the resumable protocol: a `start` POST on the
//!   `/resumable/upload/v25/...:create` host returns `x-goog-upload-url`, and the
//!   whole file goes there in one `upload, finalize` PUT.
//! - A custom segment has no `googleAds:mutate` key, so its plan must reach
//!   `customAudiences:mutate` with the operations as written.

mod common;

use mcp_google_ads::client::{resumable_upload_base, YouTubeVideoUploadRequest};
use mcp_google_ads::safety::preview::get_plan;
use mcp_google_ads::tools::confirm::{confirm_and_apply, ConfirmApplyInput};
use mcp_google_ads::tools::raw_mutate::build_raw_mutate_plan;
use serde_json::json;
use wiremock::matchers::{body_json, body_string, header, headers, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn upload_host_is_derived_from_the_api_base() {
    assert_eq!(
        resumable_upload_base("https://googleads.googleapis.com/v25"),
        "https://googleads.googleapis.com/resumable/upload/v25"
    );
    assert_eq!(
        resumable_upload_base("http://127.0.0.1:1234/v25/"),
        "http://127.0.0.1:1234/resumable/upload/v25"
    );
}

#[tokio::test]
async fn upload_starts_a_resumable_session_and_finalizes_in_one_put() {
    let (mock, client) = common::spawn_mock_google_ads().await;

    let dir = std::env::temp_dir().join("mcp_google_ads_upload_flow");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("ad.mp4");
    std::fs::write(&file, b"0123456789").unwrap();

    let upload_url = format!("{}/upload-session/abc", mock.uri());
    Mock::given(method("POST"))
        .and(path("/resumable/upload/v25/customers/1234567890/youTubeVideoUploads:create"))
        .and(header("x-goog-upload-protocol", "resumable"))
        .and(header("x-goog-upload-command", "start"))
        .and(header("x-goog-upload-header-content-length", "10"))
        .and(body_json(json!({
            "customerId": "1234567890",
            "youTubeVideoUpload": {
                "videoTitle": "Ad01",
                "videoDescription": "d",
                "videoPrivacy": "UNLISTED"
            }
        })))
        .respond_with(ResponseTemplate::new(200).insert_header("x-goog-upload-url", upload_url.as_str()))
        .expect(1)
        .mount(&mock)
        .await;

    Mock::given(method("PUT"))
        .and(path("/upload-session/abc"))
        // wiremock splits header values on commas, so the two commands are matched as a list.
        .and(headers("x-goog-upload-command", vec!["upload", "finalize"]))
        .and(header("x-goog-upload-offset", "0"))
        .and(body_string("0123456789"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "resourceName": "customers/1234567890/youTubeVideoUploads/42"
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let response = client
        .upload_youtube_video(
            "123-456-7890",
            &YouTubeVideoUploadRequest {
                file_path: file.to_string_lossy().to_string(),
                video_title: "Ad01".to_string(),
                video_description: "d".to_string(),
                video_privacy: "UNLISTED".to_string(),
                channel_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        response["resourceName"],
        "customers/1234567890/youTubeVideoUploads/42"
    );
}

#[tokio::test]
async fn custom_audience_plan_is_sent_to_its_own_service() {
    let mock = MockServer::start().await;
    std::env::set_var("GOOGLE_ADS_API_BASE_URL", format!("{}/v25", mock.uri()));

    let operations = vec![json!({"create": {
        "name": "Busca CRM",
        "type": "SEARCH",
        "members": [{"memberType": "KEYWORD", "keyword": "crm para clínicas"}]
    }})];

    Mock::given(method("POST"))
        .and(path("/v25/customers/1234567890/customAudiences:mutate"))
        .and(body_json(json!({
            "operations": operations,
            "validateOnly": false
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{"resourceName": "customers/1234567890/customAudiences/7"}]
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let config = common::test_config();
    let (plan, _) = build_raw_mutate_plan(
        &config,
        "1234567890",
        "custom segment",
        Some("customAudiences"),
        operations.clone(),
    )
    .unwrap();
    let plan_id = plan.plan_id.clone();
    mcp_google_ads::safety::preview::store_plan(plan);

    let result = confirm_and_apply(
        &config,
        ConfirmApplyInput {
            plan_id: plan_id.clone(),
            dry_run: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    std::env::remove_var("GOOGLE_ADS_API_BASE_URL");

    assert_eq!(result["status"], "APPLIED");
    assert_eq!(
        result["results"][0]["resourceName"],
        "customers/1234567890/customAudiences/7"
    );
    assert!(get_plan(&plan_id).is_none(), "an applied plan leaves the store");
}
