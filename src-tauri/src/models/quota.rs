use serde::{Deserialize, Serialize};

/// 模型配额信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelQuota {
    pub name: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub percentage: i32, // 剩余百分比 0-100
    pub reset_time: String,
}

/// AI 积分信息（来自 loadCodeAssist paidTier.availableCredits）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreditInfo {
    pub credit_type: String,
    #[serde(default)]
    pub credit_amount: Option<String>,
    #[serde(default)]
    pub minimum_credit_amount_for_usage: Option<String>,
}

/// 配额数据结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaData {
    pub models: Vec<ModelQuota>,
    pub last_updated: i64,
    #[serde(default)]
    pub is_forbidden: bool,
    /// 订阅等级 (FREE/PRO/ULTRA)
    #[serde(default)]
    pub subscription_tier: Option<String>,
    /// AI 积分列表（仅付费账号可能有）
    #[serde(default)]
    pub credits: Vec<CreditInfo>,
    /// 账号层级 ID（如 free-tier、g1-pro-tier）
    #[serde(default)]
    pub tier_id: Option<String>,
    /// 是否使用 GCP ToS
    #[serde(default)]
    pub is_gcp_tos: Option<bool>,
    /// GCP / Enterprise 项目 ID
    #[serde(default)]
    pub project_id: Option<String>,
}

impl QuotaData {
    pub fn new() -> Self {
        Self {
            models: Vec::new(),
            last_updated: chrono::Utc::now().timestamp(),
            is_forbidden: false,
            subscription_tier: None,
            credits: Vec::new(),
            tier_id: None,
            is_gcp_tos: None,
            project_id: None,
        }
    }

    pub fn add_model(
        &mut self,
        name: String,
        display_name: Option<String>,
        percentage: i32,
        reset_time: String,
    ) {
        self.models.push(ModelQuota {
            name,
            display_name,
            percentage,
            reset_time,
        });
    }

    /// Keep a previously identified plan / project when a later refresh only
    /// got models (or timed out on loadCodeAssist).
    pub fn merge_preserving_identity(self, previous: Option<&QuotaData>) -> Self {
        let Some(previous) = previous else {
            return self;
        };
        let mut merged = self;
        if option_blank(&merged.subscription_tier) {
            merged.subscription_tier = previous.subscription_tier.clone();
        }
        if merged.is_gcp_tos.is_none() {
            merged.is_gcp_tos = previous.is_gcp_tos;
        }
        if option_blank(&merged.project_id) {
            merged.project_id = previous.project_id.clone();
        }
        if merged.credits.is_empty() && !previous.credits.is_empty() {
            merged.credits = previous.credits.clone();
        }
        if merged.models.is_empty() && !previous.models.is_empty() {
            merged.models = previous.models.clone();
        }
        merged
    }
}

fn option_blank(value: &Option<String>) -> bool {
    value
        .as_deref()
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .is_none()
}

impl Default for QuotaData {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_preserving_identity_keeps_previous_tier_when_refresh_has_none() {
        let mut previous = QuotaData::new();
        previous.subscription_tier = Some("g1-pro-tier".to_string());
        previous.is_gcp_tos = Some(true);
        previous.project_id = Some("aicode-consumers".to_string());
        previous.add_model("3p-5h".to_string(), None, 80, "old".to_string());

        let mut incoming = QuotaData::new();
        incoming.add_model("3p-5h".to_string(), None, 100, "new".to_string());

        let merged = incoming.merge_preserving_identity(Some(&previous));
        assert_eq!(merged.subscription_tier.as_deref(), Some("g1-pro-tier"));
        assert_eq!(merged.is_gcp_tos, Some(true));
        assert_eq!(merged.project_id.as_deref(), Some("aicode-consumers"));
        assert_eq!(merged.models[0].percentage, 100);
    }

    #[test]
    fn merge_preserving_identity_does_not_override_fresh_tier() {
        let mut previous = QuotaData::new();
        previous.subscription_tier = Some("g1-pro-tier".to_string());

        let mut incoming = QuotaData::new();
        incoming.subscription_tier = Some("free-tier".to_string());

        let merged = incoming.merge_preserving_identity(Some(&previous));
        assert_eq!(merged.subscription_tier.as_deref(), Some("free-tier"));
    }
}
