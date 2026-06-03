import { useCallback, useRef, useState } from "react";
import type { TerminalSize } from "../../protocol/types";

interface InstallTerminalFocusResizeListenersOptions {
  host: HTMLElement;
  focusOutSettleMs: number;
  reportTerminalFocus: (focused: boolean) => void;
  queueCursorReport: (options?: { immediate?: boolean }) => void;
  scheduleScrollToBottomIfPinned: () => void;
  resize: (source?: "layout" | "focus" | "session" | "snapshot" | "mobile-viewport") => void;
}

export function useTerminalFocusResizeState() {
  const focusedRef = useRef(false);
  const clientSizeRef = useRef<TerminalSize | undefined>(undefined);
  const mobileViewportResizeOwnerRef = useRef(false);
  const focusActivationArmedRef = useRef(false);
  const suppressPassiveFocusRef = useRef(false);
  const windowActiveRef = useRef(true);
  const focusOutTimerRef = useRef<number | undefined>(undefined);
  const [focused, setFocused] = useState(false);

  const clearPendingFocusOut = useCallback(() => {
    if (focusOutTimerRef.current === undefined) {
      return;
    }
    window.clearTimeout(focusOutTimerRef.current);
    focusOutTimerRef.current = undefined;
  }, []);

  const installTerminalFocusResizeListeners = useCallback((options: InstallTerminalFocusResizeListenersOptions) => {
    const blurActiveTerminalElement = () => {
      const activeElement = document.activeElement;
      if (activeElement instanceof HTMLElement && options.host.contains(activeElement)) {
        activeElement.blur();
      }
    };
    const handleFocusIn = () => {
      clearPendingFocusOut();
      if (!windowActiveRef.current) {
        focusedRef.current = false;
        setFocused(false);
        blurActiveTerminalElement();
        options.queueCursorReport({ immediate: true });
        return;
      }
      if (suppressPassiveFocusRef.current && !focusActivationArmedRef.current) {
        focusedRef.current = false;
        setFocused(false);
        blurActiveTerminalElement();
        options.queueCursorReport({ immediate: true });
        return;
      }
      focusActivationArmedRef.current = false;
      suppressPassiveFocusRef.current = false;
      options.reportTerminalFocus(true);
      // 主动点击或程序 focus 回到终端时，只有原本就在底部才继续跟随最新输出；
      // 否则会打断用户查看 scrollback 或搜索结果。
      options.scheduleScrollToBottomIfPinned();
    };
    const handleFocusOut = () => {
      focusActivationArmedRef.current = false;
      if (!focusedRef.current || focusOutTimerRef.current !== undefined) {
        return;
      }
      // 浏览器窗口 resize、移动端视觉视口变化和 xterm 内部重排都可能短暂触发
      // focusout -> focusin。延迟确认失焦，避免把这种瞬时 DOM 抖动上报成
      // operator 在 focused/blurred 之间来回切换。
      focusOutTimerRef.current = window.setTimeout(() => {
        focusOutTimerRef.current = undefined;
        options.reportTerminalFocus(false);
      }, options.focusOutSettleMs);
    };
    const handleWindowBlur = () => {
      windowActiveRef.current = false;
      focusActivationArmedRef.current = false;
      mobileViewportResizeOwnerRef.current = false;
      suppressPassiveFocusRef.current = true;
      clearPendingFocusOut();
      // 真实浏览器切到另一个窗口后，旧窗口的 textarea 可能仍留着 DOM focus。
      // 这里立即撤销 operator 聚焦态，避免旧窗口继续按自己的布局上报 PTY resize。
      options.reportTerminalFocus(false);
      blurActiveTerminalElement();
      options.resize("layout");
    };
    const handleWindowFocus = () => {
      windowActiveRef.current = true;
      focusActivationArmedRef.current = false;
      // 回到浏览器窗口不等于用户要接管 PTY；仍需点击终端区域重新激活。
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
      window.removeEventListener("blur", handleWindowBlur);
      window.removeEventListener("focus", handleWindowFocus);
      document.removeEventListener("visibilitychange", handleVisibilityChange);
      options.host.removeEventListener("focusin", handleFocusIn);
      options.host.removeEventListener("focusout", handleFocusOut);
    };
  }, [clearPendingFocusOut]);

  return {
    focused,
    setFocused,
    focusedRef,
    clientSizeRef,
    mobileViewportResizeOwnerRef,
    focusActivationArmedRef,
    suppressPassiveFocusRef,
    windowActiveRef,
    focusOutTimerRef,
    clearPendingFocusOut,
    installTerminalFocusResizeListeners,
  };
}
