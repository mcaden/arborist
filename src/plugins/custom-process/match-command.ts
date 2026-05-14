function shellTokens(command: string): string[] {
  const tokens: string[] = [];
  let current = '';
  let quote: '"' | "'" | undefined;

  for (let i = 0; i < command.length; i++) {
    const ch = command[i]!;
    if (quote) {
      if (ch === quote) {
        quote = undefined;
      } else {
        current += ch;
      }
      continue;
    }

    if (ch === '"' || ch === "'") {
      quote = ch;
      continue;
    }
    if (/\s/.test(ch)) {
      if (current.length > 0) {
        tokens.push(current);
        current = '';
      }
      continue;
    }
    current += ch;
  }

  if (current.length > 0) tokens.push(current);
  return tokens;
}

function firstExecutableToken(command: string): string | undefined {
  for (const token of shellTokens(command)) {
    if (token === 'env' || /^[A-Za-z_][A-Za-z0-9_]*=.*/.test(token)) {
      continue;
    }
    return token;
  }
  return undefined;
}

function executableBasename(command: string): string | undefined {
  const executable = firstExecutableToken(command);
  if (!executable) return undefined;
  const normalized = executable.replace(/\\/g, '/');
  return normalized.slice(normalized.lastIndexOf('/') + 1).toLowerCase();
}

export function looksLikeVsCodeCommand(command: string): boolean {
  const base = executableBasename(command);
  return (
    base === 'code' ||
    base === 'code.cmd' ||
    base === 'code.exe' ||
    base === 'code-insiders' ||
    base === 'code-insiders.cmd' ||
    base === 'code-insiders.exe'
  );
}

export function looksLikeExplorerCommand(command: string): boolean {
  const base = executableBasename(command);
  return base === 'explorer' || base === 'explorer.exe';
}
