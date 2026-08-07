import { ExternalLink, X } from "lucide-react";
import { useEffect, useState } from "react";
import packageMetadata from "../../package.json";
import { useI18n } from "../i18n";
import { checkLatestRelease, isNewerVersion, type VersionCheckResult } from "../version-check";

const APP_VERSION = packageMetadata.version;
const CHECK_DELAY_MS = 3000;
/** 页面打开期间周期性检查新版本的间隔（与 localStorage 缓存 TTL 对齐）。 */
const CHECK_INTERVAL_MS = 60 * 60 * 1000;

/**
 * 版本号徽标：显示当前构建版本，后台探测 GitHub 最新 release；
 * 有新版本时在版本号右上角显示黄色小点，点击弹出版本信息小窗
 * （当前版本 / 最新版本 / GitHub Release 跳转）。
 */
export function AppVersionBadge() {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const [checkState, setCheckState] = useState<"checking" | "done" | "failed">("checking");
  const [updateInfo, setUpdateInfo] = useState<VersionCheckResult | undefined>(undefined);

  useEffect(() => {
    let cancelled = false;
    // 延迟首次探测，避免与首屏加载抢带宽；此后每小时检查一次，
    // 新版本发布后页面无需刷新也能在缓存过期后看到提示。
    // 结果缓存在 localStorage（1 小时），缓存有效时复用、不重复请求 GitHub API。
    const runCheck = () => {
      void checkLatestRelease().then((result) => {
        if (cancelled) {
          return;
        }
        setUpdateInfo(result);
        setCheckState(result ? "done" : "failed");
      });
    };
    const initialTimer = window.setTimeout(runCheck, CHECK_DELAY_MS);
    const interval = window.setInterval(runCheck, CHECK_INTERVAL_MS);
    return () => {
      cancelled = true;
      window.clearTimeout(initialTimer);
      window.clearInterval(interval);
    };
  }, []);

  const hasUpdate = updateInfo !== undefined && isNewerVersion(updateInfo.latest, APP_VERSION);

  return (
    <>
      <button
        type="button"
        className="app-version-badge"
        aria-label={t("updateCheck.title")}
        onClick={() => setOpen(true)}
      >
        <span className="app-version">v{APP_VERSION}</span>
        {hasUpdate ? <span className="app-version-dot" aria-hidden="true" /> : null}
      </button>
      {open ? (
        <div
          className="modal-backdrop version-check-backdrop"
          role="presentation"
          onMouseDown={(event) => event.target === event.currentTarget && setOpen(false)}
        >
          <section
            className="version-check-dialog"
            role="dialog"
            aria-modal="true"
            aria-labelledby="version-check-title"
          >
            <header className="version-check-header">
              <h2 id="version-check-title">{t("updateCheck.title")}</h2>
              <button
                type="button"
                className="icon-button"
                aria-label={t("settings.close")}
                onClick={() => setOpen(false)}
              >
                <X size={16} aria-hidden="true" />
              </button>
            </header>
            <div className="version-check-body">
              <div className="version-check-row">
                <span>{t("updateCheck.current")}</span>
                <strong>v{APP_VERSION}</strong>
              </div>
              {hasUpdate ? (
                <div className="version-check-row">
                  <span>{t("updateCheck.latest")}</span>
                  <strong className="version-check-latest">v{updateInfo.latest}</strong>
                </div>
              ) : null}
              <p className="version-check-note">
                {checkState === "checking"
                  ? t("updateCheck.checking")
                  : checkState === "failed"
                    ? t("updateCheck.checkFailed")
                    : hasUpdate
                      ? t("updateCheck.newVersion")
                      : t("updateCheck.upToDate")}
              </p>
            </div>
            <footer className="version-check-footer">
              <a
                href={updateInfo?.releaseUrl ?? "https://github.com/yiiilin/termd/releases"}
                target="_blank"
                rel="noreferrer"
                className="version-check-release-link"
              >
                <ExternalLink size={14} aria-hidden="true" />
                {t("updateCheck.openRelease")}
              </a>
            </footer>
          </section>
        </div>
      ) : null}
    </>
  );
}
