//! AI Usage dashboard-widget backend descriptor.

use crate::plugins::dashboard_widget::DashboardWidgetBackend;
use crate::plugins::Plugin;

pub struct AiUsageBackend;

impl Plugin for AiUsageBackend {
    fn id(&self) -> &'static str {
        "ai-usage"
    }

    fn display_name(&self) -> &'static str {
        "AI Usage"
    }
}

impl DashboardWidgetBackend for AiUsageBackend {}
