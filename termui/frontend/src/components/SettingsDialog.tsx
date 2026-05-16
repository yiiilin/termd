import { MonitorCog, Palette, Languages, X } from "lucide-react";
import type { BrowserPreferences, BrowserLanguagePreference, BrowserThemePreference, EffectiveTheme } from "../protocol/types";
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

function localeLabelKey(locale: Locale): "settings.language.zhCN" | "settings.language.enUS" {
  return locale === "zh-CN" ? "settings.language.zhCN" : "settings.language.enUS";
}

function themeLabelKey(theme: EffectiveTheme): "settings.theme.dark" | "settings.theme.light" {
  return theme === "dark" ? "settings.theme.dark" : "settings.theme.light";
}
