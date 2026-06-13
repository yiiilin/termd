import { useCallback, useRef, useState } from "react";
import type { TerminalSize } from "../../protocol/types";

interface InstallTerminalFocusResizeListenersOptions {
  host: HTMLElement;
  focusOutSettleMs: number;
  isPassiveInputTarget?: (target: EventTarget | null) => boolean;
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
  const passiveFocusBypassRef = useRef(false);
  const passiveInputFocusRef = useRef(false);
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
    const handleFocusIn = (event: FocusEvent) => {
      clearPendingFocusOut();
      if (!windowActiveRef.current) {
        passiveFocusBypassRef.current = false;
        passiveInputFocusRef.current = false;
        focusedRef.current = false;
        setFocused(false);
        blurActiveTerminalElement();
        options.queueCursorReport({ immediate: true });
        return;
      }
      const allowPassiveFocusBypass = passiveFocusBypassRef.current;
      const passiveInputTargetFocused = options.isPassiveInputTarget?.(event.target) ?? false;
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
      passiveFocusBypassRef.current = false;
      passiveInputFocusRef.current = false;
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
      passiveFocusBypassRef.current = false;
      passiveInputFocusRef.current = false;
      focusActivationArmedRef.current = false;
      mobileViewportResizeOwnerRef.current = false;
      suppressPassiveFocusRef.current = true;
      clearPendingFocusOut();
      // 真实浏览器切到另一个窗口后，旧窗口的 textarea 可能仍留着 DOM focus。
      // 这里立即撤销 operator 聚焦态，避免旧窗口继续按自己的布局上报 PTY resize。
      options.reportTerminalFocus(false);
      blurActiveTerminalElement();
    };
    const handleWindowFocus = () => {
      windowActiveRef.current = true;
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
    passiveFocusBypassRef,
    passiveInputFocusRef,
    suppressPassiveFocusRef,
    windowActiveRef,
    focusOutTimerRef,
    clearPendingFocusOut,
    installTerminalFocusResizeListeners,
  };
}
