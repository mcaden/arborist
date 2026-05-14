import { render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import { createRegistry } from './index';
import { PluginRegistryProvider } from './registry-provider';
import { useRegistry } from './use-registry';

function RegistryProbe(): JSX.Element {
  const r = useRegistry();
  return (
    <div data-testid="probe">
      ai={r.ai().length};widgets={r.widgets().length}
    </div>
  );
}

describe('PluginRegistryProvider / useRegistry', () => {
  it('exposes the registry to child components', () => {
    const r = createRegistry();
    r.registerAi({
      id: 'claude',
      displayName: 'Claude',
      defaultProgram: 'claude',
      defaultInstructionSetPath: 'claude-default.md',
      contextMetricsLimitTooltipSuffix: 'model nominal max',
      Icon: () => null,
    });
    r.registerWidget({ id: 'git-status', displayName: 'Git', order: 0, Component: () => null });

    render(
      <PluginRegistryProvider registry={r}>
        <RegistryProbe />
      </PluginRegistryProvider>,
    );

    expect(screen.getByTestId('probe')).toHaveTextContent('ai=1;widgets=1');
  });

  it('falls back to a built-in registry when no prop is supplied', () => {
    render(
      <PluginRegistryProvider>
        <RegistryProbe />
      </PluginRegistryProvider>,
    );
    expect(screen.getByTestId('probe')).toHaveTextContent('ai=2;widgets=2');
  });

  it('throws a helpful error when useRegistry() is called outside a provider', () => {
    // Silence React's expected-error console.error so the test output stays clean. The error itself is what we are asserting on. Use vi.spyOn so the
    // original implementation is restored automatically and we don't leak global console state across tests under parallel execution.
    const spy = vi.spyOn(console, 'error').mockImplementation(() => {});
    try {
      expect(() => render(<RegistryProbe />)).toThrow(/useRegistry\(\) called outside <PluginRegistryProvider>/);
    } finally {
      spy.mockRestore();
    }
  });
});
