//! PMax brand exclusions via SharedSet type BRANDS + campaign criteria.
//!
//! Google Ads attaches a brand list with a `campaignCriterion` whose
//! `brandList.sharedSet` points at a `SharedSet` of type `BRANDS` — not
//! `campaignSharedSet` (that path is for negative keywords / placements).
//! Performance Max accepts brand criteria only as negatives.
//!
//! `BrandInfo.display_name` is output-only. A brand list member is created
//! with `BrandInfo.entity_id` (the Commercial Knowledge Graph MID). Names
//! cannot be written; look them up first with [`suggest_brands`].

use serde_json::json;

use crate::client::GoogleAdsClient;
use crate::config::Config;
use crate::error::{McpGoogleAdsError, Result};
use crate::safety::guards::check_blocked_operation;
use crate::safety::preview::{store_plan, ChangePlan};
use crate::tools::shared_sets::validate_numeric_id;

const BRAND_NAMES_UNSUPPORTED: &str = "\
BrandInfo.display_name is output-only in the Google Ads API, so a brand list \
cannot be created from brand names. Each member needs BrandInfo.entity_id — \
the Commercial Knowledge Graph MID. Call suggest_brands with a name prefix to \
look up MIDs, then pass those entity_ids to create_brand_list.";

/// Look up brands by name prefix via BrandSuggestionService.
///
/// This is a dedicated RPC (`customers/{cid}:suggestBrands`), not a mutate.
/// Returns candidate brands with `id` (MID), `name`, and `urls`.
pub async fn suggest_brands(
    client: &GoogleAdsClient,
    customer_id: &str,
    brand_prefix: &str,
) -> Result<String> {
    if brand_prefix.trim().is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "brand_prefix is required".to_string(),
        ));
    }
    let suggestions = client
        .suggest_brands(customer_id, brand_prefix.trim())
        .await?;
    let result = json!({
        "brand_prefix": brand_prefix.trim(),
        "suggestions": suggestions,
        "note": "Use each suggestion's id (Commercial Knowledge Graph MID) as entity_id on create_brand_list. Names cannot be written — BrandInfo.display_name is output-only.",
    });
    serde_json::to_string_pretty(&result).map_err(Into::into)
}

/// List BRANDS shared sets in the account.
pub async fn list_brand_lists(client: &GoogleAdsClient, customer_id: &str) -> Result<String> {
    let sets_query = "\
        SELECT \
            shared_set.resource_name, \
            shared_set.id, \
            shared_set.name, \
            shared_set.type, \
            shared_set.member_count, \
            shared_set.reference_count, \
            shared_set.status \
        FROM shared_set \
        WHERE shared_set.type = 'BRANDS' \
            AND shared_set.status != 'REMOVED'";

    let members_query = "\
        SELECT \
            shared_criterion.shared_set, \
            shared_criterion.criterion_id, \
            shared_criterion.brand.entity_id, \
            shared_criterion.brand.display_name, \
            shared_criterion.brand.status \
        FROM shared_criterion \
        WHERE shared_set.type = 'BRANDS'";

    let sets = client.search(customer_id, sets_query).await?;
    let members = client.search(customer_id, members_query).await?;

    let lists: Vec<serde_json::Value> = sets
        .iter()
        .map(|row| {
            let resource = row
                .pointer("/sharedSet/resourceName")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    let id = row
                        .pointer("/sharedSet/id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let cid = GoogleAdsClient::normalize_customer_id(customer_id);
                    format!("customers/{cid}/sharedSets/{id}")
                });
            let set_id = row
                .pointer("/sharedSet/id")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let brands: Vec<serde_json::Value> = members
                .iter()
                .filter(|m| {
                    m.pointer("/sharedCriterion/sharedSet")
                        .and_then(|v| v.as_str())
                        == Some(resource.as_str())
                        || m.pointer("/sharedCriterion/sharedSet")
                            .and_then(|v| v.as_str())
                            .is_some_and(|s| s.ends_with(&format!("/sharedSets/{set_id}")))
                })
                .map(|m| {
                    json!({
                        "criterion_id": m.pointer("/sharedCriterion/criterionId"),
                        "entity_id": m.pointer("/sharedCriterion/brand/entityId"),
                        "display_name": m.pointer("/sharedCriterion/brand/displayName"),
                        "status": m.pointer("/sharedCriterion/brand/status"),
                    })
                })
                .collect();

            json!({
                "id": set_id,
                "name": row.pointer("/sharedSet/name"),
                "member_count": row.pointer("/sharedSet/memberCount"),
                "reference_count": row.pointer("/sharedSet/referenceCount"),
                "status": row.pointer("/sharedSet/status"),
                "brands": brands,
            })
        })
        .collect();

    serde_json::to_string_pretty(&json!({
        "brand_lists": lists,
        "total_count": lists.len(),
    }))
    .map_err(Into::into)
}

/// Create a BRANDS shared set from Commercial KG MIDs, optionally attaching it
/// as a negative brand list on PMax campaigns.
pub fn create_brand_list(
    config: &Config,
    customer_id: &str,
    name: &str,
    entity_ids: Vec<String>,
    brand_names: Option<Vec<String>>,
    campaign_ids: &[String],
) -> Result<serde_json::Value> {
    check_blocked_operation("create_brand_list", &config.safety)?;

    if name.trim().is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "List name is required".to_string(),
        ));
    }
    if brand_names.as_ref().map(|n| !n.is_empty()).unwrap_or(false) && entity_ids.is_empty() {
        return Err(McpGoogleAdsError::Validation(
            BRAND_NAMES_UNSUPPORTED.to_string(),
        ));
    }
    if entity_ids.is_empty() {
        return Err(McpGoogleAdsError::Validation(format!(
            "At least one entity_id (Commercial Knowledge Graph MID) is required. {BRAND_NAMES_UNSUPPORTED}"
        )));
    }
    for campaign_id in campaign_ids {
        validate_numeric_id("campaign_id", campaign_id)?;
    }
    for eid in &entity_ids {
        if eid.trim().is_empty() {
            return Err(McpGoogleAdsError::Validation(
                "entity_id must not be empty".to_string(),
            ));
        }
    }

    let cid = GoogleAdsClient::normalize_customer_id(customer_id);
    let shared_set_resource = format!("customers/{cid}/sharedSets/-1");

    let mut operations = vec![json!({
        "sharedSetOperation": {
            "create": {
                "name": name.trim(),
                "type": "BRANDS",
                "resourceName": shared_set_resource
            }
        }
    })];

    for eid in &entity_ids {
        operations.push(json!({
            "sharedCriterionOperation": {
                "create": {
                    "sharedSet": shared_set_resource,
                    "brand": {
                        "entityId": eid.trim()
                    }
                }
            }
        }));
    }

    for campaign_id in campaign_ids {
        operations.push(negative_brand_list_criterion(
            &cid,
            campaign_id,
            &shared_set_resource,
        ));
    }

    let changes = json!({
        "name": name.trim(),
        "type": "BRANDS",
        "entity_ids": entity_ids,
        "attach_to_campaign_ids": campaign_ids,
        "note": "Attached campaigns receive a negative BRAND_LIST campaign criterion (the only mode Performance Max supports). BrandInfo.display_name is output-only — members are identified by entity_id.",
    });

    let plan = ChangePlan::new(
        "create_brand_list".to_string(),
        "shared_set".to_string(),
        "new".to_string(),
        cid,
        changes,
        false,
        operations,
    );
    let preview = plan.to_preview();
    store_plan(plan);
    Ok(preview)
}

fn negative_brand_list_criterion(
    cid: &str,
    campaign_id: &str,
    shared_set: &str,
) -> serde_json::Value {
    json!({
        "campaignCriterionOperation": {
            "create": {
                "campaign": format!("customers/{cid}/campaigns/{campaign_id}"),
                "negative": true,
                "brandList": {
                    "sharedSet": shared_set
                }
            }
        }
    })
}

/// Attach an existing BRANDS shared set as a negative brand list on PMax campaigns.
pub fn attach_brand_list(
    config: &Config,
    customer_id: &str,
    shared_set_id: &str,
    campaign_ids: &[String],
) -> Result<serde_json::Value> {
    check_blocked_operation("attach_brand_list", &config.safety)?;
    validate_numeric_id("shared_set_id", shared_set_id)?;
    if campaign_ids.is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "At least one campaign ID is required".to_string(),
        ));
    }
    for campaign_id in campaign_ids {
        validate_numeric_id("campaign_id", campaign_id)?;
    }

    let cid = GoogleAdsClient::normalize_customer_id(customer_id);
    let shared_set_resource = format!("customers/{cid}/sharedSets/{shared_set_id}");
    let operations: Vec<serde_json::Value> = campaign_ids
        .iter()
        .map(|campaign_id| negative_brand_list_criterion(&cid, campaign_id, &shared_set_resource))
        .collect();

    let changes = json!({
        "shared_set_id": shared_set_id,
        "campaign_ids": campaign_ids,
        "negative": true,
        "note": "Performance Max only accepts brand list criteria as negatives (exclusions).",
    });

    let plan = ChangePlan::new(
        "attach_brand_list".to_string(),
        "campaign_criterion".to_string(),
        shared_set_id.to_string(),
        cid,
        changes,
        false,
        operations,
    );
    let preview = plan.to_preview();
    store_plan(plan);
    Ok(preview)
}

/// Detach a brand list from campaigns by removing the BRAND_LIST campaign criteria.
///
/// Requires a prior GAQL lookup of the criterion resource names because the
/// campaign-criterion ID is not `{campaign}~{shared_set}`.
pub async fn detach_brand_list(
    client: &GoogleAdsClient,
    config: &Config,
    customer_id: &str,
    shared_set_id: &str,
    campaign_ids: &[String],
) -> Result<serde_json::Value> {
    check_blocked_operation("detach_brand_list", &config.safety)?;
    validate_numeric_id("shared_set_id", shared_set_id)?;
    if campaign_ids.is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "At least one campaign ID is required".to_string(),
        ));
    }
    for campaign_id in campaign_ids {
        validate_numeric_id("campaign_id", campaign_id)?;
    }

    let cid = GoogleAdsClient::normalize_customer_id(customer_id);
    let shared_set_resource = format!("customers/{cid}/sharedSets/{shared_set_id}");
    let campaign_filter = campaign_ids.join(",");
    let query = format!(
        "SELECT \
            campaign_criterion.resource_name, \
            campaign.id, \
            campaign_criterion.brand_list.shared_set \
         FROM campaign_criterion \
         WHERE campaign.id IN ({campaign_filter}) \
           AND campaign_criterion.type = 'BRAND_LIST' \
           AND campaign_criterion.status != 'REMOVED'"
    );
    let rows = client.search(&cid, &query).await?;

    let mut operations = Vec::new();
    let mut matched = Vec::new();
    for row in &rows {
        let set = row
            .pointer("/campaignCriterion/brandList/sharedSet")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if set != shared_set_resource {
            continue;
        }
        let resource = row
            .pointer("/campaignCriterion/resourceName")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                McpGoogleAdsError::Validation(
                    "Brand list criterion row missing resource_name".to_string(),
                )
            })?;
        operations.push(json!({
            "campaignCriterionOperation": {
                "remove": resource
            }
        }));
        matched.push(json!({
            "campaign_id": row.pointer("/campaign/id"),
            "criterion": resource,
        }));
    }

    if operations.is_empty() {
        return Err(McpGoogleAdsError::Validation(format!(
            "No BRAND_LIST campaign criterion found linking shared set {shared_set_id} to the \
             given campaigns. The list may already be detached, or it was never attached via \
             campaign_criterion (PMax brand exclusions are not CampaignSharedSet links)."
        )));
    }

    let changes = json!({
        "shared_set_id": shared_set_id,
        "campaign_ids": campaign_ids,
        "removed_criteria": matched,
        "warning": "These campaigns stop excluding the brands on this list. The list itself is kept.",
    });

    let plan = ChangePlan::new(
        "detach_brand_list".to_string(),
        "campaign_criterion".to_string(),
        shared_set_id.to_string(),
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
    use crate::safety::preview::get_plan;

    fn plan_ops(preview: &serde_json::Value) -> Vec<serde_json::Value> {
        let plan_id = preview["plan_id"].as_str().unwrap();
        get_plan(plan_id).unwrap().mutate_operations
    }

    #[test]
    fn create_from_entity_ids() {
        let config = Config::default();
        let preview = create_brand_list(
            &config,
            "123-456-7890",
            "Competitor brands",
            vec!["brand/nike".into(), "brand/adidas".into()],
            None,
            &["999".into()],
        )
        .unwrap();
        assert_eq!(preview["operation"], "create_brand_list");
        let ops = plan_ops(&preview);
        assert_eq!(ops[0]["sharedSetOperation"]["create"]["type"], "BRANDS");
        assert_eq!(
            ops[1]["sharedCriterionOperation"]["create"]["brand"]["entityId"],
            "brand/nike"
        );
        assert_eq!(
            ops[3]["campaignCriterionOperation"]["create"]["negative"],
            true
        );
        assert_eq!(
            ops[3]["campaignCriterionOperation"]["create"]["brandList"]["sharedSet"],
            "customers/1234567890/sharedSets/-1"
        );
    }

    #[test]
    fn names_without_entity_ids_are_rejected_not_faked() {
        let config = Config::default();
        let err = create_brand_list(&config, "1", "List", vec![], Some(vec!["Nike".into()]), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("output-only"));
        assert!(err.contains("suggest_brands"));
    }

    #[test]
    fn empty_entity_ids_rejected() {
        let config = Config::default();
        assert!(create_brand_list(&config, "1", "List", vec![], None, &[]).is_err());
    }

    #[test]
    fn attach_is_negative_brand_list_criterion() {
        let config = Config::default();
        let preview = attach_brand_list(&config, "1234567890", "77", &["11".into()]).unwrap();
        let ops = plan_ops(&preview);
        let create = &ops[0]["campaignCriterionOperation"]["create"];
        assert_eq!(create["negative"], true);
        assert_eq!(
            create["brandList"]["sharedSet"],
            "customers/1234567890/sharedSets/77"
        );
        assert!(create.get("sharedSet").is_none());
    }

    #[test]
    fn attach_requires_campaigns() {
        let config = Config::default();
        assert!(attach_brand_list(&config, "1", "2", &[]).is_err());
    }

    #[test]
    fn blocked() {
        let mut config = Config::default();
        config.safety.blocked_operations = vec!["create_brand_list".into()];
        assert!(create_brand_list(&config, "1", "List", vec!["mid".into()], None, &[]).is_err());
    }
}
