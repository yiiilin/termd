import { ExternalLink, RefreshCw, X } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import packageMetadata from "../../package.json";
import { useI18n } from "../i18n";
import { checkLatestRelease, isNewerVersion, type VersionCheckResult } from "../version-check";

const APP_VERSION = packageMetadata.version;
const CHECK_DELAY_MS = 3000;
/** 页面打开期间周期性检查新版本的间隔（与 localStorage 缓存 TTL 对齐）。 */
const CHECK_INTERVAL_MS = 60 * 60 * 1000;
/** 更新受理后等待服务重启的秒数，之后自动刷新页面。 */
const UPDATE_RELOAD_DELAY_MS = 10_000;

interface UpdateComponentState {
  /** 组件显示名（termd / termrelay）。 */
  name: string;
  /** 当前运行版本（无 daemon 状态时回退构建版本）。 */
  current: string;
  latest?: string;
  releaseUrl?: string;
  /** 是否有更新。 */
  updateAvailable: boolean;
  /** 点击更新后的状态。 */
  updateState: "idle" | "requesting" | "applied";
}

export interface AppVersionBadgeProps {
  /** daemon 实际运行版本（来自 daemon 状态）；缺失时回退到前端构建版本。 */
  termdVersion?: string;
  /** 已认证的 termd 一键更新回调；返回是否受理。 */
  onUpdateTermd?: () => Promise<boolean>;
  /** 已认证的 relay 一键更新回调（经 daemon 代理）；返回是否受理。 */
  onUpdateRelay?: () => Promise<boolean>;
  /** 是否已连接 daemon（未连接时更新按钮禁用）。 */
  canUpdate?: boolean;
}

/**
 * 版本号徽标：显示运行组件的实际版本，后台探测 GitHub 最新 release；
 * 有新版本时版本号右上角显示黄色小点。经 relay 访问时额外显示 relay 版本，
 * 两个组件各自可一键更新（需已连接 daemon）。
 */
export function AppVersionBadge({
  termdVersion,
  onUpdateTermd,
  onUpdateRelay,
  canUpdate = false,
}: AppVersionBadgeProps) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const [checkState, setCheckState] = useState<"checking" | "done" | "failed">("checking");
  const [updateInfo, setUpdateInfo] = useState<VersionCheckResult | undefined>(undefined);
  const [relayVersion, setRelayVersion] = useState<string | undefined>(undefined);
  const [updateStates, setUpdateStates] = useState<Record<string, "idle" | "requesting" | "applied">>({});

  const effectiveTermdVersion = termdVersion || APP_VERSION;

  const runCheck = useCallback((force = false) => {
    setCheckState("checking");
    void checkLatestRelease({ force }).then((result) => {
      setUpdateInfo(result);
      setCheckState(result ? "done" : "failed");
    });
  }, []);

  useEffect(() => {
    let cancelled = false;
    // 延迟首次探测，避免与首屏加载抢带宽；此后每小时检查一次。
    // 结果缓存在 localStorage（1 小时），缓存有效时复用、不重复请求 GitHub API。
    const runPeriodicCheck = () => {
      void checkLatestRelease().then((result) => {
        if (cancelled) {
          return;
        }
        setUpdateInfo(result);
        setCheckState(result ? "done" : "failed");
      });
    };
    const initialTimer = window.setTimeout(runPeriodicCheck, CHECK_DELAY_MS);
    const interval = window.setInterval(runPeriodicCheck, CHECK_INTERVAL_MS);
    return () => {
      cancelled = true;
      window.clearTimeout(initialTimer);
      window.clearInterval(interval);
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    // 探测当前连接的服务组件：直连 daemon 时 `/version` 返回 termd；
    // 经 relay 时同一路径由 relay 返回 termrelay。
    const detectRelay = async () => {
      try {
        const response = await fetch(`${window.location.origin}/version`, {
          signal: AbortSignal.timeout(5000),
        });
        if (!response.ok) {
          return;
        }
        const body = (await response.json()) as { component?: unknown; version?: unknown };
        if (body.component === "termrelay" && typeof body.version === "string") {
          if (!cancelled) {
            setRelayVersion(body.version);
          }
        }
      } catch {
        // 探测失败静默：直连或未就绪时按「只有 termd」处理
      }
    };
    void detectRelay();
    return () => {
      cancelled = true;
    };
  }, []);

  const hasUpdate = updateInfo !== undefined && isNewerVersion(updateInfo.latest, effectiveTermdVersion);
  const relayHasUpdate = updateInfo !== undefined && relayVersion !== undefined
    && isNewerVersion(updateInfo.latest, relayVersion);

  const requestUpdate = useCallback(async (component: "termd" | "termrelay") => {
    if (component === "termd" && !onUpdateTermd) {
      return;
    }
    if (component === "termrelay" && !onUpdateRelay) {
      return;
    }
    setUpdateStates((current) => ({ ...current, [component]: "requesting" }));
    const accepted = component === "termd"
      ? await onUpdateTermd!()
      : await onUpdateRelay!();
    setUpdateStates((current) => ({ ...current, [component]: accepted ? "applied" : "idle" }));
    if (accepted) {
      // 服务（termd 或 relay）即将重启；延迟后刷新页面，重新探测版本。
      window.setTimeout(() => {
        window.location.reload();
      }, UPDATE_RELOAD_DELAY_MS);
    }
  }, [onUpdateTermd, onUpdateRelay]);

  const termdState: UpdateComponentState = {
    name: "termd",
    current: effectiveTermdVersion,
    latest: updateInfo?.latest,
    releaseUrl: updateInfo?.releaseUrl,
    updateAvailable: hasUpdate,
    updateState: updateStates.termd ?? "idle",
  };
  const relayState: UpdateComponentState | undefined = relayVersion === undefined ? undefined : {
    name: "termrelay",
    current: relayVersion,
    latest: updateInfo?.latest,
    releaseUrl: updateInfo?.releaseUrl,
    updateAvailable: relayHasUpdate,
    updateState: updateStates.termrelay ?? "idle",
  };

  return (
    <>
      <button
        type="button"
        className="app-version-badge"
        aria-label={t("updateCheck.title")}
        onClick={() => setOpen(true)}
      >
        <span className="app-version">v{effectiveTermdVersion}</span>
        {(hasUpdate || relayHasUpdate) ? <span className="app-version-dot" aria-hidden="true" /> : null}
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
              <UpdateComponentRow
                state={termdState}
                canUpdate={canUpdate}
                onUpdate={() => requestUpdate("termd")}
                t={t}
              />
              {relayState ? (
                <UpdateComponentRow
                  state={relayState}
                  canUpdate={canUpdate}
                  onUpdate={() => requestUpdate("termrelay")}
                  t={t}
                />
              ) : null}
              <p className="version-check-note">
                {checkState === "checking"
                  ? t("updateCheck.checking")
                  : checkState === "failed"
                    ? t("updateCheck.checkFailed")
                    : relayVersion === undefined
                      ? (hasUpdate ? t("updateCheck.newVersion") : t("updateCheck.upToDate"))
                      : (hasUpdate || relayHasUpdate ? t("updateCheck.newVersion") : t("updateCheck.upToDate"))}
              </p>
            </div>
            <footer className="version-check-footer">
              <button
                type="button"
                className="version-check-check-button"
                disabled={checkState === "checking"}
                onClick={() => runCheck(true)}
              >
                <RefreshCw size={13} aria-hidden="true" />
                {checkState === "checking" ? t("updateCheck.checkingShort") : t("updateCheck.checkNow")}
              </button>
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

function UpdateComponentRow({
  state,
  canUpdate,
  onUpdate,
  t,
}: {
  state: UpdateComponentState;
  canUpdate: boolean;
  onUpdate: () => void;
  t: (key: "updateCheck.updateNow" | "updateCheck.updating" | "updateCheck.restarting" | "updateCheck.needConnection") => string;
}) {
  return (
    <div className="version-check-component">
      <div className="version-check-row">
        <span>{state.name === "termd" ? "Termd" : "Relay"}</span>
        <strong className={state.updateAvailable ? "version-check-latest" : undefined}>
          v{state.current}
          {state.updateAvailable ? ` → v${state.latest}` : ""}
        </strong>
      </div>
      {state.updateAvailable ? (
        <div className="version-check-update-action">
          {state.updateState === "applied" ? (
            <span className="version-check-updating">{t("updateCheck.restarting")}</span>
          ) : (
            <button
              type="button"
              className="version-check-update-button"
              disabled={state.updateState === "requesting" || !canUpdate}
              onClick={onUpdate}
            >
              <RefreshCw size={13} aria-hidden="true" />
              {state.updateState === "requesting" ? t("updateCheck.updating") : t("updateCheck.updateNow")}
            </button>
          )}
          {!canUpdate ? <span className="version-check-need-connection">{t("updateCheck.needConnection")}</span> : null}
        </div>
      ) : null}
    </div>
  );
}
