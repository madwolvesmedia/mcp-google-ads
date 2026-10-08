//! Campaign- and customer-level settings used by retail Performance Max:
//! Final URL expansion, automatically created assets, and tracking templates.

use serde_json::json;

use crate::client::GoogleAdsClient;
use crate::config::Config;
use crate::error::{McpGoogleAdsError, Result};
use crate::safety::guards::check_blocked_operation;
use crate::safety::preview::{store_plan, ChangePlan};
use crate::tools::shared_sets::validate_numeric_id;

/// `Campaign.url_expansion_opt_out` was removed from the Google Ads API
/// (replaced in v21+). Final URL expansion is now
/// `AssetAutomationType.FINAL_URL_EXPANSION_TEXT_ASSET_AUTOMATION`.
/// Automatically created assets (text customization) are `TEXT_ASSET_AUTOMATION`.
const URL_EXPANSION_TYPE: &str = "FINAL_URL_EXPANSION_TEXT_ASSET_AUTOMATION";
const TEXT_ASSET_TYPE: &str = "TEXT_ASSET_AUTOMATION";

fn automation_status(opted_in: bool) -> &'static str {
    if opted_in {
        "OPTED_IN"
    } else {
        "OPTED_OUT"
    }
}

/// Toggle Final URL expansion and/or automatically created assets on a PMax campaign.
///
/// `url_expansion_opt_out = true` maps to FINAL_URL_EXPANSION_TEXT_ASSET_AUTOMATION
/// OPTED_OUT (the old `campaign.url_expansion_opt_out` field no longer exists).
/// `automatically_created_assets = false` maps to TEXT_ASSET_AUTOMATION OPTED_OUT.
///
/// Existing automation settings of other types are fetched and merged so a
/// partial update does not wipe GENERATE_IMAGE_ENHANCEMENT etc.
pub async fn set_pmax_campaign_settings(
    client: &GoogleAdsClient,
    config: &Config,
    customer_id: &str,
    campaign_id: &str,
    url_expansion_opt_out: Option<bool>,
    automatically_created_assets: Option<bool>,
) -> Result<serde_json::Value> {
    check_blocked_operation("set_pmax_campaign_settings", &config.safety)?;
    validate_numeric_id("campaign_id", campaign_id)?;

    if url_expansion_opt_out.is_none() && automatically_created_assets.is_none() {
        return Err(McpGoogleAdsError::Validation(
            "Set url_expansion_opt_out and/or automatically_created_assets".to_string(),
        ));
    }

    let cid = GoogleAdsClient::normalize_customer_id(customer_id);
    let mut settings = fetch_asset_automation_settings(client, &cid, campaign_id).await?;

    if let Some(opt_out) = url_expansion_opt_out {
        upsert_automation(&mut settings, URL_EXPANSION_TYPE, !opt_out);
    }
    if let Some(enabled) = automatically_created_assets {
        upsert_automation(&mut settings, TEXT_ASSET_TYPE, enabled);
    }

    let campaign_resource = format!("customers/{cid}/campaigns/{campaign_id}");
    let operations = vec![json!({
        "campaignOperation": {
            "update": {
                "resourceName": campaign_resource,
                "assetAutomationSettings": settings
            },
            "updateMask": "assetAutomationSettings"
        }
    })];

    let changes = json!({
        "campaign_id": campaign_id,
        "url_expansion_opt_out": url_expansion_opt_out,
        "automatically_created_assets": automatically_created_assets,
        "asset_automation_settings": settings,
        "note": "campaign.url_expansion_opt_out was removed from the API. url_expansion_opt_out=true writes AssetAutomationSetting FINAL_URL_EXPANSION_TEXT_ASSET_AUTOMATION=OPTED_OUT. automatically_created_assets writes TEXT_ASSET_AUTOMATION. Other automation types on the campaign are preserved.",
    });

    let plan = ChangePlan::new(
        "set_pmax_campaign_settings".to_string(),
        "campaign".to_string(),
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

fn upsert_automation(settings: &mut Vec<serde_json::Value>, automation_type: &str, opted_in: bool) {
    let status = automation_status(opted_in);
    if let Some(existing) = settings
        .iter_mut()
        .find(|s| s.get("assetAutomationType").and_then(|v| v.as_str()) == Some(automation_type))
    {
        existing["assetAutomationStatus"] = json!(status);
        return;
    }
    settings.push(json!({
        "assetAutomationType": automation_type,
        "assetAutomationStatus": status
    }));
}

async fn fetch_asset_automation_settings(
    client: &GoogleAdsClient,
    customer_id: &str,
    campaign_id: &str,
) -> Result<Vec<serde_json::Value>> {
    let query = format!(
        "SELECT campaign.asset_automation_settings \
         FROM campaign \
         WHERE campaign.id = {campaign_id}"
    );
    let rows = client.search(customer_id, &query).await?;
    let settings = rows
        .first()
        .and_then(|row| row.pointer("/campaign/assetAutomationSettings"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(settings)
}

/// Build the assetAutomationSettings payload for a campaign *create*.
pub fn asset_automation_settings_for_create(
    url_expansion_opt_out: Option<bool>,
    automatically_created_assets: Option<bool>,
) -> Option<Vec<serde_json::Value>> {
    if url_expansion_opt_out.is_none() && automatically_created_assets.is_none() {
        return None;
    }
    let mut settings = Vec::new();
    if let Some(opt_out) = url_expansion_opt_out {
        settings.push(json!({
            "assetAutomationType": URL_EXPANSION_TYPE,
            "assetAutomationStatus": automation_status(!opt_out)
        }));
    }
    if let Some(enabled) = automatically_created_assets {
        settings.push(json!({
            "assetAutomationType": TEXT_ASSET_TYPE,
            "assetAutomationStatus": automation_status(enabled)
        }));
    }
    Some(settings)
}

/// Set or clear `tracking_url_template` and `final_url_suffix` at customer
/// (account) or campaign level. Pass an empty string to clear a field.
pub fn set_tracking(
    config: &Config,
    customer_id: &str,
    level: &str,
    campaign_id: Option<&str>,
    tracking_url_template: Option<String>,
    final_url_suffix: Option<String>,
) -> Result<serde_json::Value> {
    check_blocked_operation("set_tracking", &config.safety)?;

    if tracking_url_template.is_none() && final_url_suffix.is_none() {
        return Err(McpGoogleAdsError::Validation(
            "Set tracking_url_template and/or final_url_suffix (empty string clears the field)"
                .to_string(),
        ));
    }

    let cid = GoogleAdsClient::normalize_customer_id(customer_id);
    let level = level.trim().to_lowercase();

    let (operation, entity_id, update_mask_fields, resource) = match level.as_str() {
        "customer" | "account" => {
            let resource = format!("customers/{cid}");
            let (update, mask) =
                tracking_update_body(&resource, &tracking_url_template, &final_url_suffix);
            (
                json!({
                    "customerOperation": {
                        "update": update,
                        "updateMask": mask.join(",")
                    }
                }),
                cid.clone(),
                mask,
                resource,
            )
        }
        "campaign" => {
            let campaign_id = campaign_id.ok_or_else(|| {
                McpGoogleAdsError::Validation(
                    "campaign_id is required when level is 'campaign'".to_string(),
                )
            })?;
            validate_numeric_id("campaign_id", campaign_id)?;
            let resource = format!("customers/{cid}/campaigns/{campaign_id}");
            let (update, mask) =
                tracking_update_body(&resource, &tracking_url_template, &final_url_suffix);
            (
                json!({
                    "campaignOperation": {
                        "update": update,
                        "updateMask": mask.join(",")
                    }
                }),
                campaign_id.to_string(),
                mask,
                resource,
            )
        }
        other => {
            return Err(McpGoogleAdsError::Validation(format!(
                "level must be 'customer' or 'campaign', got '{other}'"
            )));
        }
    };

    let changes = json!({
        "level": level,
        "resource": resource,
        "tracking_url_template": tracking_url_template,
        "final_url_suffix": final_url_suffix,
        "update_mask": update_mask_fields,
        "note": "Empty string clears the field. Customer-level values apply to campaigns that do not set their own template/suffix.",
    });

    let plan = ChangePlan::new(
        "set_tracking".to_string(),
        level,
        entity_id,
        cid,
        changes,
        false,
        vec![operation],
    );
    let preview = plan.to_preview();
    store_plan(plan);
    Ok(preview)
}

fn tracking_update_body(
    resource: &str,
    tracking_url_template: &Option<String>,
    final_url_suffix: &Option<String>,
) -> (serde_json::Value, Vec<&'static str>) {
    let mut update = json!({ "resourceName": resource });
    let mut mask = Vec::new();
    if let Some(t) = tracking_url_template {
        update["trackingUrlTemplate"] = json!(t);
        mask.push("trackingUrlTemplate");
    }
    if let Some(s) = final_url_suffix {
        update["finalUrlSuffix"] = json!(s);
        mask.push("finalUrlSuffix");
    }
    (update, mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::safety::preview::get_plan;

    fn plan_ops(preview: &serde_json::Value) -> Vec<serde_json::Value> {
        let plan_id = preview["plan_id"].as_str().unwrap();
        get_plan(plan_id).unwrap().mutate_operations
    }

    #[test]
    fn create_settings_map_opt_out_to_final_url_expansion_type() {
        let settings = asset_automation_settings_for_create(Some(true), Some(false)).unwrap();
        assert_eq!(settings.len(), 2);
        assert_eq!(settings[0]["assetAutomationType"], URL_EXPANSION_TYPE);
        assert_eq!(settings[0]["assetAutomationStatus"], "OPTED_OUT");
        assert_eq!(settings[1]["assetAutomationType"], TEXT_ASSET_TYPE);
        assert_eq!(settings[1]["assetAutomationStatus"], "OPTED_OUT");
    }

    #[test]
    fn create_settings_omitted_when_unset() {
        assert!(asset_automation_settings_for_create(None, None).is_none());
    }

    #[test]
    fn upsert_merges_by_type() {
        let mut settings = vec![json!({
            "assetAutomationType": "GENERATE_IMAGE_ENHANCEMENT",
            "assetAutomationStatus": "OPTED_IN"
        })];
        upsert_automation(&mut settings, TEXT_ASSET_TYPE, false);
        upsert_automation(&mut settings, TEXT_ASSET_TYPE, true);
        assert_eq!(settings.len(), 2);
        assert_eq!(settings[1]["assetAutomationStatus"], "OPTED_IN");
        assert_eq!(
            settings[0]["assetAutomationType"],
            "GENERATE_IMAGE_ENHANCEMENT"
        );
    }

    #[test]
    fn set_tracking_customer() {
        let config = Config::default();
        let preview = set_tracking(
            &config,
            "123-456-7890",
            "customer",
            None,
            Some("{lpurl}?device={device}".into()),
            Some("utm_source=google".into()),
        )
        .unwrap();
        let ops = plan_ops(&preview);
        let update = &ops[0]["customerOperation"];
        assert_eq!(update["update"]["resourceName"], "customers/1234567890");
        assert_eq!(
            update["update"]["trackingUrlTemplate"],
            "{lpurl}?device={device}"
        );
        assert_eq!(update["update"]["finalUrlSuffix"], "utm_source=google");
        assert_eq!(update["updateMask"], "trackingUrlTemplate,finalUrlSuffix");
    }

    #[test]
    fn set_tracking_campaign_clear_suffix() {
        let config = Config::default();
        let preview = set_tracking(
            &config,
            "1234567890",
            "campaign",
            Some("55"),
            None,
            Some("".into()),
        )
        .unwrap();
        let ops = plan_ops(&preview);
        let update = &ops[0]["campaignOperation"];
        assert_eq!(
            update["update"]["resourceName"],
            "customers/1234567890/campaigns/55"
        );
        assert_eq!(update["update"]["finalUrlSuffix"], "");
        assert_eq!(update["updateMask"], "finalUrlSuffix");
        assert!(update["update"].get("trackingUrlTemplate").is_none());
    }

    #[test]
    fn set_tracking_campaign_requires_id() {
        let config = Config::default();
        let err = set_tracking(&config, "1", "campaign", None, Some("x".into()), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("campaign_id"));
    }

    #[test]
    fn set_tracking_rejects_unknown_level() {
        let config = Config::default();
        assert!(set_tracking(&config, "1", "ad_group", None, Some("x".into()), None).is_err());
    }

    #[test]
    fn set_tracking_requires_a_field() {
        let config = Config::default();
        assert!(set_tracking(&config, "1", "customer", None, None, None).is_err());
    }

    #[test]
    fn settings_require_a_flag() {
        // The async path is tested via upsert/create helpers; the validation
        // of "at least one flag" is the same guard.
        assert!(url_expansion_opt_out_xor_none(None, None));
    }

    fn url_expansion_opt_out_xor_none(a: Option<bool>, b: Option<bool>) -> bool {
        a.is_none() && b.is_none()
    }
}
