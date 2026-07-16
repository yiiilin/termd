import { createContext, useContext, useMemo, type ReactNode } from "react";
import type { BrowserLanguagePreference, SafeError } from "./protocol/types";

export type Locale = "zh-CN" | "en-US";

const enUS = {
  "app.adminTitle": "Termd admin",
  "app.adminAria": "daemon admin",
  "app.workspace": "Workspace",
  "app.selectedDaemon": "Selected daemon",
  "app.selectedDaemonAria": "selected daemon",
  "app.unpaired": "unpaired",
  "app.openWorkspace": "Open workspace",
  "app.noSession": "No session",
  "app.noDaemon": "No daemon",
  "app.termd": "Termd",
  "app.newSession": "New session",
  "app.clients": "Clients",
  "app.daemons": "Daemons",
  "app.sessions": "Sessions",
  "app.files": "Files",
  "app.new": "New",
  "app.git": "Git",
  "app.settings": "Settings",
  "app.expandSidebar": "Expand sidebar",
  "app.collapseSidebar": "Collapse sidebar",
  "app.collapsedSessions": "collapsed sessions",
  "app.mobileWorkspaceMenu": "mobile workspace menu",
  "app.sessionsPanel": "sessions panel",
  "app.closeMobileMenu": "Close mobile workspace menu",
  "app.openMobileMenu": "Open mobile workspace menu",
  "app.openSessionListFromTitle": "Open session list from title",
  "app.showFilesPanel": "Show files panel",
  "app.filesPanelCollapsed": "files panel collapsed",
  "app.terminalUnavailable": "terminal unavailable",
  "app.disconnected": "disconnected",
  "app.connectionReady": "Connected",
  "app.connectionChecking": "Checking connection",
  "protocolError.title": "Connection error",
  "protocolError.retry": "Refresh",
  "operators.empty": "no operators",
  "operators.aria": "session operators",
  "operators.client": "Client",
  "operators.cursorUnknown": "cursor ?",
  "operators.focused": "focused",
  "operators.blurred": "blurred",
  "operators.you": "you",
  "daemonStatus.cpu": "CPU",
  "daemonStatus.aria": "daemon server status",
  "daemonStatus.fallbackHost": "daemon",
  "daemonStatus.cpuBars": "CPU usage bars",
  "daemonStatus.memory": "Mem",
  "daemonStatus.disk": "Disk",
  "daemonStatus.network": "Net",
  "daemonStatus.load": "Load",
  "daemonStatus.uptime": "Uptime",

  "settings.title": "Settings",
  "settings.subtitle": "Saved only in this browser.",
  "settings.close": "Close settings",
  "settings.language": "Language",
  "settings.language.auto": "Auto",
  "settings.language.zhCN": "中文",
  "settings.language.enUS": "English",
  "settings.theme": "Theme",
  "settings.theme.system": "System",
  "settings.theme.dark": "Dark",
  "settings.theme.light": "Light",
  "settings.notifications": "Notifications",
  "settings.notifications.off": "Off",
  "settings.notifications.mentions": "Background output",
  "settings.notifications.all": "Connection + output",
  "settings.mobileShortcuts": "Mobile shortcuts",
  "settings.mobileShortcutsHelp": "One button per line, format: label=escaped text",
  "settings.effective": "Current: {value}",

  "connection.wsUrl": "WS URL",
  "connection.panelAria": "connection",
  "connection.statusAria": "connection status",
  "connection.address": "Address",
  "connection.editAddress": "Edit address",
  "connection.pairingToken": "Pairing token",
  "connection.pair": "Pair",
  "connection.scanQr": "Scan QR",
  "connection.saveUrl": "Save URL",
  "connection.manageDaemons": "Manage daemons",
  "connection.editConnection": "Edit connection",
  "connection.daemon": "Daemon",

  "clients.title": "Clients",
  "clients.panelAria": "daemon clients",
  "clients.empty": "No online clients",
  "clients.thisBrowser": "This browser",
  "clients.webClient": "Web client",
  "clients.online": "online",
  "clients.viewing": "Viewing",
  "clients.viewingSessions": "Viewing {sessions}",
  "clients.notViewingSession": "Not viewing a session",
  "clients.offline": "offline",
  "clients.attached": "attached",
  "clients.detached": "detached",
  "clients.attachedSessions": "attached {count} sessions",
  "clients.deleteOffline": "Delete offline client {label}",

  "daemons.title": "Daemons",
  "daemons.managerAria": "daemon manager",
  "daemons.empty": "No daemons",
  "daemons.name": "Daemon name",
  "daemons.saveName": "Save daemon name",
  "daemons.cancelRename": "Cancel daemon rename",
  "daemons.use": "Use",
  "daemons.active": "Active",
  "daemons.useDaemon": "Use daemon {label}",
  "daemons.renameDaemon": "Rename daemon {label}",
  "daemons.deleteDaemon": "Delete daemon {label}",
  "daemons.fallbackName": "Daemon {index}",
  "daemons.fallbackHostName": "Daemon {index} {host}",

  "sessions.aria": "sessions",
  "sessions.empty": "No sessions",
  "sessions.creating": "Creating session",
  "sessions.open": "Open {name}",
  "sessions.openNewOutput": "Open {name}, new output",
  "sessions.select": "Select {name}",
  "sessions.selectNewOutput": "Select {name}, new output",
  "sessions.drag": "Drag {name}",
  "sessions.name": "Session name",
  "sessions.actions": "Session actions",
  "sessions.saveName": "Save session name",
  "sessions.cancelRename": "Cancel rename",
  "sessions.rename": "Rename session",
  "sessions.close": "Close session",
  "sessions.closePanel": "Close sessions panel",
  "sessions.refresh": "Refresh sessions",
  "sessions.activity.agent.codex": "Codex",
  "sessions.activity.agent.claude_code": "Claude Code",
  "sessions.activity.agent.opencode": "OpenCode",
  "sessions.activity.agent.zcode": "ZCode",
  "sessions.activity.idle": "{agent} is ready",
  "sessions.activity.running": "{agent} is running",
  "sessions.activity.attention": "{agent} needs attention",
  "sessions.activity.completed": "{agent} finished",
  "sessions.activity.notification": "{name}: {status}",

  "files.panelAria": "session files",
  "files.resizePanel": "Resize files panel",
  "files.panelView": "Files panel view",
  "files.hidePanel": "Hide files panel",
  "files.parentDirectory": "Parent directory",
  "files.currentDirectory": "Current directory",
  "files.go": "Go",
  "files.refresh": "Refresh files",
  "files.upload": "Upload",
  "files.uploadFile": "Upload file",
  "files.detached": "detached",
  "files.loading": "loading",
  "files.unavailable": "unavailable",
  "files.emptyDirectory": "empty directory",
  "files.followCwd": "Follow terminal cwd",
  "files.uploadProgress": "Uploading {name}",
  "files.uploadCommitting": "Saving {name}",
  "files.downloadProgress": "Downloading {name}",
  "files.open": "Open {name}",
  "files.edit": "Edit {name}",
  "files.download": "Download {name}",
  "files.delete": "Delete {name}",
  "files.rename": "Rename {name}",

  "git.status": "Git status",
  "git.graph": "Git graph",
  "git.changes": "Changes",
  "git.repository": "Repository",
  "git.refresh": "Refresh Git",
  "git.changesTree": "Git changes tree",
  "git.expandChanges": "Expand Git changes",
  "git.collapseChanges": "Collapse Git changes",
  "git.expandGraph": "Expand Git graph",
  "git.collapseGraph": "Collapse Git graph",
  "git.resizeGraph": "Resize Git graph",
  "git.cleanRepository": "clean repository",
  "git.noCommits": "no commits",
  "git.detached": "detached",
  "git.cwd": "cwd",
  "git.worktreeChanges": "{label} changes",
  "git.expandWorktree": "Expand {label} worktree",
  "git.collapseWorktree": "Collapse {label} worktree",
  "git.staged": "Staged",
  "git.unstaged": "Unstaged",
  "git.noStaged": "no staged changes",
  "git.noUnstaged": "no unstaged changes",
  "git.openFile": "Open {path}",
  "git.stageFile": "Stage {path}",
  "git.unstageFile": "Unstage {path}",
  "git.discardFile": "Discard {path}",
  "git.diffFile": "Diff {path}",
  "git.commits": "Git graph commits",

  "status.attached": "attached",
  "status.detached": "detached",
  "connectionState.idle": "idle",
  "connectionState.connecting": "connecting",
  "connectionState.pairing": "pairing",
  "connectionState.ready": "ready",
  "connectionState.savingUrl": "saving URL",
  "connectionState.listing": "listing",
  "connectionState.attaching": "attaching",
  "connectionState.attached": "attached",
  "connectionState.creating": "creating",
  "connectionState.error": "error",
  "error.protocolOperationFailed": "protocol operation failed",
  "error.missingPairing": "device is not paired",
  "error.pairingServerUnknown": "pairing requires a known daemon server id",
  "error.pairingServerMismatch": "pairing payload does not match the connected daemon",
  "error.emptyPairingCandidates": "no pairing URL candidates",
  "error.connectionClosed": "connection closed",
  "error.connectionError": "connection error",
  "error.operationTimedOut": "operation timed out",
  "error.unexpectedMessage": "unexpected protocol message",
  "error.invalidHandshake": "daemon handshake was incomplete",
  "error.fileUploadTooLarge": "file is too large to upload in browser",
  "error.fileEditTooLarge": "file is too large to edit in browser",
  "error.fileDownloadTooLarge": "browser streaming download is unavailable for this file",
  "error.binaryFile": "binary files cannot be edited in browser",
  "error.invalidFileChunk": "file chunk did not advance",
  "error.invalidFileData": "invalid file data",
  "error.fileReadFailed": "file read failed",
  "error.downloadCancelled": "download was cancelled",

  "terminal.mobileShortcuts": "mobile terminal shortcuts",
  "terminal.mobileDirection": "mobile direction gesture",
  "terminal.copySelection": "Copy selection",
  "terminal.copied": "Copied",
  "terminal.sendTab": "Send Tab",
  "terminal.sendEscape": "Send Escape",
  "terminal.sendCtrlC": "Send Ctrl-C",
  "terminal.sendCtrlZ": "Send Ctrl-Z",
  "terminal.sendCtrlD": "Send Ctrl-D",
  "terminal.paste": "Paste",
  "terminal.search": "Search terminal",
  "terminal.searchPlaceholder": "Search scrollback",
  "terminal.previousMatch": "Previous match",
  "terminal.nextMatch": "Next match",
  "terminal.closeSearch": "Close search",
  "terminal.searchFailed": "Search failed",

  "qr.startingCamera": "Starting camera",
  "qr.cameraNotAvailable": "Camera not available",
  "qr.unableToRead": "Unable to read QR code",
  "qr.cameraAccessFailed": "Camera access failed",
  "qr.scannerUnavailable": "Scanner unavailable",
  "qr.scanningHelp": "Scanning. Fill the frame with the QR code.",
  "qr.readingImage": "Reading image",
  "qr.noQrInImage": "No QR code found in image",
  "qr.scanning": "Scanning",
  "qr.scanPairing": "Scan pairing QR",
  "qr.closeScanner": "Close scanner",
  "qr.inviteCode": "Invite code",
  "qr.useInvite": "Use invite",
  "qr.uploadImage": "Upload image",
  "qr.uploadQrImage": "Upload QR image",

  "editor.untitled": "Untitled",
  "editor.loading": "loading",
  "editor.saving": "saving",
  "editor.readOnly": "read only",
  "editor.modified": "modified",
  "editor.saved": "saved",
  "editor.save": "Save",
  "editor.savingButton": "Saving",
  "editor.close": "Close editor",
  "editor.loadingEditor": "loading editor",
  "editor.cancel": "Cancel",
  "editor.lineNumbers": "Line numbers",
  "editor.fileText": "File text",
  "editor.minimap": "Editor minimap",
} as const;

type TranslationKey = keyof typeof enUS;
type TranslationParams = Record<string, string | number | undefined>;

const zhCN: Record<TranslationKey, string> = {
  "app.adminTitle": "Termd 管理",
  "app.adminAria": "守护进程管理",
  "app.workspace": "工作台",
  "app.selectedDaemon": "当前守护进程",
  "app.selectedDaemonAria": "当前守护进程",
  "app.unpaired": "未配对",
  "app.openWorkspace": "打开工作台",
  "app.noSession": "无会话",
  "app.noDaemon": "无守护进程",
  "app.termd": "Termd",
  "app.newSession": "新建会话",
  "app.clients": "客户端",
  "app.daemons": "守护进程",
  "app.sessions": "会话",
  "app.files": "文件",
  "app.new": "新建",
  "app.git": "Git",
  "app.settings": "设置",
  "app.expandSidebar": "展开侧栏",
  "app.collapseSidebar": "折叠侧栏",
  "app.collapsedSessions": "已折叠会话",
  "app.mobileWorkspaceMenu": "移动端工作台菜单",
  "app.sessionsPanel": "会话面板",
  "app.closeMobileMenu": "关闭移动端工作台菜单",
  "app.openMobileMenu": "打开移动端工作台菜单",
  "app.openSessionListFromTitle": "从标题打开会话列表",
  "app.showFilesPanel": "显示文件面板",
  "app.filesPanelCollapsed": "文件面板已折叠",
  "app.terminalUnavailable": "终端不可用",
  "app.disconnected": "已断开",
  "app.connectionReady": "已连接",
  "app.connectionChecking": "检查连接中",
  "protocolError.title": "连接错误",
  "protocolError.retry": "刷新",
  "operators.empty": "没有操作者",
  "operators.aria": "会话操作者",
  "operators.client": "客户端",
  "operators.cursorUnknown": "光标 ?",
  "operators.focused": "聚焦",
  "operators.blurred": "失焦",
  "operators.you": "你",
  "daemonStatus.cpu": "CPU",
  "daemonStatus.aria": "守护进程服务状态",
  "daemonStatus.fallbackHost": "守护进程",
  "daemonStatus.cpuBars": "CPU 使用率柱状图",
  "daemonStatus.memory": "内存",
  "daemonStatus.disk": "磁盘",
  "daemonStatus.network": "网络",
  "daemonStatus.load": "负载",
  "daemonStatus.uptime": "运行",

  "settings.title": "设置",
  "settings.subtitle": "只保存在当前浏览器。",
  "settings.close": "关闭设置",
  "settings.language": "语言",
  "settings.language.auto": "自动",
  "settings.language.zhCN": "中文",
  "settings.language.enUS": "English",
  "settings.theme": "主题",
  "settings.theme.system": "跟随系统",
  "settings.theme.dark": "深色",
  "settings.theme.light": "浅色",
  "settings.notifications": "通知",
  "settings.notifications.off": "关闭",
  "settings.notifications.mentions": "后台输出",
  "settings.notifications.all": "连接和输出",
  "settings.mobileShortcuts": "移动端快捷键",
  "settings.mobileShortcutsHelp": "每行一个按钮，格式：标签=转义文本",
  "settings.effective": "当前：{value}",

  "connection.wsUrl": "WS URL",
  "connection.panelAria": "连接",
  "connection.statusAria": "连接状态",
  "connection.address": "地址",
  "connection.editAddress": "编辑地址",
  "connection.pairingToken": "配对 token",
  "connection.pair": "配对",
  "connection.scanQr": "扫描二维码",
  "connection.saveUrl": "保存 URL",
  "connection.manageDaemons": "管理守护进程",
  "connection.editConnection": "编辑连接",
  "connection.daemon": "守护进程",

  "clients.title": "客户端",
  "clients.panelAria": "守护进程客户端",
  "clients.empty": "没有在线客户端",
  "clients.thisBrowser": "当前浏览器",
  "clients.webClient": "Web 客户端",
  "clients.online": "在线",
  "clients.viewing": "正在查看",
  "clients.viewingSessions": "正在查看 {sessions}",
  "clients.notViewingSession": "未查看会话",
  "clients.offline": "离线",
  "clients.attached": "已接入",
  "clients.detached": "未接入",
  "clients.attachedSessions": "已接入 {count} 个会话",
  "clients.deleteOffline": "删除离线客户端 {label}",

  "daemons.title": "守护进程",
  "daemons.managerAria": "守护进程管理器",
  "daemons.empty": "没有守护进程",
  "daemons.name": "守护进程名称",
  "daemons.saveName": "保存守护进程名称",
  "daemons.cancelRename": "取消重命名守护进程",
  "daemons.use": "使用",
  "daemons.active": "当前",
  "daemons.useDaemon": "使用守护进程 {label}",
  "daemons.renameDaemon": "重命名守护进程 {label}",
  "daemons.deleteDaemon": "删除守护进程 {label}",
  "daemons.fallbackName": "守护进程 {index}",
  "daemons.fallbackHostName": "守护进程 {index} {host}",

  "sessions.aria": "会话",
  "sessions.empty": "没有会话",
  "sessions.creating": "正在创建会话",
  "sessions.open": "打开 {name}",
  "sessions.openNewOutput": "打开 {name}，有新输出",
  "sessions.select": "选择 {name}",
  "sessions.selectNewOutput": "选择 {name}，有新输出",
  "sessions.drag": "拖动 {name}",
  "sessions.name": "会话名称",
  "sessions.actions": "会话操作",
  "sessions.saveName": "保存会话名称",
  "sessions.cancelRename": "取消重命名",
  "sessions.rename": "重命名会话",
  "sessions.close": "关闭会话",
  "sessions.closePanel": "关闭会话面板",
  "sessions.refresh": "刷新会话",
  "sessions.activity.agent.codex": "Codex",
  "sessions.activity.agent.claude_code": "Claude Code",
  "sessions.activity.agent.opencode": "OpenCode",
  "sessions.activity.agent.zcode": "ZCode",
  "sessions.activity.idle": "{agent} 已就绪",
  "sessions.activity.running": "{agent} 正在运行",
  "sessions.activity.attention": "{agent} 需要操作",
  "sessions.activity.completed": "{agent} 已完成",
  "sessions.activity.notification": "{name}：{status}",

  "files.panelAria": "会话文件",
  "files.resizePanel": "调整文件面板宽度",
  "files.panelView": "文件面板视图",
  "files.hidePanel": "隐藏文件面板",
  "files.parentDirectory": "上级目录",
  "files.currentDirectory": "当前目录",
  "files.go": "跳转",
  "files.refresh": "刷新文件",
  "files.upload": "上传",
  "files.uploadFile": "上传文件",
  "files.detached": "未接入",
  "files.loading": "加载中",
  "files.unavailable": "不可用",
  "files.emptyDirectory": "空目录",
  "files.followCwd": "跟随终端当前目录",
  "files.uploadProgress": "正在上传 {name}",
  "files.uploadCommitting": "正在保存 {name}",
  "files.downloadProgress": "正在下载 {name}",
  "files.open": "打开 {name}",
  "files.edit": "编辑 {name}",
  "files.download": "下载 {name}",
  "files.delete": "删除 {name}",
  "files.rename": "重命名 {name}",

  "git.status": "Git 状态",
  "git.graph": "Git 图",
  "git.changes": "变更",
  "git.repository": "仓库",
  "git.refresh": "刷新 Git",
  "git.changesTree": "Git 变更树",
  "git.expandChanges": "展开 Git 变更",
  "git.collapseChanges": "折叠 Git 变更",
  "git.expandGraph": "展开 Git 图",
  "git.collapseGraph": "折叠 Git 图",
  "git.resizeGraph": "调整 Git 图高度",
  "git.cleanRepository": "仓库干净",
  "git.noCommits": "没有提交",
  "git.detached": "游离 HEAD",
  "git.cwd": "当前目录",
  "git.worktreeChanges": "{label} 变更",
  "git.expandWorktree": "展开 {label} 工作树",
  "git.collapseWorktree": "折叠 {label} 工作树",
  "git.staged": "已暂存",
  "git.unstaged": "未暂存",
  "git.noStaged": "没有已暂存变更",
  "git.noUnstaged": "没有未暂存变更",
  "git.openFile": "打开 {path}",
  "git.stageFile": "暂存 {path}",
  "git.unstageFile": "取消暂存 {path}",
  "git.discardFile": "撤销 {path}",
  "git.diffFile": "查看差异 {path}",
  "git.commits": "Git 提交图",

  "status.attached": "已接入",
  "status.detached": "未接入",
  "connectionState.idle": "空闲",
  "connectionState.connecting": "连接中",
  "connectionState.pairing": "配对中",
  "connectionState.ready": "就绪",
  "connectionState.savingUrl": "保存 URL 中",
  "connectionState.listing": "读取列表中",
  "connectionState.attaching": "接入中",
  "connectionState.attached": "已接入",
  "connectionState.creating": "创建中",
  "connectionState.error": "错误",
  "error.protocolOperationFailed": "协议操作失败",
  "error.missingPairing": "设备尚未配对",
  "error.pairingServerUnknown": "配对需要已知的守护进程 server id",
  "error.pairingServerMismatch": "配对内容与当前连接的守护进程不匹配",
  "error.emptyPairingCandidates": "没有可用的配对连接地址",
  "error.connectionClosed": "连接已关闭",
  "error.connectionError": "连接错误",
  "error.operationTimedOut": "操作超时",
  "error.unexpectedMessage": "收到非预期的协议消息",
  "error.invalidHandshake": "守护进程握手未完成",
  "error.fileUploadTooLarge": "文件过大，无法在浏览器中上传",
  "error.fileEditTooLarge": "文件过大，无法在浏览器中编辑",
  "error.fileDownloadTooLarge": "当前浏览器无法下载这个大文件",
  "error.binaryFile": "二进制文件不能在浏览器中编辑",
  "error.invalidFileChunk": "文件分块没有推进，已停止读取",
  "error.invalidFileData": "文件数据无效",
  "error.fileReadFailed": "文件读取失败",
  "error.downloadCancelled": "下载已取消",

  "terminal.mobileShortcuts": "移动端终端快捷键",
  "terminal.mobileDirection": "移动端方向手势",
  "terminal.copySelection": "复制选区",
  "terminal.copied": "复制成功",
  "terminal.sendTab": "发送 Tab",
  "terminal.sendEscape": "发送 Escape",
  "terminal.sendCtrlC": "发送 Ctrl-C",
  "terminal.sendCtrlZ": "发送 Ctrl-Z",
  "terminal.sendCtrlD": "发送 Ctrl-D",
  "terminal.paste": "粘贴",
  "terminal.search": "搜索终端",
  "terminal.searchPlaceholder": "搜索滚动历史",
  "terminal.previousMatch": "上一个匹配",
  "terminal.nextMatch": "下一个匹配",
  "terminal.closeSearch": "关闭搜索",
  "terminal.searchFailed": "搜索失败",

  "qr.startingCamera": "正在启动摄像头",
  "qr.cameraNotAvailable": "摄像头不可用",
  "qr.unableToRead": "无法识别二维码",
  "qr.cameraAccessFailed": "摄像头访问失败",
  "qr.scannerUnavailable": "扫描器不可用",
  "qr.scanningHelp": "扫描中。请让二维码填满画面。",
  "qr.readingImage": "正在读取图片",
  "qr.noQrInImage": "图片中没有找到二维码",
  "qr.scanning": "扫描中",
  "qr.scanPairing": "扫描配对二维码",
  "qr.closeScanner": "关闭扫描器",
  "qr.inviteCode": "邀请码",
  "qr.useInvite": "使用邀请码",
  "qr.uploadImage": "上传图片",
  "qr.uploadQrImage": "上传二维码图片",

  "editor.untitled": "未命名",
  "editor.loading": "加载中",
  "editor.saving": "保存中",
  "editor.readOnly": "只读",
  "editor.modified": "已修改",
  "editor.saved": "已保存",
  "editor.save": "保存",
  "editor.savingButton": "保存中",
  "editor.close": "关闭编辑器",
  "editor.loadingEditor": "正在加载编辑器",
  "editor.cancel": "取消",
  "editor.lineNumbers": "行号",
  "editor.fileText": "文件文本",
  "editor.minimap": "编辑器 minimap",
};

const dictionaries: Record<Locale, Record<TranslationKey, string>> = {
  "en-US": enUS,
  "zh-CN": zhCN,
};

export type Translate = (key: TranslationKey, params?: TranslationParams) => string;

export interface I18nContextValue {
  locale: Locale;
  t: Translate;
}

const fallbackContext: I18nContextValue = {
  locale: "en-US",
  t: createTranslator("en-US"),
};

const I18nContext = createContext<I18nContextValue>(fallbackContext);

export function I18nProvider({ locale, children }: { locale: Locale; children: ReactNode }) {
  const value = useMemo<I18nContextValue>(
    () => ({
      locale,
      t: createTranslator(locale),
    }),
    [locale],
  );

  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>;
}

export function useI18n(): I18nContextValue {
  return useContext(I18nContext);
}

export function createTranslator(locale: Locale): Translate {
  const dictionary = dictionaries[locale];
  return (key, params) => interpolate(dictionary[key] ?? enUS[key], params);
}

export function translateSafeErrorMessage(error: SafeError, t: Translate): string {
  switch (error.code) {
    case "missing_pairing":
      return t("error.missingPairing");
    case "pairing_server_unknown":
      return t("error.pairingServerUnknown");
    case "pairing_payload_server_mismatch":
    case "route_server_mismatch":
      return t("error.pairingServerMismatch");
    case "empty_pairing_candidates":
      return t("error.emptyPairingCandidates");
    case "connection_closed":
      return t("error.connectionClosed");
    case "connection_error":
      return t("error.connectionError");
    case "unexpected_message":
      return t("error.unexpectedMessage");
    case "invalid_handshake":
      return t("error.invalidHandshake");
    case "binary_file":
      return t("error.binaryFile");
    case "invalid_file_chunk":
      return t("error.invalidFileChunk");
    case "download_cancelled":
      return t("error.downloadCancelled");
    case "client_error":
      if (error.message === "invalid_file_data") {
        return t("error.invalidFileData");
      }
      if (error.message === "file_read_failed") {
        return t("error.fileReadFailed");
      }
      break;
    case "file_too_large":
      if (error.message.includes("upload")) {
        return t("error.fileUploadTooLarge");
      }
      if (error.message.includes("download") || error.message.includes("streaming")) {
        return t("error.fileDownloadTooLarge");
      }
      return t("error.fileEditTooLarge");
    default:
      break;
  }

  if (error.code.endsWith("_timeout") || error.message === "operation timed out") {
    return t("error.operationTimedOut");
  }
  if (error.message === "protocol operation failed") {
    return t("error.protocolOperationFailed");
  }
  return error.message;
}

export function resolveLocale(preference: BrowserLanguagePreference, languages?: readonly string[]): Locale {
  if (preference === "zh-CN" || preference === "en-US") {
    return preference;
  }
  const candidates =
    languages ??
    (typeof navigator === "undefined"
      ? []
      : navigator.languages?.length
        ? navigator.languages
        : [navigator.language]);
  return candidates.some((language) => language.toLowerCase().startsWith("zh")) ? "zh-CN" : "en-US";
}

function interpolate(template: string, params: TranslationParams = {}): string {
  return template.replace(/\{([a-zA-Z0-9_]+)\}/g, (match, name: string) => String(params[name] ?? match));
}
