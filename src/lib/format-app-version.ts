// Format the release version for display. Dev builds get a `-dev` suffix so it's
// obvious when a running instance came from `pnpm dev` rather than a release bundle.
export function formatAppVersion(version: string, isDev: boolean): string {
  return isDev ? `${version}-dev` : version;
}
