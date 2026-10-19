//! `draft_youtube_video_upload`: put a local video on YouTube through the
//! Google Ads API so video ads (Demand Gen, Performance Max) can use it.
//!
//! Every video ad points at a YouTube video ID. Since v23 the Ads API uploads
//! the file itself (`YouTubeVideoUploadService`), either to the advertiser's
//! brand channel or, by default, to a Google-managed channel linked to the
//! account. Publishing to YouTube is outward-facing, so the upload is drafted
//! like any write and only runs on `confirm_and_apply`.

use std::path::Path;

use serde_json::json;

use crate::client::{GoogleAdsClient, YouTubeVideoUploadRequest};
use crate::config::Config;
use crate::error::{McpGoogleAdsError, Result};
use crate::safety::guards::check_blocked_operation;
use crate::safety::preview::{store_plan, ChangePlan, PlanDispatch};

/// Formats YouTube accepts that an ad video is realistically delivered in.
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mov", "m4v", "avi", "wmv", "webm", "mkv", "mpeg", "mpg", "flv", "3gp"];

/// YouTube's own limits on the metadata.
const MAX_TITLE_CHARS: usize = 100;
const MAX_DESCRIPTION_CHARS: usize = 5000;

/// The upload is sent in one request, read whole into memory first.
const MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;

pub const VIDEO_PRIVACIES: &[&str] = &["UNLISTED", "PUBLIC", "PRIVATE"];

/// Draft the upload of `file_path` to YouTube. Returns a preview; the file is
/// read and sent only on `confirm_and_apply`.
pub fn draft_youtube_video_upload(
    config: &Config,
    customer_id: &str,
    file_path: &str,
    video_title: &str,
    video_description: Option<&str>,
    video_privacy: Option<&str>,
    channel_id: Option<&str>,
) -> Result<serde_json::Value> {
    check_blocked_operation("draft_youtube_video_upload", &config.safety)?;

    let path = Path::new(file_path);
    let metadata = std::fs::metadata(path).map_err(|e| {
        McpGoogleAdsError::Validation(format!("Cannot read video file '{file_path}': {e}"))
    })?;
    if !metadata.is_file() {
        return Err(McpGoogleAdsError::Validation(format!(
            "'{file_path}' is not a file"
        )));
    }
    if metadata.len() == 0 {
        return Err(McpGoogleAdsError::Validation(format!(
            "Video file '{file_path}' is empty"
        )));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(McpGoogleAdsError::Validation(format!(
            "Video file '{file_path}' is {} bytes; this tool sends files up to {MAX_FILE_BYTES} bytes",
            metadata.len()
        )));
    }
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .unwrap_or_default();
    if !VIDEO_EXTENSIONS.contains(&extension.as_str()) {
        return Err(McpGoogleAdsError::Validation(format!(
            "'{file_path}' does not look like a video (extension '{extension}'). Accepted: {}",
            VIDEO_EXTENSIONS.join(", ")
        )));
    }

    let title = video_title.trim();
    if title.is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "video_title cannot be empty".to_string(),
        ));
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(McpGoogleAdsError::Validation(format!(
            "video_title has {} characters; YouTube allows {MAX_TITLE_CHARS}",
            title.chars().count()
        )));
    }
    if title.contains('<') || title.contains('>') {
        return Err(McpGoogleAdsError::Validation(
            "YouTube rejects '<' and '>' in titles".to_string(),
        ));
    }
    let description = video_description.unwrap_or("").trim();
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(McpGoogleAdsError::Validation(format!(
            "video_description has {} characters; YouTube allows {MAX_DESCRIPTION_CHARS}",
            description.chars().count()
        )));
    }

    let channel_id = channel_id.map(str::trim).filter(|c| !c.is_empty());
    let privacy = video_privacy
        .map(|p| p.trim().to_uppercase())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "UNLISTED".to_string());
    if !VIDEO_PRIVACIES.contains(&privacy.as_str()) {
        return Err(McpGoogleAdsError::Validation(format!(
            "video_privacy must be one of {}",
            VIDEO_PRIVACIES.join(", ")
        )));
    }
    if channel_id.is_none() && privacy != "UNLISTED" {
        return Err(McpGoogleAdsError::Validation(
            "Without channel_id the video goes to the Google-managed channel, which only \
             accepts UNLISTED"
                .to_string(),
        ));
    }

    let cid = GoogleAdsClient::normalize_customer_id(customer_id);
    let upload = YouTubeVideoUploadRequest {
        file_path: file_path.to_string(),
        video_title: title.to_string(),
        video_description: description.to_string(),
        video_privacy: privacy.clone(),
        channel_id: channel_id.map(str::to_string),
    };

    let changes = json!({
        "file_path": file_path,
        "file_size_bytes": metadata.len(),
        "video_title": title,
        "video_description": description,
        "video_privacy": privacy,
        "channel": channel_id.unwrap_or("Google-managed channel linked to the account"),
    });

    let plan = ChangePlan::new(
        "draft_youtube_video_upload".to_string(),
        "you_tube_video_upload".to_string(),
        "new".to_string(),
        cid,
        changes,
        false,
        Vec::new(),
    )
    .with_dispatch(PlanDispatch::YouTubeVideoUpload { upload });

    let preview = plan.to_preview();
    store_plan(plan);
    Ok(preview)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::preview::{get_plan, remove_plan};

    fn temp_video(name: &str, bytes: &[u8]) -> String {
        let dir = std::env::temp_dir().join("mcp_google_ads_youtube_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn drafts_an_unlisted_upload_with_dispatch() {
        let file = temp_video("ok.mp4", b"fake video bytes");
        let preview = draft_youtube_video_upload(
            &Config::default(),
            "123-456-7890",
            &file,
            "  Ad01 Secretária  ",
            Some("desc"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(preview["changes"]["video_privacy"], "UNLISTED");
        assert_eq!(preview["changes"]["video_title"], "Ad01 Secretária");
        let plan_id = preview["plan_id"].as_str().unwrap().to_string();
        let plan = get_plan(&plan_id).unwrap();
        assert_eq!(plan.customer_id, "1234567890");
        assert!(plan.mutate_operations.is_empty());
        match plan.dispatch {
            PlanDispatch::YouTubeVideoUpload { upload } => {
                assert_eq!(upload.video_privacy, "UNLISTED");
                assert_eq!(upload.channel_id, None);
            }
            other => panic!("unexpected dispatch {other:?}"),
        }
        remove_plan(&plan_id);
    }

    #[test]
    fn public_needs_a_brand_channel() {
        let file = temp_video("public.mp4", b"x");
        let err = draft_youtube_video_upload(
            &Config::default(),
            "1",
            &file,
            "t",
            None,
            Some("public"),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("UNLISTED"), "{err}");

        draft_youtube_video_upload(
            &Config::default(),
            "1",
            &file,
            "t",
            None,
            Some("PUBLIC"),
            Some("UC123"),
        )
        .expect("PUBLIC is fine on a brand channel");
    }

    #[test]
    fn rejects_missing_empty_and_non_video_files() {
        let config = Config::default();
        assert!(draft_youtube_video_upload(&config, "1", "Z:/nope/missing.mp4", "t", None, None, None).is_err());
        let empty = temp_video("empty.mp4", b"");
        assert!(draft_youtube_video_upload(&config, "1", &empty, "t", None, None, None).is_err());
        let text = temp_video("notes.txt", b"hello");
        assert!(draft_youtube_video_upload(&config, "1", &text, "t", None, None, None).is_err());
    }

    #[test]
    fn rejects_bad_titles() {
        let config = Config::default();
        let file = temp_video("titles.mp4", b"x");
        assert!(draft_youtube_video_upload(&config, "1", &file, "  ", None, None, None).is_err());
        let long = "a".repeat(101);
        assert!(draft_youtube_video_upload(&config, "1", &file, &long, None, None, None).is_err());
        assert!(draft_youtube_video_upload(&config, "1", &file, "a <b>", None, None, None).is_err());
    }
}
