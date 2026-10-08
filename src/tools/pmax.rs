use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::Config;
use crate::error::{McpGoogleAdsError, Result};
use crate::models::{AdStatus, NextActionHint};
use crate::safety::guards::{
    check_blocked_operation, check_budget_cap, validate_description, validate_final_url,
    validate_headline,
};
use crate::safety::preview::{store_plan, ChangePlan};
use crate::tools::assets::VALID_ASSET_GROUP_FIELD_TYPES;
use crate::tools::campaign_settings::asset_automation_settings_for_create;
use crate::tools::listing_groups::{build_listing_group_create_operations, ListingGroupSpec};
use crate::tools::shared_sets::validate_numeric_id;

/// Convert a dollar amount to micros (Google Ads uses micros: $1 = 1_000_000).
fn dollars_to_micros(dollars: f64) -> i64 {
    (dollars * 1_000_000.0) as i64
}

/// Link an existing asset (typically an image) onto a PMax asset group.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageAssetLink {
    pub asset_id: String,
    /// Field type: MARKETING_IMAGE, SQUARE_MARKETING_IMAGE, LOGO, …
    pub field_type: String,
}

/// Parameters for creating a Performance Max campaign.
pub struct CreatePmaxCampaignParams<'a> {
    pub config: &'a Config,
    pub customer_id: &'a str,
    pub campaign_name: &'a str,
    pub daily_budget: f64,
    pub bidding_strategy: &'a str,
    pub final_urls: Vec<String>,
    pub headlines: Vec<String>,
    pub long_headlines: Vec<String>,
    pub descriptions: Vec<String>,
    pub business_name: &'a str,
    pub geo_target_ids: Vec<String>,
    pub start_paused: bool,
    /// Language constant IDs (e.g. `1022` for Greek). Optional.
    pub language_ids: Vec<String>,
    /// Merchant Center account ID. When set, this is a retail/feed PMax and
    /// text assets become optional.
    pub merchant_id: Option<&'a str>,
    /// Merchant Center feed label (replaces the deprecated `sales_country`).
    /// Country codes such as `GR` are valid feed labels.
    pub feed_label: Option<&'a str>,
    pub target_cpa: Option<f64>,
    pub target_roas: Option<f64>,
    /// Maps to FINAL_URL_EXPANSION_TEXT_ASSET_AUTOMATION (the API no longer
    /// has `campaign.url_expansion_opt_out`).
    pub url_expansion_opt_out: Option<bool>,
    /// Maps to TEXT_ASSET_AUTOMATION (automatically created text assets).
    pub automatically_created_assets: Option<bool>,
    pub enable_local: Option<bool>,
    pub listing_group: Option<ListingGroupSpec>,
    pub image_assets: Vec<ImageAssetLink>,
}

/// Create a Performance Max campaign as an atomic batch.
///
/// Uses temporary resource IDs: -1 for budget, -2 for campaign, -3 for asset group.
/// Text assets (headlines, long headlines, descriptions) are created and linked to the asset group.
/// Image assets require separate upload via `upload_image_asset`.
///
/// Validations:
/// - 3-15 headlines (max 30 chars each)
/// - 1-5 long headlines (max 90 chars each)
/// - 2-5 descriptions (max 90 chars each)
/// - business_name max 25 chars
/// - budget cap check
/// - at least one final URL
///
/// Returns a ChangePlan preview that must be confirmed via `confirm_and_apply`.
pub fn create_pmax_campaign(params: &CreatePmaxCampaignParams) -> Result<serde_json::Value> {
    check_blocked_operation("create_pmax_campaign", &params.config.safety)?;
    check_budget_cap(params.daily_budget, &params.config.safety)?;

    let is_retail = params.merchant_id.is_some();
    if is_retail {
        if let Some(mid) = params.merchant_id {
            validate_numeric_id("merchant_id", mid)?;
        }
        if let Some(label) = params.feed_label {
            validate_feed_label(label)?;
        }
        validate_optional_text_assets(
            &params.headlines,
            &params.long_headlines,
            &params.descriptions,
            Some(params.business_name).filter(|s| !s.is_empty()),
        )?;
    } else {
        // Standard (non-retail) PMax still requires a full text asset set so
        // existing callers of create_pmax_campaign keep the same contract.
        if params.headlines.len() < 3 || params.headlines.len() > 15 {
            return Err(McpGoogleAdsError::Validation(format!(
                "PMax requires 3-15 headlines, got {}",
                params.headlines.len()
            )));
        }
        if params.long_headlines.is_empty() || params.long_headlines.len() > 5 {
            return Err(McpGoogleAdsError::Validation(format!(
                "PMax requires 1-5 long headlines, got {}",
                params.long_headlines.len()
            )));
        }
        if params.descriptions.len() < 2 || params.descriptions.len() > 5 {
            return Err(McpGoogleAdsError::Validation(format!(
                "PMax requires 2-5 descriptions, got {}",
                params.descriptions.len()
            )));
        }
        for headline in &params.headlines {
            validate_headline(headline)?;
        }
        for lh in &params.long_headlines {
            validate_description(lh)?;
        }
        for desc in &params.descriptions {
            validate_description(desc)?;
        }
        if params.business_name.len() > 25 {
            return Err(McpGoogleAdsError::Validation(format!(
                "Business name exceeds 25 character limit ({} chars)",
                params.business_name.len()
            )));
        }
    }

    if params.final_urls.is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "At least one final URL is required".to_string(),
        ));
    }
    for url in &params.final_urls {
        validate_final_url(url)?;
    }

    let cid = crate::client::GoogleAdsClient::normalize_customer_id(params.customer_id);
    let mut operations: Vec<serde_json::Value> = Vec::new();

    // 1. Campaign budget (temp resource ID -1)
    let budget_resource = format!("customers/{}/campaignBudgets/-1", cid);
    operations.push(json!({
        "campaignBudgetOperation": {
            "create": {
                "name": format!("{} Budget", params.campaign_name),
                "amountMicros": dollars_to_micros(params.daily_budget).to_string(),
                "deliveryMethod": "STANDARD",
                // Must be explicit: budgets default to shared, and Google rejects
                // a shared budget on a Performance Max campaign with
                // BIDDING_STRATEGY_TYPE_INCOMPATIBLE_WITH_SHARED_BUDGET.
                "explicitlyShared": false,
                "resourceName": budget_resource
            }
        }
    }));

    // 2. Campaign (temp resource ID -2)
    let campaign_resource = format!("customers/{}/campaigns/-2", cid);
    let resolved_status = if params.start_paused {
        AdStatus::Paused
    } else {
        AdStatus::Enabled
    };

    let mut campaign_create = json!({
        "name": params.campaign_name,
        "status": resolved_status.as_api_str(),
        "advertisingChannelType": "PERFORMANCE_MAX",
        "campaignBudget": budget_resource,
        // Required on every campaign create since the EU political ads
        // regulation; omitting it fails with fieldError=REQUIRED.
        "containsEuPoliticalAdvertising": "DOES_NOT_CONTAIN_EU_POLITICAL_ADVERTISING",
        "resourceName": campaign_resource
    });

    apply_pmax_bidding(
        &mut campaign_create,
        params.bidding_strategy,
        params.target_cpa,
        params.target_roas,
    );

    if let Some(mid) = params.merchant_id {
        let mut shopping = json!({ "merchantId": mid });
        if let Some(label) = params.feed_label {
            shopping["feedLabel"] = json!(label);
        }
        if let Some(local) = params.enable_local {
            shopping["enableLocal"] = json!(local);
        }
        campaign_create["shoppingSetting"] = shopping;
    }

    if let Some(settings) = asset_automation_settings_for_create(
        params.url_expansion_opt_out,
        params.automatically_created_assets,
    ) {
        campaign_create["assetAutomationSettings"] = json!(settings);
    }

    operations.push(json!({
        "campaignOperation": {
            "create": campaign_create
        }
    }));

    // 3. Geo targets (e.g. Greece = 2300)
    for geo_id in &params.geo_target_ids {
        operations.push(json!({
            "campaignCriterionOperation": {
                "create": {
                    "campaign": campaign_resource,
                    "location": {
                        "geoTargetConstant": format!("geoTargetConstants/{}", geo_id)
                    }
                }
            }
        }));
    }

    // 3b. Language targets (e.g. Greek = 1022)
    for lang_id in &params.language_ids {
        operations.push(json!({
            "campaignCriterionOperation": {
                "create": {
                    "campaign": campaign_resource,
                    "language": {
                        "languageConstant": format!("languageConstants/{}", lang_id)
                    }
                }
            }
        }));
    }

    // 4. Asset group (temp resource ID -3) — inherits the campaign-level
    // status so a PMax campaign that opted into ENABLED doesn't ship with
    // a paused asset group blocking traffic.
    let asset_group_resource = format!("customers/{}/assetGroups/-3", cid);
    operations.push(json!({
        "assetGroupOperation": {
            "create": {
                "name": format!("{} Asset Group", params.campaign_name),
                "campaign": campaign_resource,
                "finalUrls": params.final_urls,
                "status": resolved_status.as_api_str(),
                "resourceName": asset_group_resource
            }
        }
    }));

    // 5. Text / image assets — omitted entirely for feed-only retail PMax.
    let mut temp_asset_id: i64 = -100;
    let business_name = if params.business_name.is_empty() {
        None
    } else {
        Some(params.business_name)
    };
    push_pmax_assets(
        &mut operations,
        &cid,
        &asset_group_resource,
        &mut temp_asset_id,
        &params.headlines,
        &params.long_headlines,
        &params.descriptions,
        business_name,
        &params.image_assets,
    )?;

    // 6. Listing group tree — required for retail PMax asset groups.
    if is_retail {
        let spec = params.listing_group.clone().unwrap_or_default();
        let (lg_ops, _) =
            build_listing_group_create_operations(&cid, "-3", &asset_group_resource, &spec, -4)?;
        operations.extend(lg_ops);
    }

    let changes = json!({
        "campaign_name": params.campaign_name,
        "daily_budget": params.daily_budget,
        "bidding_strategy": params.bidding_strategy,
        "target_cpa": params.target_cpa,
        "target_roas": params.target_roas,
        "channel_type": "PERFORMANCE_MAX",
        "headlines_count": params.headlines.len(),
        "long_headlines_count": params.long_headlines.len(),
        "descriptions_count": params.descriptions.len(),
        "business_name": params.business_name,
        "final_urls": params.final_urls,
        "geo_targets": params.geo_target_ids,
        "language_ids": params.language_ids,
        "merchant_id": params.merchant_id,
        "feed_label": params.feed_label,
        "url_expansion_opt_out": params.url_expansion_opt_out,
        "automatically_created_assets": params.automatically_created_assets,
        "start_paused": params.start_paused,
        "feed_only": is_retail && params.headlines.is_empty(),
        "note": "Image assets can be linked at create time via image_assets, or uploaded later with upload_image_asset + link_asset_to_asset_group. Retail PMax (merchant_id set) creates a listing group tree; text assets are optional."
    });

    let mut plan = ChangePlan::new(
        "create_pmax_campaign".to_string(),
        "campaign".to_string(),
        "new".to_string(),
        cid,
        changes,
        false,
        operations,
    )
    .with_status_after_apply(resolved_status);

    if resolved_status == AdStatus::Paused {
        plan = plan.with_next_action_hint(NextActionHint::enable_parent_campaign_after_create(
            "customers/{cid}/campaigns/-2",
        ));
    }

    let preview = plan.to_preview();
    store_plan(plan);
    Ok(preview)
}

fn dollars_to_micros_str(dollars: f64) -> String {
    dollars_to_micros(dollars).to_string()
}

fn apply_pmax_bidding(
    campaign: &mut serde_json::Value,
    strategy: &str,
    target_cpa: Option<f64>,
    target_roas: Option<f64>,
) {
    let obj = match campaign.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    match strategy {
        "MAXIMIZE_CONVERSION_VALUE" | "TARGET_ROAS" => {
            let mut mcv = json!({});
            if let Some(roas) = target_roas {
                mcv["targetRoas"] = json!(roas);
            }
            obj.insert("maximizeConversionValue".to_string(), mcv);
        }
        "MAXIMIZE_CONVERSIONS" | "TARGET_CPA" => {
            let mut mc = json!({});
            if let Some(cpa) = target_cpa {
                mc["targetCpaMicros"] = json!(dollars_to_micros_str(cpa));
            }
            obj.insert("maximizeConversions".to_string(), mc);
        }
        other => {
            // Unknown strategy — default to Maximize Conversions, matching the
            // previous create_pmax_campaign behaviour.
            let _ = other;
            obj.insert("maximizeConversions".to_string(), json!({}));
        }
    }
}

fn validate_feed_label(label: &str) -> Result<()> {
    if label.is_empty() || label.len() > 20 {
        return Err(McpGoogleAdsError::Validation(format!(
            "feed_label must be 1-20 characters (uppercase letters, digits, hyphen, underscore), got '{}'",
            label
        )));
    }
    if !label
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(McpGoogleAdsError::Validation(format!(
            "feed_label '{label}' must contain only uppercase letters, digits, hyphens, and underscores \
             (a country code such as GR is a valid feed label; sales_country is deprecated)"
        )));
    }
    Ok(())
}

/// Validate text assets when they are provided. Empty is allowed (feed-only).
/// Any non-empty set must meet PMax minimums.
pub fn validate_optional_text_assets(
    headlines: &[String],
    long_headlines: &[String],
    descriptions: &[String],
    business_name: Option<&str>,
) -> Result<()> {
    if !headlines.is_empty() {
        if headlines.len() < 3 || headlines.len() > 15 {
            return Err(McpGoogleAdsError::Validation(format!(
                "PMax requires 3-15 headlines when headlines are provided, got {}",
                headlines.len()
            )));
        }
        for headline in headlines {
            validate_headline(headline)?;
        }
    }
    if !long_headlines.is_empty() {
        if long_headlines.len() > 5 {
            return Err(McpGoogleAdsError::Validation(format!(
                "PMax allows at most 5 long headlines, got {}",
                long_headlines.len()
            )));
        }
        for lh in long_headlines {
            validate_description(lh)?;
        }
    }
    if !descriptions.is_empty() {
        if descriptions.len() < 2 || descriptions.len() > 5 {
            return Err(McpGoogleAdsError::Validation(format!(
                "PMax requires 2-5 descriptions when descriptions are provided, got {}",
                descriptions.len()
            )));
        }
        for desc in descriptions {
            validate_description(desc)?;
        }
    }
    if let Some(name) = business_name {
        if name.chars().count() > 25 {
            return Err(McpGoogleAdsError::Validation(format!(
                "Business name exceeds 25 character limit ({} chars)",
                name.chars().count()
            )));
        }
    }
    Ok(())
}

/// Append text-asset creates + asset-group links, and links to existing image assets.
#[allow(clippy::too_many_arguments)]
pub fn push_pmax_assets(
    operations: &mut Vec<serde_json::Value>,
    cid: &str,
    asset_group_resource: &str,
    temp_asset_id: &mut i64,
    headlines: &[String],
    long_headlines: &[String],
    descriptions: &[String],
    business_name: Option<&str>,
    image_assets: &[ImageAssetLink],
) -> Result<()> {
    fn push_text(
        operations: &mut Vec<serde_json::Value>,
        cid: &str,
        asset_group_resource: &str,
        temp_asset_id: &mut i64,
        text: &str,
        field_type: &str,
    ) {
        let asset_resource = format!("customers/{}/assets/{}", cid, temp_asset_id);
        operations.push(json!({
            "assetOperation": {
                "create": {
                    "resourceName": asset_resource,
                    "textAsset": { "text": text }
                }
            }
        }));
        operations.push(json!({
            "assetGroupAssetOperation": {
                "create": {
                    "assetGroup": asset_group_resource,
                    "asset": asset_resource,
                    "fieldType": field_type
                }
            }
        }));
        *temp_asset_id -= 1;
    }

    for headline in headlines {
        push_text(
            operations,
            cid,
            asset_group_resource,
            temp_asset_id,
            headline,
            "HEADLINE",
        );
    }
    for lh in long_headlines {
        push_text(
            operations,
            cid,
            asset_group_resource,
            temp_asset_id,
            lh,
            "LONG_HEADLINE",
        );
    }
    for desc in descriptions {
        push_text(
            operations,
            cid,
            asset_group_resource,
            temp_asset_id,
            desc,
            "DESCRIPTION",
        );
    }
    if let Some(name) = business_name.filter(|s| !s.is_empty()) {
        push_text(
            operations,
            cid,
            asset_group_resource,
            temp_asset_id,
            name,
            "BUSINESS_NAME",
        );
    }

    for link in image_assets {
        if link.asset_id.trim().is_empty() {
            return Err(McpGoogleAdsError::Validation(
                "image_assets.asset_id must not be empty".to_string(),
            ));
        }
        validate_numeric_id("image_assets.asset_id", link.asset_id.trim())?;
        let field_type = link.field_type.to_uppercase();
        if !VALID_ASSET_GROUP_FIELD_TYPES.contains(&field_type.as_str()) {
            return Err(McpGoogleAdsError::Validation(format!(
                "Invalid asset group field type '{}'. Must be one of: {}",
                field_type,
                VALID_ASSET_GROUP_FIELD_TYPES.join(", ")
            )));
        }
        operations.push(json!({
            "assetGroupAssetOperation": {
                "create": {
                    "assetGroup": asset_group_resource,
                    "asset": format!("customers/{}/assets/{}", cid, link.asset_id.trim()),
                    "fieldType": field_type
                }
            }
        }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn default_params(config: &Config) -> CreatePmaxCampaignParams<'_> {
        CreatePmaxCampaignParams {
            config,
            customer_id: "123-456-7890",
            campaign_name: "Test PMax",
            daily_budget: 10.0,
            bidding_strategy: "MAXIMIZE_CONVERSIONS",
            final_urls: vec!["https://example.com".to_string()],
            headlines: vec![
                "Headline 1".to_string(),
                "Headline 2".to_string(),
                "Headline 3".to_string(),
            ],
            long_headlines: vec!["Long Headline 1".to_string()],
            descriptions: vec!["Description 1".to_string(), "Description 2".to_string()],
            business_name: "Test Business",
            geo_target_ids: vec!["2840".to_string()],
            start_paused: true,
            language_ids: vec![],
            merchant_id: None,
            feed_label: None,
            target_cpa: None,
            target_roas: None,
            url_expansion_opt_out: None,
            automatically_created_assets: None,
            enable_local: None,
            listing_group: None,
            image_assets: vec![],
        }
    }

    #[test]
    fn test_create_pmax_success() {
        let config = Config::default();
        let params = default_params(&config);
        let result = create_pmax_campaign(&params);
        assert!(result.is_ok());
        let preview = result.ok().unwrap_or_default();
        assert_eq!(preview["operation"], "create_pmax_campaign");
        assert_eq!(preview["status"], "PENDING_CONFIRMATION");
    }

    #[test]
    fn test_create_pmax_budget_cap_exceeded() {
        let mut config = Config::default();
        config.safety.max_daily_budget = 5.0;
        let mut params = default_params(&config);
        params.daily_budget = 10.0;
        assert!(create_pmax_campaign(&params).is_err());
    }

    #[test]
    fn test_create_pmax_too_few_headlines() {
        let config = Config::default();
        let mut params = default_params(&config);
        params.headlines = vec!["H1".to_string(), "H2".to_string()];
        let result = create_pmax_campaign(&params);
        assert!(result.is_err());
        let err = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("3-15 headlines"));
    }

    #[test]
    fn test_create_pmax_too_many_headlines() {
        let config = Config::default();
        let mut params = default_params(&config);
        params.headlines = (0..16).map(|i| format!("H{}", i)).collect();
        assert!(create_pmax_campaign(&params).is_err());
    }

    #[test]
    fn test_create_pmax_no_long_headlines() {
        let config = Config::default();
        let mut params = default_params(&config);
        params.long_headlines = vec![];
        assert!(create_pmax_campaign(&params).is_err());
    }

    #[test]
    fn test_create_pmax_too_few_descriptions() {
        let config = Config::default();
        let mut params = default_params(&config);
        params.descriptions = vec!["D1".to_string()];
        assert!(create_pmax_campaign(&params).is_err());
    }

    #[test]
    fn test_create_pmax_headline_too_long() {
        let config = Config::default();
        let mut params = default_params(&config);
        params.headlines[0] = "A".repeat(31);
        assert!(create_pmax_campaign(&params).is_err());
    }

    #[test]
    fn test_create_pmax_business_name_too_long() {
        let config = Config::default();
        let mut params = default_params(&config);
        params.business_name = Box::leak("A".repeat(26).into_boxed_str());
        assert!(create_pmax_campaign(&params).is_err());
    }

    #[test]
    fn test_create_pmax_no_final_urls() {
        let config = Config::default();
        let mut params = default_params(&config);
        params.final_urls = vec![];
        assert!(create_pmax_campaign(&params).is_err());
    }

    #[test]
    fn test_create_pmax_blocked() {
        let mut config = Config::default();
        config.safety.blocked_operations = vec!["create_pmax_campaign".to_string()];
        let params = default_params(&config);
        assert!(create_pmax_campaign(&params).is_err());
    }

    #[test]
    fn test_create_pmax_start_paused_true_sets_paused_in_payload() {
        let config = Config::default();
        let mut params = default_params(&config);
        params.start_paused = true;
        let preview = create_pmax_campaign(&params).unwrap();
        assert_eq!(preview["status_after_apply"], "PAUSED");
    }

    #[test]
    fn test_create_pmax_start_paused_false_sets_enabled_in_payload() {
        // Regression for the v0.2.x bug: `start_paused=false` was ignored and
        // the campaign always shipped PAUSED. The fix wires start_paused into
        // the actual mutate operation status.
        let config = Config::default();
        let mut params = default_params(&config);
        params.start_paused = false;
        let preview = create_pmax_campaign(&params).unwrap();
        assert_eq!(preview["status_after_apply"], "ENABLED");
        // No hint when ENABLED — the workflow is complete.
        assert!(preview.get("next_action_hint").is_none() || preview["next_action_hint"].is_null());
    }

    /// Pull the raw mutate operations back off the stored plan.
    fn pmax_ops() -> Vec<serde_json::Value> {
        let config = Config::default();
        let params = default_params(&config);
        let preview = create_pmax_campaign(&params).unwrap();
        let plan_id = preview["plan_id"].as_str().unwrap_or_default();
        let plan = crate::safety::preview::get_plan(plan_id).expect("plan stored");
        plan.mutate_operations.clone()
    }

    #[test]
    fn test_create_pmax_budget_not_shared() {
        // Budgets default to shared server-side, and PMax rejects a shared budget
        // with BIDDING_STRATEGY_TYPE_INCOMPATIBLE_WITH_SHARED_BUDGET. draft_campaign
        // got this fix in 5670d2d; create_pmax_campaign was missed.
        let ops = pmax_ops();
        let budget_create = ops
            .iter()
            .find_map(|op| op.pointer("/campaignBudgetOperation/create"))
            .expect("budget operation present");
        assert_eq!(budget_create["explicitlyShared"], json!(false));
    }

    #[test]
    fn test_create_pmax_sets_eu_political_advertising() {
        // Required on every campaign create; without it the whole mutate fails
        // with fieldError=REQUIRED on contains_eu_political_advertising.
        let ops = pmax_ops();
        let campaign_create = ops
            .iter()
            .find_map(|op| op.pointer("/campaignOperation/create"))
            .expect("campaign operation present");
        assert_eq!(
            campaign_create["containsEuPoliticalAdvertising"],
            json!("DOES_NOT_CONTAIN_EU_POLITICAL_ADVERTISING")
        );
    }

    #[test]
    fn test_feed_only_pmax_skips_text_assets() {
        let config = Config::default();
        let mut params = default_params(&config);
        params.merchant_id = Some("123456789");
        params.feed_label = Some("GR");
        params.language_ids = vec!["1022".into()];
        params.geo_target_ids = vec!["2300".into()];
        params.headlines = vec![];
        params.long_headlines = vec![];
        params.descriptions = vec![];
        params.business_name = "";
        params.bidding_strategy = "MAXIMIZE_CONVERSION_VALUE";
        params.target_roas = Some(3.5);
        params.url_expansion_opt_out = Some(true);
        params.automatically_created_assets = Some(false);

        let preview = create_pmax_campaign(&params).unwrap();
        assert_eq!(preview["changes"]["feed_only"], true);
        assert_eq!(preview["status_after_apply"], "PAUSED");

        let plan_id = preview["plan_id"].as_str().unwrap();
        let plan = crate::safety::preview::get_plan(plan_id).unwrap();
        let campaign = plan
            .mutate_operations
            .iter()
            .find_map(|op| op.pointer("/campaignOperation/create"))
            .unwrap();
        assert_eq!(campaign["shoppingSetting"]["merchantId"], "123456789");
        assert_eq!(campaign["shoppingSetting"]["feedLabel"], "GR");
        assert_eq!(campaign["maximizeConversionValue"]["targetRoas"], 3.5);
        let automations = campaign["assetAutomationSettings"].as_array().unwrap();
        assert_eq!(
            automations[0]["assetAutomationType"],
            "FINAL_URL_EXPANSION_TEXT_ASSET_AUTOMATION"
        );
        assert_eq!(automations[0]["assetAutomationStatus"], "OPTED_OUT");

        assert!(plan.mutate_operations.iter().any(|op| op
            .pointer("/campaignCriterionOperation/create/language/languageConstant")
            == Some(&json!("languageConstants/1022"))));
        assert!(plan.mutate_operations.iter().any(|op| {
            op.pointer("/assetGroupListingGroupFilterOperation/create/type")
                == Some(&json!("UNIT_INCLUDED"))
        }));
        assert!(plan
            .mutate_operations
            .iter()
            .all(|op| op.get("assetOperation").is_none()));
    }

    #[test]
    fn test_feed_only_without_merchant_id_still_requires_headlines() {
        let config = Config::default();
        let mut params = default_params(&config);
        params.headlines = vec![];
        let err = create_pmax_campaign(&params).unwrap_err().to_string();
        assert!(err.contains("3-15 headlines"));
    }

    #[test]
    fn test_invalid_feed_label_rejected() {
        let config = Config::default();
        let mut params = default_params(&config);
        params.merchant_id = Some("1");
        params.feed_label = Some("gr"); // must be uppercase
        params.headlines = vec![];
        params.long_headlines = vec![];
        params.descriptions = vec![];
        params.business_name = "";
        assert!(create_pmax_campaign(&params).is_err());
    }

    #[test]
    fn test_retail_with_text_assets_still_emits_them() {
        let config = Config::default();
        let mut params = default_params(&config);
        params.merchant_id = Some("99");
        let preview = create_pmax_campaign(&params).unwrap();
        let plan_id = preview["plan_id"].as_str().unwrap();
        let plan = crate::safety::preview::get_plan(plan_id).unwrap();
        assert!(plan
            .mutate_operations
            .iter()
            .any(|op| op.get("assetOperation").is_some()));
        assert_eq!(preview["changes"]["feed_only"], false);
    }
}
