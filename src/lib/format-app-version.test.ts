import { describe, expect, it } from 'vitest';

import { formatAppVersion } from './format-app-version';

describe('formatAppVersion', () => {
  it('suffixes dev builds with -dev', () => {
    expect(formatAppVersion('1.2.3', true)).toBe('1.2.3-dev');
  });

  it('leaves production builds bare', () => {
    expect(formatAppVersion('1.2.3', false)).toBe('1.2.3');
  });
});
