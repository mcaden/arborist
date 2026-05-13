import type { DashboardWidgetPlugin } from '@/plugins/registry';

import { AiUsageWidget } from './widget';

export const aiUsageWidget: DashboardWidgetPlugin = {
  id: 'ai-usage',
  displayName: 'AI Usage',
  order: 200,
  Component: AiUsageWidget,
};
