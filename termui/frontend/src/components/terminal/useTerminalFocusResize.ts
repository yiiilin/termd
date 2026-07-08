import { useCallback, useRef, useState } from "react";
import type { TerminalSize } from "../../protocol/types";

interface InstallTerminalFocusResizeListenersOptions {
  host: HTMLElement;
  focusOutSettleMs: number;
  isPassiveInputTarget?: (target: EventTarget | null) => boolean;
  isMobileInputMode?: () => boolean;
  isMobileKeyboardOpen?: () => boolean;
  reportTerminalFocus: (focused: boolean) => void;
  queueCursorReport: (options?: { immediate?: boolean }) => void;
  restoreTerminalInputFocus?: () => void;
  scheduleScrollToBottomIfPinned: () => void;
  resize: (source?: "layout" | "focus" | "session" | "snapshot" | "mobile-viewport") => void;
}

const MOBILE_INPUT_FOCUS_RESCUE_MS = 700;
const MOBILE_INPUT_FOCUS_RESTORE_DELAYS_MS = [0, 16, 48, 120, 240] as const;

export function useTerminalFocusResizeState() {
  const focusedRef = useRef(false);
  const clientSizeRef = useRef<TerminalSize | undefined>(undefined);
  const focusActivationArmedRef = useRef(false);
  const passiveFocusBypassRef = useRef(false);
  const passiveInputFocusRef = useRef(false);
  const suppressPassiveFocusRef = useRef(false);
  const windowActiveRef = useRef(true);
  const terminalOwnedFocusBeforeWindowBlurRef = useRef(false);
  const focusOutTimerRef = useRef<number | undefined>(undefined);
  const mobileInputFocusRescueUntilRef = useRef(0);
  const mobileInputFocusRestoreTimersRef = useRef<number[]>([]);
  const [focused, setFocused] = useState(false);

  const clearPendingFocusOut = useCallback(() => {
    if (focusOutTimerRef.current === undefined) {
      return;
    }
    window.clearTimeout(focusOutTimerRef.current);
    focusOutTimerRef.current = undefined;
  }, []);

  const armMobileInputFocusRescue = useCallback(() => {
    mobileInputFocusRescueUntilRef.current = Date.now() + MOBILE_INPUT_FOCUS_RESCUE_MS;
  }, []);

  const clearMobileInputFocusRescue = useCallback(() => {
    mobileInputFocusRescueUntilRef.current = 0;
  }, []);

  const clearScheduledMobileInputFocusRestore = useCallback(() => {
    for (const timer of mobileInputFocusRestoreTimersRef.current) {
      window.clearTimeout(timer);
    }
    mobileInputFocusRestoreTimersRef.current = [];
  }, []);

  const cancelMobileInputFocusRecovery = useCallback(() => {
    clearPendingFocusOut();
    clearScheduledMobileInputFocusRestore();
    clearMobileInputFocusRescue();
  }, [clearMobileInputFocusRescue, clearPendingFocusOut, clearScheduledMobileInputFocusRestore]);

  const mobileInputFocusRescueActive = useCallback(() => (
    Date.now() <= mobileInputFocusRescueUntilRef.current
  ), []);

  const installTerminalFocusResizeListeners = useCallback((options: InstallTerminalFocusResizeListenersOptions) => {
    const isMobileInputModeEnabled = () => Boolean(options.isMobileInputMode?.());
    const isMobileKeyboardOpenFromProps = () => Boolean(options.isMobileKeyboardOpen?.());
    const mobileInputFocusRescueActiveForListeners = () =>
      isMobileInputModeEnabled() && mobileInputFocusRescueActive();
    const blurActiveTerminalElement = () => {
      const activeElement = document.activeElement;
      if (activeElement instanceof HTMLElement && options.host.contains(activeElement)) {
        activeElement.blur();
      }
    };
    const terminalHasDomFocus = () => {
      const activeElement = document.activeElement;
      return activeElement instanceof HTMLElement && options.host.contains(activeElement);
    };
    const commitFocusOutIfStillOutsideHost = () => {
      const activeElement = document.activeElement;
      if (activeElement instanceof HTMLElement && options.host.contains(activeElement)) {
        return;
      }
      if (shouldRestoreMobileInputFocus()) {
        scheduleMobileInputFocusRestore();
        return;
      }
      terminalOwnedFocusBeforeWindowBlurRef.current = false;
      passiveFocusBypassRef.current = false;
      passiveInputFocusRef.current = false;
      focusActivationArmedRef.current = false;
      windowActiveRef.current = false;
      clearMobileInputFocusRescue();
      options.reportTerminalFocus(false);
      blurActiveTerminalElement();
    };
    const isProbablyMobileKeyboardOpen = () => {
      const viewport = window.visualViewport;
      if (!viewport) {
        return false;
      }
      const viewportHeight = Math.round(viewport.height ?? window.innerHeight);
      const viewportOffsetTop = Math.round(viewport.offsetTop ?? 0);
      const keyboardInset = Math.max(0, Math.round(window.innerHeight - viewportHeight - viewportOffsetTop));
      return keyboardInset >= 80;
    };
    const isMobileKeyboardLikelyOpen = () => isMobileKeyboardOpenFromProps() || isProbablyMobileKeyboardOpen();
    const mobileInputOwnershipActive = () => (
      focusedRef.current ||
      focusOutTimerRef.current !== undefined ||
      passiveInputFocusRef.current ||
      focusActivationArmedRef.current ||
      isMobileKeyboardLikelyOpen() ||
      mobileInputFocusRescueActiveForListeners()
    );
    const isMobileTransientFocusTransition = () => {
      if (!isMobileInputModeEnabled()) {
        return false;
      }
      return mobileInputOwnershipActive();
    };
    const shouldRestoreMobileInputFocus = () => {
      if (!isMobileInputModeEnabled()) {
        return false;
      }
      if (document.visibilityState === "hidden") {
        return false;
      }
      if (!mobileInputOwnershipActive()) {
        return false;
      }
      const activeElement = document.activeElement;
      if (!(activeElement instanceof HTMLElement)) {
        return false;
      }
      if (options.isPassiveInputTarget?.(activeElement)) {
        return false;
      }
      return (
        activeElement === document.body ||
        activeElement === document.documentElement ||
        options.host.contains(activeElement)
      );
    };
    const restoreMobileInputFocusIfNeeded = () => {
      if (!shouldRestoreMobileInputFocus()) {
        return false;
      }
      options.restoreTerminalInputFocus?.();
      return true;
    };
    const scheduleMobileInputFocusRestore = () => {
      if (!isMobileInputModeEnabled() || document.visibilityState === "hidden") {
        return;
      }
      clearScheduledMobileInputFocusRestore();
      for (const delay of MOBILE_INPUT_FOCUS_RESTORE_DELAYS_MS) {
        const timer = window.setTimeout(() => {
          mobileInputFocusRestoreTimersRef.current = mobileInputFocusRestoreTimersRef.current.filter((item) => item !== timer);
          if (!restoreMobileInputFocusIfNeeded()) {
            return;
          }
          const activeElement = document.activeElement;
          if (activeElement instanceof HTMLElement && options.isPassiveInputTarget?.(activeElement)) {
            clearScheduledMobileInputFocusRestore();
          }
        }, delay);
        mobileInputFocusRestoreTimersRef.current.push(timer);
      }
    };
    const scheduleFocusOutCommit = () => {
      clearPendingFocusOut();
      focusOutTimerRef.current = window.setTimeout(() => {
        focusOutTimerRef.current = undefined;
        commitFocusOutIfStillOutsideHost();
      }, options.focusOutSettleMs);
    };
    const handleFocusIn = (event: FocusEvent) => {
      const passiveInputTargetFocused = options.isPassiveInputTarget?.(event.target) ?? false;
      clearPendingFocusOut();
      if (passiveInputTargetFocused) {
        clearScheduledMobileInputFocusRestore();
      }
      if (!windowActiveRef.current) {
        if (isMobileInputModeEnabled()) {
          windowActiveRef.current = true;
          passiveFocusBypassRef.current = false;
          suppressPassiveFocusRef.current = false;
          if (focusedRef.current || passiveInputFocusRef.current) {
            scheduleMobileInputFocusRestore();
          }
          return;
        }
        passiveFocusBypassRef.current = false;
        passiveInputFocusRef.current = false;
        focusedRef.current = false;
        setFocused(false);
        blurActiveTerminalElement();
        options.queueCursorReport({ immediate: true });
        return;
      }
      const allowPassiveFocusBypass = passiveFocusBypassRef.current;
      if (
        passiveInputFocusRef.current &&
        !focusActivationArmedRef.current &&
        !passiveInputTargetFocused
      ) {
        // 中文注释：helper textarea 已经在 capture 阶段接住了被动焦点后，
        // 原始 host focusin 事件仍可能继续冒泡到这里。此时不能再把这轮焦点
        // 当成 suppression 违规处理，否则会把刚拿到的 IME 输入焦点立刻 blur 掉。
        return;
      }
      if (
        suppressPassiveFocusRef.current &&
        !focusActivationArmedRef.current &&
        !allowPassiveFocusBypass
      ) {
        if (
          terminalOwnedFocusBeforeWindowBlurRef.current &&
          passiveInputTargetFocused &&
          windowActiveRef.current
        ) {
          // 中文注释：桌面窗口切走再回来时，浏览器可能自动把焦点恢复到原 helper textarea。
          // 只有失焦前终端本来持有焦点时才接受这次恢复，避免普通窗口 focus 抢走外部控件。
          terminalOwnedFocusBeforeWindowBlurRef.current = false;
          passiveInputFocusRef.current = false;
          suppressPassiveFocusRef.current = false;
          options.reportTerminalFocus(true);
          options.resize("focus");
          options.scheduleScrollToBottomIfPinned();
          return;
        }
        terminalOwnedFocusBeforeWindowBlurRef.current = false;
        passiveInputFocusRef.current = false;
        focusedRef.current = false;
        setFocused(false);
        blurActiveTerminalElement();
        options.queueCursorReport({ immediate: true });
        return;
      }
      if (allowPassiveFocusBypass && !focusActivationArmedRef.current) {
        // 中文注释：移动端真实时序里，surface 先 focus 到 host，再由 bridge 同步把
        // 焦点转交给 helper textarea。只有 textarea 真的接住焦点时，才消费这次
        // 临时 bypass；否则会把第二跳 textarea focusin 重新打回 suppression。
        if (passiveInputTargetFocused) {
          passiveFocusBypassRef.current = false;
          passiveInputFocusRef.current = true;
        }
        return;
      }
      passiveFocusBypassRef.current = false;
      passiveInputFocusRef.current = false;
      focusActivationArmedRef.current = false;
      suppressPassiveFocusRef.current = false;
      options.reportTerminalFocus(true);
      // 中文注释：focusin 是终端重新成为 operator 的可靠边界；这里只触发稳定
      // resize 流程，真正写回 daemon 仍由 TerminalPane 的多帧 metrics gate 决定。
      options.resize("focus");
      // 主动点击或程序 focus 回到终端时，只有原本就在底部才继续跟随最新输出；
      // 否则会打断用户查看 scrollback 或搜索结果。
      options.scheduleScrollToBottomIfPinned();
    };
    const handleFocusOut = (event: FocusEvent) => {
      const relatedTarget = event.relatedTarget;
      if (relatedTarget instanceof HTMLElement && options.host.contains(relatedTarget)) {
        // 中文注释：host 与 helper textarea 之间的焦点交接属于同一个终端内部流转。
        // 这时若提前清掉 passive/bypass 状态，下一跳 helper focusin 会被我们自己
        // 再次当成 suppression 违规处理，移动端输入法就接不住了。
        return;
      }
      if (shouldRestoreMobileInputFocus()) {
        // 中文注释：移动端软键盘打开时，helper textarea 可能先被浏览器落到 body/html，
        // 这里必须立刻把输入焦点拉回去，避免键盘因为“看起来失焦”被系统直接收回。
        scheduleMobileInputFocusRestore();
        return;
      }
      if (!focusedRef.current || focusOutTimerRef.current !== undefined) {
        return;
      }
      // 中文注释：移动端软键盘打开/收起时，浏览器可能先给出一次表面上的 focusout，
      // 但 hidden textarea 还在接管输入中。这里先把失焦确认挂起，等 settle 窗口过后
      // 再确认 activeElement 是否真的离开了终端。
      scheduleFocusOutCommit();
    };
    const handleWindowBlur = () => {
      terminalOwnedFocusBeforeWindowBlurRef.current =
        focusedRef.current || focusActivationArmedRef.current || terminalHasDomFocus();
      if (document.visibilityState === "hidden") {
        terminalOwnedFocusBeforeWindowBlurRef.current = false;
        clearPendingFocusOut();
        clearScheduledMobileInputFocusRestore();
        passiveFocusBypassRef.current = false;
        passiveInputFocusRef.current = false;
        focusActivationArmedRef.current = false;
        windowActiveRef.current = false;
        clearMobileInputFocusRescue();
        // 真实浏览器切到另一个窗口后，旧窗口的 textarea 可能仍留着 DOM focus。
        // 这里立即撤销 operator 聚焦态，避免旧窗口继续按自己的布局上报 PTY resize。
        options.reportTerminalFocus(false);
        blurActiveTerminalElement();
        return;
      }
      if (isMobileInputModeEnabled()) {
        if (shouldRestoreMobileInputFocus()) {
          // 中文注释：移动端只要 helper textarea 还应该维持输入态，就不要把 window blur
          // 当成真正失焦。部分浏览器在 window blur 事件栈里会忽略同步 focus，
          // 因此这里短时间重试，等系统键盘/visualViewport 稳定后再把输入 sink 拉回。
          scheduleMobileInputFocusRestore();
          return;
        }
        // 中文注释：移动端 window.blur 很多时候只是软键盘弹出/收起的中间态，
        // 这时不要立刻走桌面的“真失焦”路径，否则 helper textarea 会被我们自己
        // 主动 blur 掉，系统键盘会立刻合上。
        scheduleFocusOutCommit();
        return;
      }
      if (isMobileTransientFocusTransition()) {
        // 中文注释：移动端系统键盘打开时，浏览器可能短暂派发 window blur。
        // 这类 blur 不能直接当成真实失焦，需要和 focusout 一样先做确认。
        scheduleFocusOutCommit();
        return;
      }
      clearPendingFocusOut();
      passiveFocusBypassRef.current = false;
      passiveInputFocusRef.current = false;
      focusActivationArmedRef.current = false;
      windowActiveRef.current = false;
      suppressPassiveFocusRef.current = true;
      clearMobileInputFocusRescue();
      // 这里是桌面窗口切换或真正失焦，必须立即撤销 operator 聚焦态。
      options.reportTerminalFocus(false);
      blurActiveTerminalElement();
    };
    const handleWindowFocus = () => {
      const recoveringFromTransientBlur = isMobileTransientFocusTransition();
      windowActiveRef.current = true;
      clearPendingFocusOut();
      if (recoveringFromTransientBlur) {
        // 中文注释：这是移动端软键盘弹出过程中常见的短暂 window blur/focus 抖动，
        // 不能把它当成真正失焦来重置 passive/helper 状态。
        suppressPassiveFocusRef.current = false;
        if (focusedRef.current || passiveInputFocusRef.current) {
          scheduleMobileInputFocusRestore();
        }
        return;
      }
      passiveFocusBypassRef.current = false;
      passiveInputFocusRef.current = false;
      focusActivationArmedRef.current = false;
      // 回到浏览器窗口不等于用户要接管 PTY；没有显式激活动作时仍保持被动聚焦抑制。
      suppressPassiveFocusRef.current = true;
    };
    const handleVisibilityChange = () => {
      if (document.visibilityState === "hidden") {
        handleWindowBlur();
        return;
      }
      handleWindowFocus();
    };

    options.host.addEventListener("focusin", handleFocusIn);
    options.host.addEventListener("focusout", handleFocusOut);
    window.addEventListener("blur", handleWindowBlur);
    window.addEventListener("focus", handleWindowFocus);
    document.addEventListener("visibilitychange", handleVisibilityChange);

    return () => {
      clearPendingFocusOut();
      clearScheduledMobileInputFocusRestore();
      clearMobileInputFocusRescue();
      window.removeEventListener("blur", handleWindowBlur);
      window.removeEventListener("focus", handleWindowFocus);
      document.removeEventListener("visibilitychange", handleVisibilityChange);
      options.host.removeEventListener("focusin", handleFocusIn);
      options.host.removeEventListener("focusout", handleFocusOut);
    };
  }, [clearMobileInputFocusRescue, clearPendingFocusOut, clearScheduledMobileInputFocusRestore, mobileInputFocusRescueActive]);

  return {
    focused,
    setFocused,
    focusedRef,
    clientSizeRef,
    focusActivationArmedRef,
    passiveFocusBypassRef,
    passiveInputFocusRef,
    suppressPassiveFocusRef,
    windowActiveRef,
    focusOutTimerRef,
    clearPendingFocusOut,
    armMobileInputFocusRescue,
    cancelMobileInputFocusRecovery,
    mobileInputFocusRescueActive,
    installTerminalFocusResizeListeners,
  };
}
