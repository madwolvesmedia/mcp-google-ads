//! Additional Performance Max asset groups on an existing campaign.
//!
//! Feed-only (retail) asset groups are allowed: text and image assets are
//! optional. Retail PMax still requires a listing group tree, so by default a
//! single UNIT_INCLUDED "all products" node is created unless the caller
//! supplies a [`ListingGroupSpec`].

use serde_json::json;

use crate::config::Config;
use crate::error::{McpGoogleAdsError, Result};
use crate::models::{AdStatus, NextActionHint};
use crate::safety::guards::{check_blocked_operation, validate_final_url};
use crate::safety::preview::{store_plan, ChangePlan};
use crate::tools::listing_groups::{build_listing_group_create_operations, ListingGroupSpec};
use crate::tools::pmax::{push_pmax_assets, validate_optional_text_assets, ImageAssetLink};
use crate::tools::shared_sets::validate_numeric_id;

/// Parameters for creating an additional PMax asset group.
pub struct CreatePmaxAssetGroupParams<'a> {
    pub config: &'a Config,
    pub customer_id: &'a str,
    pub campaign_id: &'a str,
    pub name: &'a str,
    pub final_urls: Vec<String>,
    pub headlines: Vec<String>,
    pub long_headlines: Vec<String>,
    pub descriptions: Vec<String>,
    pub business_name: Option<&'a str>,
    pub image_assets: Vec<ImageAssetLink>,
    pub status: Option<AdStatus>,
    /// When `listing_group` is omitted, create an all-products UNIT_INCLUDED
    /// root so a retail PMax asset group has a valid partition tree. Pass
    /// `false` only for non-retail asset groups (no Merchant Center).
    pub include_all_products: bool,
    pub listing_group: Option<ListingGroupSpec>,
}

pub fn create_pmax_asset_group(params: &CreatePmaxAssetGroupParams) -> Result<serde_json::Value> {
    check_blocked_operation("create_pmax_asset_group", &params.config.safety)?;
    validate_numeric_id("campaign_id", params.campaign_id)?;

    if params.name.trim().is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "Asset group name is required".to_string(),
        ));
    }
    if params.final_urls.is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "At least one final URL is required".to_string(),
        ));
    }
    for url in &params.final_urls {
        validate_final_url(url)?;
    }
    validate_optional_text_assets(
        &params.headlines,
        &params.long_headlines,
        &params.descriptions,
        params.business_name,
    )?;

    let cid = crate::client::GoogleAdsClient::normalize_customer_id(params.customer_id);
    let resolved_status = params.status.unwrap_or_default();
    let asset_group_resource = format!("customers/{cid}/assetGroups/-1");

    let mut operations = vec![json!({
        "assetGroupOperation": {
            "create": {
                "resourceName": asset_group_resource,
                "name": params.name.trim(),
                "campaign": format!("customers/{}/campaigns/{}", cid, params.campaign_id),
                "finalUrls": params.final_urls,
                "status": resolved_status.as_api_str()
            }
        }
    })];

    let mut temp_asset_id: i64 = -100;
    push_pmax_assets(
        &mut operations,
        &cid,
        &asset_group_resource,
        &mut temp_asset_id,
        &params.headlines,
        &params.long_headlines,
        &params.descriptions,
        params.business_name,
        &params.image_assets,
    )?;

    if let Some(ref spec) = params.listing_group {
        let (lg_ops, _) =
            build_listing_group_create_operations(&cid, "-1", &asset_group_resource, spec, -2)?;
        operations.extend(lg_ops);
    } else if params.include_all_products {
        let (lg_ops, _) = build_listing_group_create_operations(
            &cid,
            "-1",
            &asset_group_resource,
            &ListingGroupSpec::default(),
            -2,
        )?;
        operations.extend(lg_ops);
    }

    let changes = json!({
        "campaign_id": params.campaign_id,
        "name": params.name.trim(),
        "final_urls": params.final_urls,
        "status": resolved_status.as_api_str(),
        "headlines_count": params.headlines.len(),
        "descriptions_count": params.descriptions.len(),
        "include_all_products": params.listing_group.is_none() && params.include_all_products,
        "listing_group": params.listing_group,
        "feed_only": params.headlines.is_empty()
            && params.long_headlines.is_empty()
            && params.descriptions.is_empty()
            && params.business_name.map(str::trim).unwrap_or("").is_empty()
            && params.image_assets.is_empty(),
    });

    let mut plan = ChangePlan::new(
        "create_pmax_asset_group".to_string(),
        "asset_group".to_string(),
        "new".to_string(),
        cid,
        changes,
        false,
        operations,
    )
    .with_status_after_apply(resolved_status);

    if resolved_status == AdStatus::Paused {
        plan = plan.with_next_action_hint(NextActionHint::enable_pmax_asset_group());
    }

    let preview = plan.to_preview();
    store_plan(plan);
    Ok(preview)
}

/// Rename, pause/enable, and/or set final URLs on an existing PMax asset group.
pub fn update_pmax_asset_group(
    config: &Config,
    customer_id: &str,
    asset_group_id: &str,
    name: Option<&str>,
    status: Option<AdStatus>,
    final_urls: Option<Vec<String>>,
) -> Result<serde_json::Value> {
    check_blocked_operation("update_pmax_asset_group", &config.safety)?;
    validate_numeric_id("asset_group_id", asset_group_id)?;

    if name.is_none() && status.is_none() && final_urls.is_none() {
        return Err(McpGoogleAdsError::Validation(
            "At least one of name, status, or final_urls must be provided".to_string(),
        ));
    }

    if let Some(n) = name {
        if n.trim().is_empty() {
            return Err(McpGoogleAdsError::Validation(
                "Asset group name must not be empty".to_string(),
            ));
        }
    }
    if let Some(AdStatus::Removed) = status {
        return Err(McpGoogleAdsError::Validation(
            "status=REMOVED is not supported on update_pmax_asset_group; omit the asset group \
             from serving with status=PAUSED"
                .to_string(),
        ));
    }
    if let Some(ref urls) = final_urls {
        if urls.is_empty() {
            return Err(McpGoogleAdsError::Validation(
                "final_urls must contain at least one URL".to_string(),
            ));
        }
        for url in urls {
            validate_final_url(url)?;
        }
    }

    let cid = crate::client::GoogleAdsClient::normalize_customer_id(customer_id);
    let resource = format!("customers/{cid}/assetGroups/{asset_group_id}");

    let mut update = json!({ "resourceName": resource });
    let mut update_mask_fields: Vec<&str> = Vec::new();
    let mut changes = serde_json::Map::new();

    if let Some(n) = name {
        update["name"] = json!(n.trim());
        update_mask_fields.push("name");
        changes.insert("name".to_string(), json!(n.trim()));
    }
    if let Some(st) = status {
        update["status"] = json!(st.as_api_str());
        update_mask_fields.push("status");
        changes.insert("status".to_string(), json!(st.as_api_str()));
    }
    if let Some(urls) = final_urls {
        update["finalUrls"] = json!(urls);
        update_mask_fields.push("finalUrls");
        changes.insert("final_urls".to_string(), json!(urls));
    }

    let operations = vec![json!({
        "assetGroupOperation": {
            "update": update,
            "updateMask": update_mask_fields.join(",")
        }
    })];

    let mut plan = ChangePlan::new(
        "update_pmax_asset_group".to_string(),
        "asset_group".to_string(),
        asset_group_id.to_string(),
        cid,
        serde_json::Value::Object(changes),
        false,
        operations,
    );
    if let Some(st) = status {
        plan = plan.with_status_after_apply(st);
    }

    let preview = plan.to_preview();
    store_plan(plan);
    Ok(preview)
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
    fn feed_only_asset_group_no_text_assets() {
        let config = Config::default();
        let preview = create_pmax_asset_group(&CreatePmaxAssetGroupParams {
            config: &config,
            customer_id: "123-456-7890",
            campaign_id: "999",
            name: "Nike",
            final_urls: vec!["https://shop.example.gr/nike".into()],
            headlines: vec![],
            long_headlines: vec![],
            descriptions: vec![],
            business_name: None,
            image_assets: vec![],
            status: None,
            include_all_products: true,
            listing_group: None,
        })
        .unwrap();
        assert_eq!(preview["operation"], "create_pmax_asset_group");
        assert_eq!(preview["status_after_apply"], "PAUSED");
        assert_eq!(preview["changes"]["feed_only"], true);

        let ops = plan_ops(&preview);
        assert!(ops.iter().any(|op| op.get("assetGroupOperation").is_some()));
        assert!(ops.iter().any(|op| op
            .pointer("/assetGroupListingGroupFilterOperation/create/type")
            == Some(&json!("UNIT_INCLUDED"))));
        assert!(ops.iter().all(|op| op.get("assetOperation").is_none()
            && op.get("assetGroupAssetOperation").is_none()
            || op.get("assetGroupOperation").is_some()
            || op.get("assetGroupListingGroupFilterOperation").is_some()));
        // No text asset operations.
        assert!(ops.iter().all(|op| op.get("assetOperation").is_none()));
    }

    #[test]
    fn listing_group_include_only_replaces_all_products() {
        let config = Config::default();
        let spec = ListingGroupSpec {
            include_only: Some(vec![crate::tools::listing_groups::ListingGroupValue {
                dimension: "BRAND".into(),
                value: Some("Nike".into()),
                category_id: None,
            }]),
            exclude: None,
            partitions: None,
        };
        let preview = create_pmax_asset_group(&CreatePmaxAssetGroupParams {
            config: &config,
            customer_id: "1234567890",
            campaign_id: "999",
            name: "Nike",
            final_urls: vec!["https://example.com".into()],
            headlines: vec![],
            long_headlines: vec![],
            descriptions: vec![],
            business_name: None,
            image_assets: vec![],
            status: Some(AdStatus::Enabled),
            include_all_products: true, // ignored when listing_group is set
            listing_group: Some(spec),
        })
        .unwrap();
        let ops = plan_ops(&preview);
        let lg: Vec<_> = ops
            .iter()
            .filter_map(|op| op.pointer("/assetGroupListingGroupFilterOperation/create"))
            .collect();
        assert!(lg.len() >= 3);
        assert_eq!(lg[0]["type"], "SUBDIVISION");
        assert_eq!(preview["status_after_apply"], "ENABLED");
    }

    #[test]
    fn too_few_headlines_when_provided_is_rejected() {
        let config = Config::default();
        let err = create_pmax_asset_group(&CreatePmaxAssetGroupParams {
            config: &config,
            customer_id: "1",
            campaign_id: "2",
            name: "AG",
            final_urls: vec!["https://example.com".into()],
            headlines: vec!["H1".into()],
            long_headlines: vec![],
            descriptions: vec![],
            business_name: None,
            image_assets: vec![],
            status: None,
            include_all_products: false,
            listing_group: None,
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("3-15 headlines"));
    }

    #[test]
    fn missing_final_url_rejected() {
        let config = Config::default();
        assert!(create_pmax_asset_group(&CreatePmaxAssetGroupParams {
            config: &config,
            customer_id: "1",
            campaign_id: "2",
            name: "AG",
            final_urls: vec![],
            headlines: vec![],
            long_headlines: vec![],
            descriptions: vec![],
            business_name: None,
            image_assets: vec![],
            status: None,
            include_all_products: false,
            listing_group: None,
        })
        .is_err());
    }

    #[test]
    fn update_name_and_status_and_url() {
        let config = Config::default();
        let preview = update_pmax_asset_group(
            &config,
            "123-456-7890",
            "555",
            Some("Renamed"),
            Some(AdStatus::Paused),
            Some(vec!["https://example.com/new".into()]),
        )
        .unwrap();
        assert_eq!(preview["operation"], "update_pmax_asset_group");
        let ops = plan_ops(&preview);
        let update = &ops[0]["assetGroupOperation"];
        assert_eq!(update["update"]["name"], "Renamed");
        assert_eq!(update["update"]["status"], "PAUSED");
        assert_eq!(update["updateMask"], "name,status,finalUrls");
    }

    #[test]
    fn update_requires_a_field() {
        let config = Config::default();
        assert!(update_pmax_asset_group(&config, "1", "2", None, None, None).is_err());
    }

    #[test]
    fn update_rejects_removed() {
        let config = Config::default();
        let err = update_pmax_asset_group(&config, "1", "2", None, Some(AdStatus::Removed), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("REMOVED"));
    }

    #[test]
    fn blocked_create() {
        let mut config = Config::default();
        config.safety.blocked_operations = vec!["create_pmax_asset_group".into()];
        assert!(create_pmax_asset_group(&CreatePmaxAssetGroupParams {
            config: &config,
            customer_id: "1",
            campaign_id: "2",
            name: "AG",
            final_urls: vec!["https://example.com".into()],
            headlines: vec![],
            long_headlines: vec![],
            descriptions: vec![],
            business_name: None,
            image_assets: vec![],
            status: None,
            include_all_products: false,
            listing_group: None,
        })
        .is_err());
    }
}
