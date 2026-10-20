/**
 * English catalog: the source of truth for every UI string key.
 *
 * Keys are dotted `area.name`. `{name}` marks an interpolated parameter.
 * Other locales must define exactly these keys (see i18n.spec.ts).
 */
export const EN = {
  'nav.downloads': 'Downloads',
  'nav.history': 'History',
  'nav.search': 'Search',
  'nav.rss': 'RSS',
  'nav.media': 'Media',
  'nav.logs': 'Logs',
  'nav.statistics': 'Statistics',
  'nav.settings': 'Settings',
  'nav.welcome': 'Welcome',
  'nav.signIn': 'Sign in',
  'nav.exitDemo': 'Exit demo',

  'status.stale': 'Stale: showing the last known values',
  'status.offline': 'Offline',
  'status.paused': 'Paused',
  'status.live': 'Live',
  'status.queued': '{count} queued',
  'status.free': '{size} free',
  'status.left': '{count} left',

  'speed.setLimit': 'Set speed limit',
  'speed.title': 'Speed limit',
  'speed.current': 'Current: {value}',
  'speed.off': 'off',
  'speed.customLabel': 'Custom speed limit in MB/s',
  'speed.set': 'Set',

  'layout.width': 'Layout width',
  'layout.compact': 'Compact',
  'layout.compactLabel': 'Compact layout',
  'layout.expanded': 'Expanded',
  'layout.expandedLabel': 'Expanded layout',

  'actions.uploadNzb': '+ Upload NZB',
  'actions.resume': 'Resume',
  'actions.pause': 'Pause',
  'actions.pauseFor': 'Pause for…',
  'actions.customPauseLabel': 'Custom pause duration in minutes',
  'actions.go': 'Go',
  'actions.shortcutsTitle': 'Keyboard shortcuts (?)',
  'actions.shortcuts': 'Keyboard shortcuts',
  'actions.signOut': 'Sign out',

  'shell.noServers': 'No news servers are set up yet, so downloads will not start.',
  'shell.addServer': 'Add a server →',
  'shell.dropNzb': 'Drop .nzb files to add them',

  'settings.general.title': 'General',
  'settings.general.sub': 'Global speed limit, concurrency, history retention.',
  'settings.appearance.title': 'Appearance',
  'settings.appearance.themeLabel': 'Application theme',
  'settings.appearance.system': 'System',
  'settings.appearance.systemDescription': "Match this device's light or dark setting.",
  'settings.language.title': 'Language',
  'settings.language.label': 'Interface language',
  'settings.language.description':
    'Stored in this browser. Strings without a translation fall back to English.',
} as const;

export type TranslationKey = keyof typeof EN;
export type Catalog = Record<TranslationKey, string>;
