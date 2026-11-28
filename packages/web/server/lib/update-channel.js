// The desktop release workflows publish these two mirrors. Only the settings
// choice selects a channel; it is never a URL supplied by a client.
export const desktopUpdateMirrors = Object.freeze({
  stable: 'https://downloads.openchamber.dev/desktop/stable/',
  canary: 'https://downloads.openchamber.dev/desktop/canary/',
});

export const isUpdateChannel = (value) => value === 'stable' || value === 'canary';
export const isCanaryVersion = (version) => /-canary\./.test(String(version ?? ''));
export const normalizeUpdateChannel = (value, appVersion) => {
  if (isUpdateChannel(value)) return value;
  return isCanaryVersion(appVersion) ? 'canary' : 'stable';
};

// Other clients have their own releases. A browser, or an unrecognized
// caller, checks the host it is connected to.
export function normalizeAppType(value) {
  if (value === 'desktop-electron' || value === 'vscode' || value === 'mobile-capacitor') return value;
  return 'web';
}
