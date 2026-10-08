//! Request-building coverage for feed-only PMax, listing groups, brand lists,
//! campaign settings, and tracking — no live Google Ads credentials.

mod common;

use mcp_google_ads::config::Config;
use mcp_google_ads::safety::preview::get_plan;
use mcp_google_ads::tools::campaign_settings::set_pmax_campaign_settings;
use mcp_google_ads::tools::listing_groups::{
    get_pmax_listing_group_tree, set_pmax_listing_groups, ListingGroupSpec, ListingGroupValue,
};
use mcp_google_ads::tools::pmax::{create_pmax_campaign, CreatePmaxCampaignParams};
use mcp_google_ads::{CreatePmaxCampaignToolParams, SetTrackingToolParams};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

fn stored_ops(preview: &serde_json::Value) -> Vec<serde_json::Value> {
    let plan_id = preview["plan_id"].as_str().unwrap();
    get_plan(plan_id).unwrap().mutate_operations
}

#[test]
fn existing_create_pmax_json_without_new_fields_still_deserializes() {
    let params: CreatePmaxCampaignToolParams = serde_json::from_value(json!({
        "campaign_name": "Legacy PMax",
        "daily_budget": 25.0,
        "bidding_strategy": "MAXIMIZE_CONVERSIONS",
        "final_urls": ["https://example.com"],
        "headlines": ["H1", "H2", "H3"],
        "long_headlines": ["Long H1"],
        "descriptions": ["D1", "D2"],
        "business_name": "BizCo",
        "geo_target_ids": ["2840"],
        "start_paused": true
    }))
    .expect("pre-existing create_pmax_campaign payloads must keep working");
    assert!(params.merchant_id.is_none());
    assert_eq!(params.headlines.len(), 3);
    assert!(params.language_ids.is_empty());
}

#[test]
fn unknown_create_pmax_field_is_rejected() {
    let err = serde_json::from_value::<CreatePmaxCampaignToolParams>(json!({
        "campaign_name": "X",
        "daily_budget": 1.0,
        "bidding_strategy": "MAXIMIZE_CONVERSIONS",
        "final_urls": ["https://example.com"],
        "geo_target_ids": ["2300"],
        "sales_country": "GR"
    }))
    .expect_err("sales_country is not a field — feed_label replaced it");
    assert!(err.to_string().contains("sales_country"));
}

#[test]
fn set_tracking_params_accept_empty_suffix() {
    let params: SetTrackingToolParams = serde_json::from_value(json!({
        "level": "campaign",
        "campaign_id": "55",
        "final_url_suffix": ""
    }))
    .unwrap();
    assert_eq!(params.final_url_suffix.as_deref(), Some(""));
}

#[tokio::test]
async fn get_listing_group_tree_is_readable() {
    let (mock, client) = common::spawn_mock_google_ads().await;
    Mock::given(method("POST"))
        .and(path("/v25/customers/1234567890/googleAds:search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                {
                    "assetGroupListingGroupFilter": {
                        "resourceName": "customers/1234567890/assetGroupListingGroupFilters/111~1",
                        "id": "1",
                        "type": "SUBDIVISION",
                        "listingSource": "SHOPPING"
                    }
                },
                {
                    "assetGroupListingGroupFilter": {
                        "resourceName": "customers/1234567890/assetGroupListingGroupFilters/111~2",
                        "id": "2",
                        "type": "UNIT_INCLUDED",
                        "listingSource": "SHOPPING",
                        "parentListingGroupFilter": "customers/1234567890/assetGroupListingGroupFilters/111~1",
                        "caseValue": { "productBrand": { "value": "Nike" } }
                    }
                },
                {
                    "assetGroupListingGroupFilter": {
                        "resourceName": "customers/1234567890/assetGroupListingGroupFilters/111~3",
                        "id": "3",
                        "type": "UNIT_EXCLUDED",
                        "listingSource": "SHOPPING",
                        "parentListingGroupFilter": "customers/1234567890/assetGroupListingGroupFilters/111~1",
                        "caseValue": { "productBrand": {} }
                    }
                }
            ]
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let body = get_pmax_listing_group_tree(&client, "1234567890", "111")
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["asset_group_id"], "111");
    assert_eq!(parsed["tree"]["node_count"], 3);
    assert_eq!(parsed["tree"]["roots"][0]["children"][0]["value"], "Nike");
    assert_eq!(
        parsed["tree"]["roots"][0]["children"][1]["everything_else"],
        true
    );
}

#[tokio::test]
async fn set_listing_groups_removes_existing_then_creates() {
    let (mock, client) = common::spawn_mock_google_ads().await;
    Mock::given(method("POST"))
        .and(path("/v25/customers/1234567890/googleAds:search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                {
                    "assetGroupListingGroupFilter": {
                        "resourceName": "customers/1234567890/assetGroupListingGroupFilters/111~9",
                        "id": "9",
                        "type": "UNIT_INCLUDED",
                        "listingSource": "SHOPPING"
                    }
                }
            ]
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let config = common::test_config();
    let spec = ListingGroupSpec {
        include_only: Some(vec![ListingGroupValue {
            dimension: "BRAND".into(),
            value: Some("Nike".into()),
            category_id: None,
        }]),
        exclude: None,
        partitions: None,
    };
    let preview = set_pmax_listing_groups(&client, &config, "1234567890", "111", spec)
        .await
        .unwrap();
    assert_eq!(preview["operation"], "set_pmax_listing_groups");
    let ops = stored_ops(&preview);
    assert_eq!(
        ops[0]["assetGroupListingGroupFilterOperation"]["remove"],
        "customers/1234567890/assetGroupListingGroupFilters/111~9"
    );
    assert_eq!(
        ops[1]["assetGroupListingGroupFilterOperation"]["create"]["type"],
        "SUBDIVISION"
    );
}

#[tokio::test]
async fn set_pmax_campaign_settings_merges_existing_automation() {
    let (mock, client) = common::spawn_mock_google_ads().await;
    Mock::given(method("POST"))
        .and(path("/v25/customers/1234567890/googleAds:search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{
                "campaign": {
                    "assetAutomationSettings": [
                        {
                            "assetAutomationType": "GENERATE_IMAGE_ENHANCEMENT",
                            "assetAutomationStatus": "OPTED_IN"
                        }
                    ]
                }
            }]
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let config = common::test_config();
    let preview = set_pmax_campaign_settings(
        &client,
        &config,
        "1234567890",
        "55",
        Some(true),
        Some(false),
    )
    .await
    .unwrap();
    let ops = stored_ops(&preview);
    let settings = ops[0]["campaignOperation"]["update"]["assetAutomationSettings"]
        .as_array()
        .unwrap();
    assert_eq!(settings.len(), 3);
    assert_eq!(
        settings[0]["assetAutomationType"],
        "GENERATE_IMAGE_ENHANCEMENT"
    );
    assert!(settings.iter().any(|s| {
        s["assetAutomationType"] == "FINAL_URL_EXPANSION_TEXT_ASSET_AUTOMATION"
            && s["assetAutomationStatus"] == "OPTED_OUT"
    }));
    assert_eq!(
        ops[0]["campaignOperation"]["updateMask"],
        "assetAutomationSettings"
    );
}

#[test]
fn maximize_conversion_value_with_target_roas_in_payload() {
    let config = Config::default();
    let params = CreatePmaxCampaignParams {
        config: &config,
        customer_id: "1234567890",
        campaign_name: "ROAS PMax",
        daily_budget: 10.0,
        bidding_strategy: "MAXIMIZE_CONVERSION_VALUE",
        final_urls: vec!["https://example.com".into()],
        headlines: vec!["H1".into(), "H2".into(), "H3".into()],
        long_headlines: vec!["LH".into()],
        descriptions: vec!["D1".into(), "D2".into()],
        business_name: "Biz",
        geo_target_ids: vec!["2300".into()],
        start_paused: true,
        language_ids: vec!["1022".into()],
        merchant_id: Some("555"),
        feed_label: Some("GR"),
        target_cpa: None,
        target_roas: Some(4.0),
        url_expansion_opt_out: None,
        automatically_created_assets: None,
        enable_local: Some(true),
        listing_group: None,
        image_assets: vec![],
    };
    let preview = create_pmax_campaign(&params).unwrap();
    let ops = stored_ops(&preview);
    let campaign = ops
        .iter()
        .find_map(|op| op.pointer("/campaignOperation/create"))
        .unwrap();
    assert_eq!(campaign["maximizeConversionValue"]["targetRoas"], 4.0);
    assert_eq!(campaign["shoppingSetting"]["enableLocal"], true);
    assert!(campaign.get("maximizeConversions").is_none());
}
