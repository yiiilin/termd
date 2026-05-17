import { Bell, Keyboard, MonitorCog, Palette, Languages, X } from "lucide-react";
import type { BrowserNotificationPreference, BrowserPreferences, BrowserLanguagePreference, BrowserThemePreference, EffectiveTheme } from "../protocol/types";
import { useI18n, type Locale } from "../i18n";

interface SettingsDialogProps {
  open: boolean;
  preferences: BrowserPreferences;
  effectiveLocale: Locale;
  effectiveTheme: EffectiveTheme;
  onPreferencesChange: (preferences: BrowserPreferences) => void;
  onClose: () => void;
}

export function SettingsDialog({
  open,
  preferences,
  effectiveLocale,
  effectiveTheme,
  onPreferencesChange,
  onClose,
}: SettingsDialogProps) {
  const { t } = useI18n();

  if (!open) {
    return null;
  }

  const setLanguage = (language: BrowserLanguagePreference) => onPreferencesChange({ ...preferences, language });
  const setTheme = (theme: BrowserThemePreference) => onPreferencesChange({ ...preferences, theme });
  const setNotifications = (notifications: BrowserNotificationPreference) => onPreferencesChange({ ...preferences, notifications });
  const setMobileShortcutsText = (text: string) =>
    onPreferencesChange({
      ...preferences,
      mobileShortcuts: parseMobileShortcutsText(text),
    });

  return (
    <div className="modal-backdrop settings-backdrop" role="presentation" onMouseDown={(event) => event.target === event.currentTarget && onClose()}>
      <section
        className="settings-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="settings-title"
      >
        <header className="settings-header">
          <div className="settings-title-group">
            <MonitorCog size={17} aria-hidden="true" />
            <div>
              <h2 id="settings-title">{t("settings.title")}</h2>
              <span>{t("settings.subtitle")}</span>
            </div>
          </div>
          <button type="button" className="icon-button" aria-label={t("settings.close")} onClick={onClose}>
            <X size={16} aria-hidden="true" />
          </button>
        </header>

        <div className="settings-body">
          <fieldset className="settings-fieldset">
            <legend>
              <Languages size={15} aria-hidden="true" />
              <span>{t("settings.language")}</span>
            </legend>
            <div className="settings-segmented" role="radiogroup" aria-label={t("settings.language")}>
              {languageOptions.map((option) => (
                <label key={option.value}>
                  <input
                    type="radio"
                    name="termd-language"
                    value={option.value}
                    checked={preferences.language === option.value}
                    onChange={() => setLanguage(option.value)}
                  />
                  <span>{t(option.labelKey)}</span>
                </label>
              ))}
            </div>
            <p>{t("settings.effective", { value: t(localeLabelKey(effectiveLocale)) })}</p>
          </fieldset>

          <fieldset className="settings-fieldset">
            <legend>
              <Palette size={15} aria-hidden="true" />
              <span>{t("settings.theme")}</span>
            </legend>
            <div className="settings-segmented" role="radiogroup" aria-label={t("settings.theme")}>
              {themeOptions.map((option) => (
                <label key={option.value}>
                  <input
                    type="radio"
                    name="termd-theme"
                    value={option.value}
                    checked={preferences.theme === option.value}
                    onChange={() => setTheme(option.value)}
                  />
                  <span>{t(option.labelKey)}</span>
                </label>
              ))}
            </div>
            <p>{t("settings.effective", { value: t(themeLabelKey(effectiveTheme)) })}</p>
          </fieldset>

          <fieldset className="settings-fieldset">
            <legend>
              <Bell size={15} aria-hidden="true" />
              <span>{t("settings.notifications")}</span>
            </legend>
            <div className="settings-segmented" role="radiogroup" aria-label={t("settings.notifications")}>
              {notificationOptions.map((option) => (
                <label key={option.value}>
                  <input
                    type="radio"
                    name="termd-notifications"
                    value={option.value}
                    checked={(preferences.notifications ?? "off") === option.value}
                    onChange={() => setNotifications(option.value)}
                  />
                  <span>{t(option.labelKey)}</span>
                </label>
              ))}
            </div>
          </fieldset>

          <fieldset className="settings-fieldset">
            <legend>
              <Keyboard size={15} aria-hidden="true" />
              <span>{t("settings.mobileShortcuts")}</span>
            </legend>
            <textarea
              className="settings-shortcuts-textarea"
              value={mobileShortcutsText(preferences.mobileShortcuts ?? [])}
              placeholder={"PgUp=\\u001b[5~\nPgDn=\\u001b[6~"}
              spellCheck={false}
              onChange={(event) => setMobileShortcutsText(event.currentTarget.value)}
            />
            <p>{t("settings.mobileShortcutsHelp")}</p>
          </fieldset>
        </div>
      </section>
    </div>
  );
}

const languageOptions: Array<{ value: BrowserLanguagePreference; labelKey: "settings.language.auto" | "settings.language.zhCN" | "settings.language.enUS" }> = [
  { value: "auto", labelKey: "settings.language.auto" },
  { value: "zh-CN", labelKey: "settings.language.zhCN" },
  { value: "en-US", labelKey: "settings.language.enUS" },
];

const themeOptions: Array<{ value: BrowserThemePreference; labelKey: "settings.theme.system" | "settings.theme.dark" | "settings.theme.light" }> = [
  { value: "system", labelKey: "settings.theme.system" },
  { value: "dark", labelKey: "settings.theme.dark" },
  { value: "light", labelKey: "settings.theme.light" },
];

const notificationOptions: Array<{ value: BrowserNotificationPreference; labelKey: "settings.notifications.off" | "settings.notifications.mentions" | "settings.notifications.all" }> = [
  { value: "off", labelKey: "settings.notifications.off" },
  { value: "mentions", labelKey: "settings.notifications.mentions" },
  { value: "all", labelKey: "settings.notifications.all" },
];

function localeLabelKey(locale: Locale): "settings.language.zhCN" | "settings.language.enUS" {
  return locale === "zh-CN" ? "settings.language.zhCN" : "settings.language.enUS";
}

function themeLabelKey(theme: EffectiveTheme): "settings.theme.dark" | "settings.theme.light" {
  return theme === "dark" ? "settings.theme.dark" : "settings.theme.light";
}

function mobileShortcutsText(shortcuts: NonNullable<BrowserPreferences["mobileShortcuts"]>): string {
  return shortcuts.map((shortcut) => `${shortcut.label}=${escapeShortcutData(shortcut.data)}`).join("\n");
}

function parseMobileShortcutsText(text: string): NonNullable<BrowserPreferences["mobileShortcuts"]> {
  return text
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line) => {
      const separator = line.indexOf("=");
      if (separator <= 0) {
        return undefined;
      }
      const label = line.slice(0, separator).trim().slice(0, 12);
      const data = unescapeShortcutData(line.slice(separator + 1).trim()).slice(0, 64);
      return label && data ? { label, data } : undefined;
    })
    .filter((shortcut): shortcut is NonNullable<typeof shortcut> => Boolean(shortcut))
    .slice(0, 12);
}

function escapeShortcutData(data: string): string {
  return data
    .replace(/\\/g, "\\\\")
    .replace(/\x1b/g, "\\e")
    .replace(/\t/g, "\\t")
    .replace(/\r/g, "\\r")
    .replace(/\n/g, "\\n");
}

function unescapeShortcutData(data: string): string {
  return data
    .replace(/\\e/g, "\x1b")
    .replace(/\\t/g, "\t")
    .replace(/\\r/g, "\r")
    .replace(/\\n/g, "\n")
    .replace(/\\\\/g, "\\");
}
