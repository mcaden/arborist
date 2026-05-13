import type { DashboardWidgetPlugin } from '@/plugins/registry';

import { GitStatusWidget } from './widget';

export const gitStatusWidget: DashboardWidgetPlugin = {
  id: 'git-status',
  displayName: 'Git Status',
  order: 100,
  Component: GitStatusWidget,
};
