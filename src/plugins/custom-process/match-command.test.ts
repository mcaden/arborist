import { describe, expect, it } from 'vitest';

import { looksLikeExplorerCommand, looksLikeVsCodeCommand } from './match-command';

describe('custom-process command matching', () => {
  it('recognizes canonical VS Code launcher commands', () => {
    for (const command of [
      'code .',
      'code.cmd .',
      'code.exe .',
      'code-insiders .',
      'code-insiders.cmd .',
      'code-insiders.exe .',
      '"C:\\Users\\me\\AppData\\Local\\Programs\\Microsoft VS Code\\bin\\code.cmd" .',
      'env ELECTRON_RUN_AS_NODE=0 code .',
      'FOO=bar code .',
    ]) {
      expect(looksLikeVsCodeCommand(command), command).toBe(true);
    }
  });

  it('rejects non-VS Code commands', () => {
    for (const command of ['', 'codium .', 'code-server .', 'my-code .', 'pwsh -c code', '/usr/bin/codium .']) {
      expect(looksLikeVsCodeCommand(command), command).toBe(false);
    }
  });

  it('recognizes canonical Windows Explorer launcher commands', () => {
    for (const command of ['explorer .', 'explorer.exe .', '"C:\\Windows\\explorer.exe" .', 'env FOO=bar explorer .']) {
      expect(looksLikeExplorerCommand(command), command).toBe(true);
    }
  });

  it('rejects non-Explorer commands', () => {
    for (const command of ['', 'open .', 'xdg-open .', 'my-explorer .', 'pwsh -c explorer']) {
      expect(looksLikeExplorerCommand(command), command).toBe(false);
    }
  });
});
