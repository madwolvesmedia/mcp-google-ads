//! Performance Max listing group (product partition) trees.
//!
//! Retail PMax asset groups require a valid product partition tree of
//! [`AssetGroupListingGroupFilter`](https://developers.google.com/google-ads/api/docs/performance-max/listing-groups)
//! resources. Each subdivision must be fully partitioned, which means every
//! sibling group includes an "everything else" node of the same dimension with
//! an empty case value. Mutations of a tree are applied atomically: existing
//! nodes are removed (children before parents) and the new tree is created in
//! the same `googleAds:mutate` request, using temporary IDs for parent/child
//! links.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::client::GoogleAdsClient;
use crate::config::Config;
use crate::error::{McpGoogleAdsError, Result};
use crate::safety::guards::check_blocked_operation;
use crate::safety::preview::{store_plan, ChangePlan};
use crate::tools::shared_sets::validate_numeric_id;

/// Dimensions supported on Performance Max listing group filters.
///
/// Names match the Google Ads `ListingGroupFilterDimension` oneof, with a
/// level/index suffix where the API requires one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListingDimension {
    Brand,
    ItemId,
    ProductType { level: u8 },
    ProductCategory { level: u8 },
    CustomLabel { index: u8 },
}

impl ListingDimension {
    pub fn parse(name: &str) -> Result<Self> {
        let upper = name.trim().to_uppercase().replace('-', "_");
        match upper.as_str() {
            "BRAND" | "PRODUCT_BRAND" => Ok(Self::Brand),
            "ITEM_ID" | "PRODUCT_ITEM_ID" => Ok(Self::ItemId),
            "PRODUCT_TYPE" | "PRODUCT_TYPE_L1" | "PRODUCT_TYPE_LEVEL1" => {
                Ok(Self::ProductType { level: 1 })
            }
            "PRODUCT_TYPE_L2" | "PRODUCT_TYPE_LEVEL2" => Ok(Self::ProductType { level: 2 }),
            "PRODUCT_TYPE_L3" | "PRODUCT_TYPE_LEVEL3" => Ok(Self::ProductType { level: 3 }),
            "PRODUCT_TYPE_L4" | "PRODUCT_TYPE_LEVEL4" => Ok(Self::ProductType { level: 4 }),
            "PRODUCT_TYPE_L5" | "PRODUCT_TYPE_LEVEL5" => Ok(Self::ProductType { level: 5 }),
            "PRODUCT_CATEGORY"
            | "GOOGLE_PRODUCT_CATEGORY"
            | "PRODUCT_CATEGORY_L1"
            | "GOOGLE_PRODUCT_CATEGORY_L1" => Ok(Self::ProductCategory { level: 1 }),
            "PRODUCT_CATEGORY_L2" | "GOOGLE_PRODUCT_CATEGORY_L2" => {
                Ok(Self::ProductCategory { level: 2 })
            }
            "PRODUCT_CATEGORY_L3" | "GOOGLE_PRODUCT_CATEGORY_L3" => {
                Ok(Self::ProductCategory { level: 3 })
            }
            "PRODUCT_CATEGORY_L4" | "GOOGLE_PRODUCT_CATEGORY_L4" => {
                Ok(Self::ProductCategory { level: 4 })
            }
            "PRODUCT_CATEGORY_L5" | "GOOGLE_PRODUCT_CATEGORY_L5" => {
                Ok(Self::ProductCategory { level: 5 })
            }
            "CUSTOM_LABEL_0" | "CUSTOM_LABEL0" | "PRODUCT_CUSTOM_ATTRIBUTE_0" => {
                Ok(Self::CustomLabel { index: 0 })
            }
            "CUSTOM_LABEL_1" | "CUSTOM_LABEL1" | "PRODUCT_CUSTOM_ATTRIBUTE_1" => {
                Ok(Self::CustomLabel { index: 1 })
            }
            "CUSTOM_LABEL_2" | "CUSTOM_LABEL2" | "PRODUCT_CUSTOM_ATTRIBUTE_2" => {
                Ok(Self::CustomLabel { index: 2 })
            }
            "CUSTOM_LABEL_3" | "CUSTOM_LABEL3" | "PRODUCT_CUSTOM_ATTRIBUTE_3" => {
                Ok(Self::CustomLabel { index: 3 })
            }
            "CUSTOM_LABEL_4" | "CUSTOM_LABEL4" | "PRODUCT_CUSTOM_ATTRIBUTE_4" => {
                Ok(Self::CustomLabel { index: 4 })
            }
            other => Err(McpGoogleAdsError::Validation(format!(
                "Unknown listing group dimension '{other}'. Must be one of: BRAND, ITEM_ID, \
                 PRODUCT_TYPE_L1..L5, PRODUCT_CATEGORY_L1..L5 (Google product category), \
                 CUSTOM_LABEL_0..4"
            ))),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Brand => "BRAND".to_string(),
            Self::ItemId => "ITEM_ID".to_string(),
            Self::ProductType { level } => format!("PRODUCT_TYPE_L{level}"),
            Self::ProductCategory { level } => format!("PRODUCT_CATEGORY_L{level}"),
            Self::CustomLabel { index } => format!("CUSTOM_LABEL_{index}"),
        }
    }

    fn rest_key_and_payload(&self, value: Option<&str>, category_id: Option<i64>) -> Value {
        match self {
            Self::Brand => {
                let mut obj = serde_json::Map::new();
                if let Some(v) = value.filter(|s| !s.is_empty()) {
                    obj.insert("value".to_string(), json!(v));
                }
                json!({ "productBrand": obj })
            }
            Self::ItemId => {
                let mut obj = serde_json::Map::new();
                if let Some(v) = value.filter(|s| !s.is_empty()) {
                    obj.insert("value".to_string(), json!(v));
                }
                json!({ "productItemId": obj })
            }
            Self::ProductType { level } => {
                let mut obj = serde_json::Map::new();
                obj.insert("level".to_string(), json!(format!("LEVEL{level}")));
                if let Some(v) = value.filter(|s| !s.is_empty()) {
                    obj.insert("value".to_string(), json!(v));
                }
                json!({ "productType": obj })
            }
            Self::ProductCategory { level } => {
                let mut obj = serde_json::Map::new();
                obj.insert("level".to_string(), json!(format!("LEVEL{level}")));
                if let Some(id) = category_id {
                    obj.insert("categoryId".to_string(), json!(id.to_string()));
                }
                json!({ "productCategory": obj })
            }
            Self::CustomLabel { index } => {
                let mut obj = serde_json::Map::new();
                obj.insert("index".to_string(), json!(format!("INDEX{index}")));
                if let Some(v) = value.filter(|s| !s.is_empty()) {
                    obj.insert("value".to_string(), json!(v));
                }
                json!({ "productCustomAttribute": obj })
            }
        }
    }

    fn requires_value_or_category(
        &self,
        value: Option<&str>,
        category_id: Option<i64>,
    ) -> Result<()> {
        match self {
            Self::ProductCategory { .. } => {
                if category_id.is_none() {
                    return Err(McpGoogleAdsError::Validation(
                        "PRODUCT_CATEGORY nodes need category_id (the Google product category \
                         integer). Omit value/category_id only on the 'everything else' node."
                            .to_string(),
                    ));
                }
                Ok(())
            }
            _ => {
                if value.map(str::trim).unwrap_or("").is_empty() {
                    return Err(McpGoogleAdsError::Validation(format!(
                        "{} nodes need a non-empty value. Omit value only on the 'everything else' node.",
                        self.label()
                    )));
                }
                Ok(())
            }
        }
    }
}

/// A single include/exclude value at one dimension.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListingGroupValue {
    /// Dimension: BRAND, ITEM_ID, PRODUCT_TYPE_L1..L5, PRODUCT_CATEGORY_L1..L5,
    /// CUSTOM_LABEL_0..4.
    pub dimension: String,
    /// Partition value (brand name, product type, item id, custom label).
    /// Omit (or leave empty) only for an "everything else" node.
    pub value: Option<String>,
    /// Google product category ID. Required for PRODUCT_CATEGORY nodes that
    /// are not the "everything else" sibling.
    pub category_id: Option<i64>,
}

/// A node in a listing group tree. Nodes with `children` become SUBDIVISION;
/// leaves become UNIT_INCLUDED or UNIT_EXCLUDED.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListingGroupPartition {
    /// Dimension: BRAND, ITEM_ID, PRODUCT_TYPE_L1..L5, PRODUCT_CATEGORY_L1..L5,
    /// CUSTOM_LABEL_0..4. Required on every child of a subdivision; omitted on
    /// the synthetic root.
    pub dimension: Option<String>,
    /// Partition value. Omit (or empty) for the "everything else" sibling.
    pub value: Option<String>,
    /// Google product category ID (PRODUCT_CATEGORY nodes).
    pub category_id: Option<i64>,
    /// `true` (default) = UNIT_INCLUDED, `false` = UNIT_EXCLUDED. Ignored when
    /// `children` is non-empty (the node is a SUBDIVISION).
    pub include: Option<bool>,
    /// Child partitions. When present and non-empty this node is a SUBDIVISION
    /// and an "everything else" sibling is added among the children if missing.
    pub children: Option<Vec<ListingGroupPartition>>,
}

/// High-level listing group spec accepted by write tools.
///
/// Provide exactly one of:
/// - nothing / empty — a single UNIT_INCLUDED root ("all products")
/// - `include_only` — those values UNIT_INCLUDED, everything else UNIT_EXCLUDED
/// - `exclude` — those values UNIT_EXCLUDED, everything else UNIT_INCLUDED
/// - `partitions` — full sibling list under a synthetic root (nested `children`
///   allowed). An "everything else" node is added at each subdivision if missing.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListingGroupSpec {
    pub include_only: Option<Vec<ListingGroupValue>>,
    pub exclude: Option<Vec<ListingGroupValue>>,
    pub partitions: Option<Vec<ListingGroupPartition>>,
}

impl ListingGroupSpec {
    pub fn is_empty(&self) -> bool {
        self.include_only
            .as_ref()
            .map(|v| v.is_empty())
            .unwrap_or(true)
            && self.exclude.as_ref().map(|v| v.is_empty()).unwrap_or(true)
            && self
                .partitions
                .as_ref()
                .map(|v| v.is_empty())
                .unwrap_or(true)
    }

    /// Resolve convenience fields into a sibling list under a synthetic root.
    /// An empty spec means "all products" (no siblings — caller should emit a
    /// UNIT_INCLUDED root).
    pub fn resolve_root_children(&self) -> Result<Vec<ListingGroupPartition>> {
        let has_include = self
            .include_only
            .as_ref()
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        let has_exclude = self
            .exclude
            .as_ref()
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        let has_partitions = self
            .partitions
            .as_ref()
            .map(|v| !v.is_empty())
            .unwrap_or(false);

        let modes = [has_include, has_exclude, has_partitions]
            .into_iter()
            .filter(|b| *b)
            .count();
        if modes > 1 {
            return Err(McpGoogleAdsError::Validation(
                "Provide only one of include_only, exclude, or partitions".to_string(),
            ));
        }

        if has_partitions {
            return Ok(self.partitions.clone().unwrap_or_default());
        }

        if has_include {
            let values = self.include_only.as_ref().cloned().unwrap_or_default();
            validate_same_dimension(&values)?;
            let mut parts: Vec<ListingGroupPartition> =
                values.iter().map(|v| value_to_partition(v, true)).collect();
            parts.push(other_partition_from_values(&values, false)?);
            return Ok(parts);
        }

        if has_exclude {
            let values = self.exclude.as_ref().cloned().unwrap_or_default();
            validate_same_dimension(&values)?;
            let mut parts: Vec<ListingGroupPartition> = values
                .iter()
                .map(|v| value_to_partition(v, false))
                .collect();
            parts.push(other_partition_from_values(&values, true)?);
            return Ok(parts);
        }

        Ok(Vec::new())
    }
}

fn value_to_partition(value: &ListingGroupValue, include: bool) -> ListingGroupPartition {
    ListingGroupPartition {
        dimension: Some(value.dimension.clone()),
        value: value.value.clone(),
        category_id: value.category_id,
        include: Some(include),
        children: None,
    }
}

fn other_partition_from_values(
    values: &[ListingGroupValue],
    include: bool,
) -> Result<ListingGroupPartition> {
    let first = values.first().ok_or_else(|| {
        McpGoogleAdsError::Validation("At least one listing group value is required".to_string())
    })?;
    Ok(ListingGroupPartition {
        dimension: Some(first.dimension.clone()),
        value: None,
        category_id: None,
        include: Some(include),
        children: None,
    })
}

fn validate_same_dimension(values: &[ListingGroupValue]) -> Result<()> {
    if values.is_empty() {
        return Err(McpGoogleAdsError::Validation(
            "At least one listing group value is required".to_string(),
        ));
    }
    let first = ListingDimension::parse(&values[0].dimension)?;
    for v in values {
        let dim = ListingDimension::parse(&v.dimension)?;
        if dim != first {
            return Err(McpGoogleAdsError::Validation(format!(
                "All siblings in a listing group must use the same dimension; got {} and {}",
                first.label(),
                dim.label()
            )));
        }
        dim.requires_value_or_category(v.value.as_deref(), v.category_id)?;
    }
    Ok(())
}

fn is_other_node(p: &ListingGroupPartition) -> bool {
    let empty_value = p.value.as_deref().map(str::trim).unwrap_or("").is_empty();
    empty_value && p.category_id.is_none()
}

/// Ensure each sibling group has an "everything else" node of the same dimension.
fn ensure_other_siblings(siblings: &mut Vec<ListingGroupPartition>) -> Result<()> {
    if siblings.is_empty() {
        return Ok(());
    }

    let mut first_dim: Option<ListingDimension> = None;
    for sib in siblings.iter() {
        let dim_name = sib.dimension.as_deref().ok_or_else(|| {
            McpGoogleAdsError::Validation(
                "Each listing group partition under a subdivision needs a dimension".to_string(),
            )
        })?;
        let dim = ListingDimension::parse(dim_name)?;
        if let Some(ref first) = first_dim {
            if *first != dim {
                return Err(McpGoogleAdsError::Validation(format!(
                    "All siblings in a listing group must use the same dimension; got {} and {}",
                    first.label(),
                    dim.label()
                )));
            }
        } else {
            first_dim = Some(dim);
        }
    }

    let has_other = siblings.iter().any(is_other_node);
    if !has_other {
        let dim_name = siblings[0].dimension.clone();
        // If every specified sibling is an include, "everything else" is excluded
        // (include-only). If every specified sibling is an exclude, other is included.
        let specified_includes: Vec<bool> =
            siblings.iter().map(|s| s.include.unwrap_or(true)).collect();
        // Include-only siblings → exclude everything else. Any exclude (or a
        // mix) → include everything else so some products still serve.
        let other_include = !specified_includes.iter().all(|i| *i);
        siblings.push(ListingGroupPartition {
            dimension: dim_name,
            value: None,
            category_id: None,
            include: Some(other_include),
            children: None,
        });
    }

    for sib in siblings.iter_mut() {
        if let Some(ref mut children) = sib.children {
            if !children.is_empty() {
                ensure_other_siblings(children)?;
            }
        }
    }
    Ok(())
}

fn has_included_unit(nodes: &[ListingGroupPartition]) -> bool {
    nodes.iter().any(|n| {
        let children = n.children.as_deref().unwrap_or(&[]);
        if !children.is_empty() {
            has_included_unit(children)
        } else {
            n.include.unwrap_or(true)
        }
    })
}

/// Build create operations for a listing group tree attached to `asset_group_resource`.
///
/// `asset_group_id` is the trailing segment of the asset group resource name
/// (a real ID, or a temporary ID such as `-3`). Temporary listing-group IDs
/// start at `start_temp_id` and count downward.
///
/// Returns the operations and the next unused temporary ID.
pub fn build_listing_group_create_operations(
    cid: &str,
    asset_group_id: &str,
    asset_group_resource: &str,
    spec: &ListingGroupSpec,
    start_temp_id: i64,
) -> Result<(Vec<Value>, i64)> {
    let mut children = spec.resolve_root_children()?;
    let mut next_id = start_temp_id;
    let mut operations = Vec::new();

    if children.is_empty() {
        // All-products root: a single UNIT_INCLUDED node with no case_value.
        operations.push(listing_group_op(
            cid,
            asset_group_id,
            asset_group_resource,
            next_id,
            None,
            "UNIT_INCLUDED",
            None,
        ));
        next_id -= 1;
        return Ok((operations, next_id));
    }

    ensure_other_siblings(&mut children)?;
    if !has_included_unit(&children) {
        return Err(McpGoogleAdsError::Validation(
            "A listing group tree must contain at least one included (UNIT_INCLUDED) leaf so \
             some products can serve"
                .to_string(),
        ));
    }

    let root_id = next_id;
    next_id -= 1;
    operations.push(listing_group_op(
        cid,
        asset_group_id,
        asset_group_resource,
        root_id,
        None,
        "SUBDIVISION",
        None,
    ));

    emit_children(
        cid,
        asset_group_id,
        asset_group_resource,
        root_id,
        &children,
        &mut operations,
        &mut next_id,
    )?;

    Ok((operations, next_id))
}

fn emit_children(
    cid: &str,
    asset_group_id: &str,
    asset_group_resource: &str,
    parent_id: i64,
    children: &[ListingGroupPartition],
    operations: &mut Vec<Value>,
    next_id: &mut i64,
) -> Result<()> {
    for child in children {
        let dim_name = child.dimension.as_deref().ok_or_else(|| {
            McpGoogleAdsError::Validation(
                "Each listing group partition under a subdivision needs a dimension".to_string(),
            )
        })?;
        let dim = ListingDimension::parse(dim_name)?;
        let is_other = is_other_node(child);
        if !is_other {
            dim.requires_value_or_category(child.value.as_deref(), child.category_id)?;
        }
        let case_value = dim.rest_key_and_payload(child.value.as_deref(), child.category_id);
        let nested = child.children.as_deref().unwrap_or(&[]);
        let this_id = *next_id;
        *next_id -= 1;

        if !nested.is_empty() {
            operations.push(listing_group_op(
                cid,
                asset_group_id,
                asset_group_resource,
                this_id,
                Some(parent_id),
                "SUBDIVISION",
                Some(case_value),
            ));
            emit_children(
                cid,
                asset_group_id,
                asset_group_resource,
                this_id,
                nested,
                operations,
                next_id,
            )?;
        } else {
            let filter_type = if child.include.unwrap_or(true) {
                "UNIT_INCLUDED"
            } else {
                "UNIT_EXCLUDED"
            };
            operations.push(listing_group_op(
                cid,
                asset_group_id,
                asset_group_resource,
                this_id,
                Some(parent_id),
                filter_type,
                Some(case_value),
            ));
        }
    }
    Ok(())
}

fn listing_group_resource(cid: &str, asset_group_id: &str, filter_id: i64) -> String {
    format!("customers/{cid}/assetGroupListingGroupFilters/{asset_group_id}~{filter_id}")
}

fn listing_group_op(
    cid: &str,
    asset_group_id: &str,
    asset_group_resource: &str,
    filter_id: i64,
    parent_id: Option<i64>,
    filter_type: &str,
    case_value: Option<Value>,
) -> Value {
    let resource = listing_group_resource(cid, asset_group_id, filter_id);
    let mut create = json!({
        "resourceName": resource,
        "assetGroup": asset_group_resource,
        "type": filter_type,
        "listingSource": "SHOPPING"
    });
    if let Some(obj) = create.as_object_mut() {
        if let Some(parent) = parent_id {
            obj.insert(
                "parentListingGroupFilter".to_string(),
                json!(listing_group_resource(cid, asset_group_id, parent)),
            );
        }
        if let Some(cv) = case_value {
            obj.insert("caseValue".to_string(), cv);
        }
    }
    json!({
        "assetGroupListingGroupFilterOperation": {
            "create": create
        }
    })
}

/// An existing listing group filter, enough to order removals and render a tree.
#[derive(Debug, Clone)]
pub struct ExistingListingGroupFilter {
    pub resource_name: String,
    pub id: String,
    pub filter_type: String,
    pub listing_source: String,
    pub parent_resource_name: Option<String>,
    pub dimension_label: Option<String>,
    pub value_label: Option<String>,
}

/// Children-before-parents remove order. Google requires child listing group
/// filters to be removed before their parents.
pub fn listing_group_remove_order(filters: &[ExistingListingGroupFilter]) -> Vec<String> {
    let mut remaining: Vec<&ExistingListingGroupFilter> = filters.iter().collect();
    let mut ordered = Vec::with_capacity(filters.len());

    while !remaining.is_empty() {
        let leaves: Vec<String> = remaining
            .iter()
            .filter(|f| {
                !remaining.iter().any(|other| {
                    other.parent_resource_name.as_deref() == Some(f.resource_name.as_str())
                })
            })
            .map(|f| f.resource_name.clone())
            .collect();

        if leaves.is_empty() {
            // Cycle or missing parent pointer — append whatever is left so the
            // mutate still attempts the replace rather than hanging.
            for f in remaining {
                ordered.push(f.resource_name.clone());
            }
            break;
        }

        for name in &leaves {
            ordered.push(name.clone());
        }
        remaining.retain(|f| !leaves.contains(&f.resource_name));
    }
    ordered
}

pub fn listing_group_remove_operations(resource_names: &[String]) -> Vec<Value> {
    resource_names
        .iter()
        .map(|rn| {
            json!({
                "assetGroupListingGroupFilterOperation": {
                    "remove": rn
                }
            })
        })
        .collect()
}

fn row_str(row: &Value, pointer: &str) -> Option<String> {
    row.pointer(pointer)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn parse_dimension_from_row(filter: &Value) -> (Option<String>, Option<String>) {
    let case = match filter.get("caseValue") {
        Some(v) => v,
        None => return (None, None),
    };

    if let Some(brand) = case.get("productBrand") {
        let value = brand
            .get("value")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        return (Some("BRAND".to_string()), value);
    }
    if let Some(item) = case.get("productItemId") {
        let value = item
            .get("value")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        return (Some("ITEM_ID".to_string()), value);
    }
    if let Some(pt) = case.get("productType") {
        let level = pt
            .get("level")
            .and_then(|v| v.as_str())
            .unwrap_or("LEVEL1")
            .trim_start_matches("LEVEL");
        let value = pt
            .get("value")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        return (Some(format!("PRODUCT_TYPE_L{level}")), value);
    }
    if let Some(pc) = case.get("productCategory") {
        let level = pc
            .get("level")
            .and_then(|v| v.as_str())
            .unwrap_or("LEVEL1")
            .trim_start_matches("LEVEL");
        let value = pc.get("categoryId").and_then(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .or_else(|| v.as_i64().map(|i| i.to_string()))
        });
        return (Some(format!("PRODUCT_CATEGORY_L{level}")), value);
    }
    if let Some(ca) = case.get("productCustomAttribute") {
        let index = ca
            .get("index")
            .and_then(|v| v.as_str())
            .unwrap_or("INDEX0")
            .trim_start_matches("INDEX");
        let value = ca
            .get("value")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        return (Some(format!("CUSTOM_LABEL_{index}")), value);
    }
    if let Some(cond) = case.get("productCondition") {
        let value = cond
            .get("condition")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        return (Some("PRODUCT_CONDITION".to_string()), value);
    }
    if let Some(ch) = case.get("productChannel") {
        let value = ch
            .get("channel")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        return (Some("PRODUCT_CHANNEL".to_string()), value);
    }
    (None, None)
}

pub fn parse_existing_listing_group_row(row: &Value) -> Option<ExistingListingGroupFilter> {
    let filter = row.get("assetGroupListingGroupFilter")?;
    let resource_name = row_str(filter, "/resourceName")?;
    let id = row_str(filter, "/id")
        .unwrap_or_else(|| resource_name.rsplit('~').next().unwrap_or("").to_string());
    let filter_type = row_str(filter, "/type").unwrap_or_default();
    let listing_source = row_str(filter, "/listingSource").unwrap_or_default();
    let parent = row_str(filter, "/parentListingGroupFilter");
    let (dimension_label, value_label) = parse_dimension_from_row(filter);
    Some(ExistingListingGroupFilter {
        resource_name,
        id,
        filter_type,
        listing_source,
        parent_resource_name: parent,
        dimension_label,
        value_label,
    })
}

/// Render existing filters as a nested, readable tree.
pub fn render_listing_group_tree(filters: &[ExistingListingGroupFilter]) -> Value {
    fn node_json(filter: &ExistingListingGroupFilter, all: &[ExistingListingGroupFilter]) -> Value {
        let children: Vec<Value> = all
            .iter()
            .filter(|c| c.parent_resource_name.as_deref() == Some(filter.resource_name.as_str()))
            .map(|c| node_json(c, all))
            .collect();
        let is_other = filter.value_label.as_deref().unwrap_or("").is_empty()
            && filter.parent_resource_name.is_some();
        json!({
            "id": filter.id,
            "resource_name": filter.resource_name,
            "type": filter.filter_type,
            "dimension": filter.dimension_label,
            "value": filter.value_label,
            "everything_else": is_other,
            "children": children,
        })
    }

    let roots: Vec<Value> = filters
        .iter()
        .filter(|f| f.parent_resource_name.is_none())
        .map(|f| node_json(f, filters))
        .collect();

    json!({
        "node_count": filters.len(),
        "roots": roots,
    })
}

const LISTING_GROUP_GAQL: &str = "\
    SELECT \
        asset_group_listing_group_filter.resource_name, \
        asset_group_listing_group_filter.id, \
        asset_group_listing_group_filter.asset_group, \
        asset_group_listing_group_filter.type, \
        asset_group_listing_group_filter.listing_source, \
        asset_group_listing_group_filter.parent_listing_group_filter, \
        asset_group_listing_group_filter.case_value.product_brand.value, \
        asset_group_listing_group_filter.case_value.product_item_id.value, \
        asset_group_listing_group_filter.case_value.product_type.value, \
        asset_group_listing_group_filter.case_value.product_type.level, \
        asset_group_listing_group_filter.case_value.product_category.category_id, \
        asset_group_listing_group_filter.case_value.product_category.level, \
        asset_group_listing_group_filter.case_value.product_custom_attribute.value, \
        asset_group_listing_group_filter.case_value.product_custom_attribute.index, \
        asset_group_listing_group_filter.case_value.product_condition.condition, \
        asset_group_listing_group_filter.case_value.product_channel.channel \
    FROM asset_group_listing_group_filter \
    WHERE asset_group.id = {ASSET_GROUP_ID}";

pub async fn fetch_listing_group_filters(
    client: &GoogleAdsClient,
    customer_id: &str,
    asset_group_id: &str,
) -> Result<Vec<ExistingListingGroupFilter>> {
    validate_numeric_id("asset_group_id", asset_group_id)?;
    let query = LISTING_GROUP_GAQL.replace("{ASSET_GROUP_ID}", asset_group_id);
    let rows = client.search(customer_id, &query).await?;
    Ok(rows
        .iter()
        .filter_map(parse_existing_listing_group_row)
        .collect())
}

/// Read an asset group's listing group tree in a nested, labelled form.
pub async fn get_pmax_listing_group_tree(
    client: &GoogleAdsClient,
    customer_id: &str,
    asset_group_id: &str,
) -> Result<String> {
    let filters = fetch_listing_group_filters(client, customer_id, asset_group_id).await?;
    let tree = render_listing_group_tree(&filters);
    let result = json!({
        "asset_group_id": asset_group_id,
        "tree": tree,
    });
    serde_json::to_string_pretty(&result).map_err(Into::into)
}

/// Replace an asset group's listing group tree atomically (remove existing,
/// create the new tree in the same mutate).
pub async fn set_pmax_listing_groups(
    client: &GoogleAdsClient,
    config: &Config,
    customer_id: &str,
    asset_group_id: &str,
    spec: ListingGroupSpec,
) -> Result<Value> {
    check_blocked_operation("set_pmax_listing_groups", &config.safety)?;
    validate_numeric_id("asset_group_id", asset_group_id)?;

    let cid = GoogleAdsClient::normalize_customer_id(customer_id);
    let existing = fetch_listing_group_filters(client, &cid, asset_group_id).await?;
    let remove_names = listing_group_remove_order(&existing);

    let asset_group_resource = format!("customers/{cid}/assetGroups/{asset_group_id}");
    let (create_ops, _) = build_listing_group_create_operations(
        &cid,
        asset_group_id,
        &asset_group_resource,
        &spec,
        -1,
    )?;

    let mut operations = listing_group_remove_operations(&remove_names);
    operations.extend(create_ops);

    let changes = json!({
        "asset_group_id": asset_group_id,
        "removed_existing_nodes": remove_names.len(),
        "spec": spec,
        "note": "The existing listing group tree is removed and the new tree is created in one atomic mutate. Google requires an 'everything else' sibling at each subdivision; this tool adds that node when the spec omits it.",
    });

    let plan = ChangePlan::new(
        "set_pmax_listing_groups".to_string(),
        "asset_group_listing_group_filter".to_string(),
        asset_group_id.to_string(),
        cid,
        changes,
        false,
        operations,
    );
    let preview = plan.to_preview();
    store_plan(plan);
    Ok(preview)
}

/// Build replace operations without a live fetch — used by unit tests and by
/// callers that already loaded the existing tree.
pub fn build_replace_listing_group_operations(
    cid: &str,
    asset_group_id: &str,
    existing: &[ExistingListingGroupFilter],
    spec: &ListingGroupSpec,
) -> Result<Vec<Value>> {
    let remove_names = listing_group_remove_order(existing);
    let asset_group_resource = format!("customers/{cid}/assetGroups/{asset_group_id}");
    let (create_ops, _) = build_listing_group_create_operations(
        cid,
        asset_group_id,
        &asset_group_resource,
        spec,
        -1,
    )?;
    let mut operations = listing_group_remove_operations(&remove_names);
    operations.extend(create_ops);
    Ok(operations)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn include_only_brand(brand: &str) -> ListingGroupSpec {
        ListingGroupSpec {
            include_only: Some(vec![ListingGroupValue {
                dimension: "BRAND".to_string(),
                value: Some(brand.to_string()),
                category_id: None,
            }]),
            exclude: None,
            partitions: None,
        }
    }

    fn create_ops(spec: &ListingGroupSpec) -> Vec<Value> {
        let (ops, _) = build_listing_group_create_operations(
            "1234567890",
            "111",
            "customers/1234567890/assetGroups/111",
            spec,
            -1,
        )
        .unwrap();
        ops
    }

    fn create_nodes(ops: &[Value]) -> Vec<&Value> {
        ops.iter()
            .filter_map(|op| op.pointer("/assetGroupListingGroupFilterOperation/create"))
            .collect()
    }

    #[test]
    fn empty_spec_is_all_products_root() {
        let ops = create_ops(&ListingGroupSpec::default());
        let creates = create_nodes(&ops);
        assert_eq!(creates.len(), 1);
        assert_eq!(creates[0]["type"], "UNIT_INCLUDED");
        assert_eq!(creates[0]["listingSource"], "SHOPPING");
        assert!(creates[0].get("caseValue").is_none());
        assert!(creates[0].get("parentListingGroupFilter").is_none());
        assert_eq!(
            creates[0]["resourceName"],
            "customers/1234567890/assetGroupListingGroupFilters/111~-1"
        );
    }

    #[test]
    fn include_only_brand_adds_excluded_other() {
        let ops = create_ops(&include_only_brand("Nike"));
        let creates = create_nodes(&ops);
        // root SUBDIVISION + Nike UNIT_INCLUDED + other UNIT_EXCLUDED
        assert_eq!(creates.len(), 3);
        assert_eq!(creates[0]["type"], "SUBDIVISION");
        assert_eq!(creates[1]["type"], "UNIT_INCLUDED");
        assert_eq!(creates[1]["caseValue"]["productBrand"]["value"], "Nike");
        assert_eq!(
            creates[1]["parentListingGroupFilter"],
            "customers/1234567890/assetGroupListingGroupFilters/111~-1"
        );
        assert_eq!(creates[2]["type"], "UNIT_EXCLUDED");
        assert!(creates[2]["caseValue"]["productBrand"]
            .get("value")
            .is_none());
        assert_eq!(creates[2]["listingSource"], "SHOPPING");
    }

    #[test]
    fn exclude_product_types_adds_included_other() {
        let spec = ListingGroupSpec {
            include_only: None,
            exclude: Some(vec![
                ListingGroupValue {
                    dimension: "PRODUCT_TYPE_L1".to_string(),
                    value: Some("Shoes".to_string()),
                    category_id: None,
                },
                ListingGroupValue {
                    dimension: "PRODUCT_TYPE_L1".to_string(),
                    value: Some("Bags".to_string()),
                    category_id: None,
                },
            ]),
            partitions: None,
        };
        let ops = create_ops(&spec);
        let creates = create_nodes(&ops);
        assert_eq!(creates.len(), 4); // root + 2 excludes + other include
        assert_eq!(creates[1]["type"], "UNIT_EXCLUDED");
        assert_eq!(creates[1]["caseValue"]["productType"]["value"], "Shoes");
        assert_eq!(creates[1]["caseValue"]["productType"]["level"], "LEVEL1");
        assert_eq!(creates[2]["type"], "UNIT_EXCLUDED");
        assert_eq!(creates[2]["caseValue"]["productType"]["value"], "Bags");
        assert_eq!(creates[3]["type"], "UNIT_INCLUDED");
        assert!(creates[3]["caseValue"]["productType"]
            .get("value")
            .is_none());
        assert_eq!(creates[3]["caseValue"]["productType"]["level"], "LEVEL1");
    }

    #[test]
    fn product_category_uses_category_id() {
        let spec = ListingGroupSpec {
            include_only: Some(vec![ListingGroupValue {
                dimension: "PRODUCT_CATEGORY_L1".to_string(),
                value: None,
                category_id: Some(166),
            }]),
            exclude: None,
            partitions: None,
        };
        let ops = create_ops(&spec);
        let creates = create_nodes(&ops);
        assert_eq!(
            creates[1]["caseValue"]["productCategory"]["categoryId"],
            "166"
        );
        assert_eq!(
            creates[1]["caseValue"]["productCategory"]["level"],
            "LEVEL1"
        );
        assert!(creates[2]["caseValue"]["productCategory"]
            .get("categoryId")
            .is_none());
    }

    #[test]
    fn custom_label_and_item_id_payloads() {
        let spec = ListingGroupSpec {
            include_only: Some(vec![ListingGroupValue {
                dimension: "CUSTOM_LABEL_0".to_string(),
                value: Some("sale".to_string()),
                category_id: None,
            }]),
            exclude: None,
            partitions: None,
        };
        let ops = create_ops(&spec);
        let creates = create_nodes(&ops);
        assert_eq!(
            creates[1]["caseValue"]["productCustomAttribute"]["value"],
            "sale"
        );
        assert_eq!(
            creates[1]["caseValue"]["productCustomAttribute"]["index"],
            "INDEX0"
        );

        let spec = ListingGroupSpec {
            include_only: Some(vec![ListingGroupValue {
                dimension: "ITEM_ID".to_string(),
                value: Some("SKU-1".to_string()),
                category_id: None,
            }]),
            exclude: None,
            partitions: None,
        };
        let ops = create_ops(&spec);
        let creates = create_nodes(&ops);
        assert_eq!(creates[1]["caseValue"]["productItemId"]["value"], "SKU-1");
    }

    #[test]
    fn mixed_modes_rejected() {
        let spec = ListingGroupSpec {
            include_only: Some(vec![ListingGroupValue {
                dimension: "BRAND".to_string(),
                value: Some("Nike".to_string()),
                category_id: None,
            }]),
            exclude: Some(vec![ListingGroupValue {
                dimension: "BRAND".to_string(),
                value: Some("Adidas".to_string()),
                category_id: None,
            }]),
            partitions: None,
        };
        let err =
            build_listing_group_create_operations("1", "1", "customers/1/assetGroups/1", &spec, -1)
                .unwrap_err()
                .to_string();
        assert!(err.contains("only one of"));
    }

    #[test]
    fn mixed_sibling_dimensions_rejected() {
        let spec = ListingGroupSpec {
            include_only: Some(vec![
                ListingGroupValue {
                    dimension: "BRAND".to_string(),
                    value: Some("Nike".to_string()),
                    category_id: None,
                },
                ListingGroupValue {
                    dimension: "ITEM_ID".to_string(),
                    value: Some("SKU-1".to_string()),
                    category_id: None,
                },
            ]),
            exclude: None,
            partitions: None,
        };
        let err =
            build_listing_group_create_operations("1", "1", "customers/1/assetGroups/1", &spec, -1)
                .unwrap_err()
                .to_string();
        assert!(err.contains("same dimension"));
    }

    #[test]
    fn unknown_dimension_rejected() {
        let err = ListingDimension::parse("COLOR").unwrap_err().to_string();
        assert!(err.contains("Unknown listing group dimension"));
    }

    #[test]
    fn nested_partitions_emit_subdivision_and_other() {
        let spec = ListingGroupSpec {
            include_only: None,
            exclude: None,
            partitions: Some(vec![ListingGroupPartition {
                dimension: Some("BRAND".to_string()),
                value: Some("Nike".to_string()),
                category_id: None,
                include: Some(true),
                children: Some(vec![ListingGroupPartition {
                    dimension: Some("PRODUCT_TYPE_L1".to_string()),
                    value: Some("Shoes".to_string()),
                    category_id: None,
                    include: Some(true),
                    children: None,
                }]),
            }]),
        };
        let ops = create_ops(&spec);
        let creates = create_nodes(&ops);
        // root SUBDIVISION, Nike SUBDIVISION, Shoes UNIT_INCLUDED, type-other UNIT_EXCLUDED
        // (auto), brand-other UNIT_EXCLUDED (auto, because the only specified brand sibling is include)
        assert!(creates.len() >= 5);
        assert_eq!(creates[0]["type"], "SUBDIVISION");
        assert_eq!(creates[1]["type"], "SUBDIVISION");
        assert_eq!(creates[1]["caseValue"]["productBrand"]["value"], "Nike");
        assert_eq!(creates[2]["type"], "UNIT_INCLUDED");
        assert_eq!(creates[2]["caseValue"]["productType"]["value"], "Shoes");
        assert_eq!(creates[3]["type"], "UNIT_EXCLUDED");
        assert!(creates.iter().any(|c| c["type"] == "UNIT_EXCLUDED"
            && c.get("caseValue")
                .and_then(|v| v.get("productBrand"))
                .is_some()
            && c["caseValue"]["productBrand"].get("value").is_none()));
    }

    #[test]
    fn all_excluded_leaves_rejected() {
        let spec = ListingGroupSpec {
            include_only: None,
            exclude: None,
            partitions: Some(vec![
                ListingGroupPartition {
                    dimension: Some("BRAND".to_string()),
                    value: Some("Nike".to_string()),
                    category_id: None,
                    include: Some(false),
                    children: None,
                },
                ListingGroupPartition {
                    dimension: Some("BRAND".to_string()),
                    value: None,
                    category_id: None,
                    include: Some(false),
                    children: None,
                },
            ]),
        };
        let err =
            build_listing_group_create_operations("1", "1", "customers/1/assetGroups/1", &spec, -1)
                .unwrap_err()
                .to_string();
        assert!(err.contains("UNIT_INCLUDED"));
    }

    #[test]
    fn remove_order_is_children_before_parents() {
        let filters = vec![
            ExistingListingGroupFilter {
                resource_name: "root".into(),
                id: "1".into(),
                filter_type: "SUBDIVISION".into(),
                listing_source: "SHOPPING".into(),
                parent_resource_name: None,
                dimension_label: None,
                value_label: None,
            },
            ExistingListingGroupFilter {
                resource_name: "child".into(),
                id: "2".into(),
                filter_type: "UNIT_INCLUDED".into(),
                listing_source: "SHOPPING".into(),
                parent_resource_name: Some("root".into()),
                dimension_label: Some("BRAND".into()),
                value_label: Some("Nike".into()),
            },
            ExistingListingGroupFilter {
                resource_name: "other".into(),
                id: "3".into(),
                filter_type: "UNIT_EXCLUDED".into(),
                listing_source: "SHOPPING".into(),
                parent_resource_name: Some("root".into()),
                dimension_label: Some("BRAND".into()),
                value_label: None,
            },
        ];
        let order = listing_group_remove_order(&filters);
        let root_pos = order.iter().position(|n| n == "root").unwrap();
        let child_pos = order.iter().position(|n| n == "child").unwrap();
        let other_pos = order.iter().position(|n| n == "other").unwrap();
        assert!(child_pos < root_pos);
        assert!(other_pos < root_pos);
    }

    #[test]
    fn replace_emits_removes_then_creates() {
        let existing = vec![ExistingListingGroupFilter {
            resource_name: "customers/1/assetGroupListingGroupFilters/111~99".into(),
            id: "99".into(),
            filter_type: "UNIT_INCLUDED".into(),
            listing_source: "SHOPPING".into(),
            parent_resource_name: None,
            dimension_label: None,
            value_label: None,
        }];
        let ops = build_replace_listing_group_operations(
            "1",
            "111",
            &existing,
            &include_only_brand("Nike"),
        )
        .unwrap();
        assert_eq!(
            ops[0]["assetGroupListingGroupFilterOperation"]["remove"],
            "customers/1/assetGroupListingGroupFilters/111~99"
        );
        assert!(ops[1]
            .pointer("/assetGroupListingGroupFilterOperation/create")
            .is_some());
    }

    #[test]
    fn render_tree_marks_everything_else() {
        let filters = vec![
            ExistingListingGroupFilter {
                resource_name: "root".into(),
                id: "1".into(),
                filter_type: "SUBDIVISION".into(),
                listing_source: "SHOPPING".into(),
                parent_resource_name: None,
                dimension_label: None,
                value_label: None,
            },
            ExistingListingGroupFilter {
                resource_name: "nike".into(),
                id: "2".into(),
                filter_type: "UNIT_INCLUDED".into(),
                listing_source: "SHOPPING".into(),
                parent_resource_name: Some("root".into()),
                dimension_label: Some("BRAND".into()),
                value_label: Some("Nike".into()),
            },
            ExistingListingGroupFilter {
                resource_name: "other".into(),
                id: "3".into(),
                filter_type: "UNIT_EXCLUDED".into(),
                listing_source: "SHOPPING".into(),
                parent_resource_name: Some("root".into()),
                dimension_label: Some("BRAND".into()),
                value_label: None,
            },
        ];
        let tree = render_listing_group_tree(&filters);
        assert_eq!(tree["node_count"], 3);
        assert_eq!(tree["roots"][0]["children"][0]["value"], "Nike");
        assert_eq!(tree["roots"][0]["children"][0]["everything_else"], false);
        assert_eq!(tree["roots"][0]["children"][1]["everything_else"], true);
        assert_eq!(tree["roots"][0]["children"][1]["type"], "UNIT_EXCLUDED");
    }

    #[test]
    fn parse_row_brand() {
        let row = json!({
            "assetGroupListingGroupFilter": {
                "resourceName": "customers/1/assetGroupListingGroupFilters/2~3",
                "id": "3",
                "type": "UNIT_INCLUDED",
                "listingSource": "SHOPPING",
                "parentListingGroupFilter": "customers/1/assetGroupListingGroupFilters/2~1",
                "caseValue": { "productBrand": { "value": "Nike" } }
            }
        });
        let parsed = parse_existing_listing_group_row(&row).unwrap();
        assert_eq!(parsed.dimension_label.as_deref(), Some("BRAND"));
        assert_eq!(parsed.value_label.as_deref(), Some("Nike"));
    }

    #[test]
    fn product_category_without_id_rejected_on_include() {
        let spec = ListingGroupSpec {
            include_only: Some(vec![ListingGroupValue {
                dimension: "PRODUCT_CATEGORY_L1".to_string(),
                value: None,
                category_id: None,
            }]),
            exclude: None,
            partitions: None,
        };
        let err =
            build_listing_group_create_operations("1", "1", "customers/1/assetGroups/1", &spec, -1)
                .unwrap_err()
                .to_string();
        assert!(err.contains("category_id"));
    }
}
