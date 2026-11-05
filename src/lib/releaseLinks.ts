export const PROJECT_URL = "https://github.com/Xias2y/cc-switch";
export const RELEASES_URL = `${PROJECT_URL}/releases`;

export function releaseNotesUrl(version?: string): string {
  const stable = version?.trim().match(/^v?(0\.\d+\.\d+)$/);
  if (stable) return `${RELEASES_URL}/tag/krill-v${stable[1]}`;
  const match = version?.trim().match(/^v?(\d+\.\d+\.\d+)-krill\.(\d+)$/);
  return match
    ? `${RELEASES_URL}/tag/krill-v${match[1]}-${match[2]}`
    : RELEASES_URL;
}
