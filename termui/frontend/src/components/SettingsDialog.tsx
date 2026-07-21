import { Bell, Keyboard, MonitorCog, Palette, Languages, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { BrowserNotificationPreference, BrowserPreferences, BrowserLanguagePreference, BrowserThemePreference, EffectiveTheme } from "../protocol/types";
import { useI18n, type Locale } from "../i18n";
import { useModalFocus } from "./useModalFocus";

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
  const dialogRef = useModalFocus({ open, onClose });
  const committedMobileShortcutsText = mobileShortcutsText(preferences.mobileShortcuts ?? []);
  const [mobileShortcutsDraft, setMobileShortcutsDraft] = useState(committedMobileShortcutsText);
  const wasOpenRef = useRef(false);

  useEffect(() => {
    if (open && !wasOpenRef.current) {
      setMobileShortcutsDraft(committedMobileShortcutsText);
    }
    wasOpenRef.current = open;
  }, [committedMobileShortcutsText, open]);

  if (!open) {
    return null;
  }

  const setLanguage = (language: BrowserLanguagePreference) => onPreferencesChange({ ...preferences, language });
  const setTheme = (theme: BrowserThemePreference) => onPreferencesChange({ ...preferences, theme });
  const setNotifications = (notifications: BrowserNotificationPreference) => onPreferencesChange({ ...preferences, notifications });
  const mobileShortcutsValidation = parseMobileShortcutsText(mobileShortcutsDraft);
  const mobileShortcutsDirty = mobileShortcutsDraft !== committedMobileShortcutsText;
  const mobileShortcutsError = mobileShortcutsValidation.error
    ? t(`settings.mobileShortcuts.error.${mobileShortcutsValidation.error.code}`, { line: mobileShortcutsValidation.error.line })
    : undefined;
  const applyMobileShortcuts = () => {
    if (!mobileShortcutsDirty || mobileShortcutsValidation.error) {
      return;
    }

    setMobileShortcutsDraft(mobileShortcutsText(mobileShortcutsValidation.shortcuts));
    onPreferencesChange({
      ...preferences,
      mobileShortcuts: mobileShortcutsValidation.shortcuts,
    });
  };

  return (
    <div className="modal-backdrop settings-backdrop" role="presentation" onMouseDown={(event) => event.target === event.currentTarget && onClose()}>
      <section
        ref={dialogRef}
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
              value={mobileShortcutsDraft}
              placeholder={"PgUp=\\u001b[5~\nPgDn=\\u001b[6~"}
              spellCheck={false}
              aria-label={t("settings.mobileShortcuts")}
              aria-invalid={Boolean(mobileShortcutsError)}
              aria-describedby={mobileShortcutsError ? "settings-mobile-shortcuts-help settings-mobile-shortcuts-error" : "settings-mobile-shortcuts-help"}
              onChange={(event) => setMobileShortcutsDraft(event.currentTarget.value)}
            />
            <p id="settings-mobile-shortcuts-help">{t("settings.mobileShortcutsHelp")}</p>
            {mobileShortcutsError ? (
              <p id="settings-mobile-shortcuts-error" className="settings-shortcuts-error" role="alert">
                {mobileShortcutsError}
              </p>
            ) : null}
          </fieldset>
        </div>

        <footer className="settings-footer">
          <button type="button" onClick={() => setMobileShortcutsDraft(committedMobileShortcutsText)}>
            {t("settings.mobileShortcuts.cancel")}
          </button>
          <button
            type="button"
            className="settings-shortcuts-apply"
            disabled={!mobileShortcutsDirty || Boolean(mobileShortcutsError)}
            onClick={applyMobileShortcuts}
          >
            {t("settings.mobileShortcuts.apply")}
          </button>
        </footer>
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

type MobileShortcutsErrorCode =
  | "format"
  | "emptyLabel"
  | "emptyData"
  | "labelTooLong"
  | "dataTooLong"
  | "nul"
  | "invalidEscape"
  | "tooMany";

interface MobileShortcutsValidation {
  shortcuts: NonNullable<BrowserPreferences["mobileShortcuts"]>;
  error?: {
    line: number;
    code: MobileShortcutsErrorCode;
  };
}

function parseMobileShortcutsText(text: string): MobileShortcutsValidation {
  const shortcuts: NonNullable<BrowserPreferences["mobileShortcuts"]> = [];
  const lines = text.split(/\r?\n/);

  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index];
    if (!line.trim()) {
      continue;
    }

    const lineNumber = index + 1;
    const separator = line.indexOf("=");
    if (separator < 0) {
      return validationError(shortcuts, lineNumber, "format");
    }

    const label = line.slice(0, separator).trim();
    const escapedData = line.slice(separator + 1);
    if (!label) {
      return validationError(shortcuts, lineNumber, "emptyLabel");
    }
    if (!escapedData) {
      return validationError(shortcuts, lineNumber, "emptyData");
    }
    if (label.length > 12) {
      return validationError(shortcuts, lineNumber, "labelTooLong");
    }

    const decoded = unescapeShortcutData(escapedData);
    if (decoded.error) {
      return validationError(shortcuts, lineNumber, decoded.error);
    }
    if (decoded.data.includes("\0")) {
      return validationError(shortcuts, lineNumber, "nul");
    }
    if (decoded.data.length > 64) {
      return validationError(shortcuts, lineNumber, "dataTooLong");
    }
    if (shortcuts.length >= 12) {
      return validationError(shortcuts, lineNumber, "tooMany");
    }

    shortcuts.push({ label, data: decoded.data });
  }

  return { shortcuts };
}

function validationError(
  shortcuts: NonNullable<BrowserPreferences["mobileShortcuts"]>,
  line: number,
  code: MobileShortcutsErrorCode,
): MobileShortcutsValidation {
  return { shortcuts, error: { line, code } };
}

function escapeShortcutData(data: string): string {
  return data
    .replace(/\\/g, "\\\\")
    .replace(/\x1b/g, "\\e")
    .replace(/\t/g, "\\t")
    .replace(/\r/g, "\\r")
    .replace(/\n/g, "\\n");
}

function unescapeShortcutData(data: string): { data: string; error?: "invalidEscape" } {
  let decoded = "";

  for (let index = 0; index < data.length; index += 1) {
    const character = data[index];
    if (character !== "\\") {
      decoded += character;
      continue;
    }

    const escape = data[index + 1];
    if (escape === undefined) {
      return { data: decoded, error: "invalidEscape" };
    }

    if (escape === "u" && (data.slice(index + 1, index + 6) === "u001b" || data.slice(index + 1, index + 6) === "u001B")) {
      decoded += "\x1b";
      index += 5;
      continue;
    }

    const escapedCharacter = shortcutEscapes[escape];
    if (escapedCharacter === undefined) {
      return { data: decoded, error: "invalidEscape" };
    }
    decoded += escapedCharacter;
    index += 1;
  }

  return { data: decoded };
}

const shortcutEscapes: Record<string, string> = {
  "\\": "\\",
  e: "\x1b",
  t: "\t",
  r: "\r",
  n: "\n",
};
