import { render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it } from 'vitest';

import { useSessionStore } from '@/store/session-store';
import type { SessionView, WorktreeTabId } from '@/types/arborist';

import { aiUsageWidget } from './index';

const TAB_ID = 'tab-feature-x' as WorktreeTabId;

function session(id: string, worktreePath: string, status: SessionView['status'] = 'running'): SessionView {
  return {
    id,
    tool: 'claude',
    worktreePath,
    worktreeName: 'feature-x',
    label: id,
    status,
    createdAt: 0,
    tabIndex: 0,
  };
}

function renderWidget(tabPath = '/repo/feature-x'): ReturnType<typeof render> {
  const Component = aiUsageWidget.Component;
  return render(<Component tabId={TAB_ID} tabPath={tabPath} />);
}

beforeEach(() => {
  useSessionStore.setState({
    sessions: [],
    activeId: undefined,
    metrics: {},
    isHydrated: false,
  });
});

describe('ai-usage dashboard widget', () => {
  it('shows the empty-state hint when no children exist', () => {
    renderWidget();
    expect(screen.getByText(/no agents yet/i)).toBeInTheDocument();
  });

  it('shows child count and status breakdown for this worktree', () => {
    useSessionStore.setState({
      sessions: [session('s1', '/repo/feature-x', 'running'), session('s2', '/repo/feature-x', 'error'), session('s3', '/other', 'running')],
      isHydrated: true,
    });

    renderWidget();

    expect(screen.getByText(/2 agents in this worktree/i)).toBeInTheDocument();
    expect(screen.getByTestId('worktree-dashboard-status-running')).toHaveTextContent(/running: 1/i);
    expect(screen.getByTestId('worktree-dashboard-status-error')).toHaveTextContent(/error: 1/i);
  });

  it('aggregates input/output tokens across sessions for this worktree only', () => {
    useSessionStore.setState({
      sessions: [session('s1', '/repo/feature-x'), session('s2', '/repo/feature-x'), session('s3', '/other')],
      metrics: {
        s1: { sessionId: 's1', inputTokens: 100, outputTokens: 50, model: 'claude-sonnet-4-6', observedAt: 1 },
        s2: { sessionId: 's2', inputTokens: 200, outputTokens: 75, model: 'claude-sonnet-4-6', observedAt: 2 },
        s3: { sessionId: 's3', inputTokens: 999, outputTokens: 999, observedAt: 3 },
      },
      isHydrated: true,
    });

    renderWidget();

    expect(screen.getByTestId('worktree-dashboard-input-tokens')).toHaveTextContent('300');
    expect(screen.getByTestId('worktree-dashboard-output-tokens')).toHaveTextContent('125');
  });
});
