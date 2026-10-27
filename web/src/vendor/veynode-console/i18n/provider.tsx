/**
 * Not core's: a stand-in for console's `useI18n` with the English strings the
 * copied components ask for (core's `i18n/locales/en.po`). The web UI has no
 * other language.
 */
const EN: Record<string, string> = {
  'vmConsole.decrease': 'Decrease',
  'vmConsole.increase': 'Increase',
}

export function useI18n() {
  return { t: (id: string) => EN[id] ?? id }
}
