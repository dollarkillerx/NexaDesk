// The page's text in the app's three languages, chosen from the browser's languages.

const STRINGS = {
  "zh-Hans": {
    tagline: "浏览器",
    title: "在浏览器中控制电脑",
    intro: "输入对方电脑上 NexaDesk 显示的一次性码。对方确认后，你就能看到并操作那台电脑，不需要安装任何软件。",
    codeLabel: "一次性码",
    codeHelp: "在对方 NexaDesk 窗口“允许别人控制本机”下方，8 位字母和数字。",
    connect: "连接",
    connecting: "正在连接…",
    verifying: "正在核对一次性码…",
    waiting: "等待对方在电脑上确认…",
    security: "一次性码只在两端核对，不会以明文发出。只在你信任的网络中使用，例如同一局域网或 Tailscale。",
    disconnect: "断开",
    fullscreen: "全屏",
    viewOnly: "仅查看",
    control: "可以控制",
    keysHint: "点击画面后，键盘和鼠标会发往对方。部分系统快捷键（如 Cmd+Tab）无法从浏览器发出。",
    ended: "连接已结束",
    again: "重新连接",
    "error.code": "一次性码不正确，请核对后重试。",
    "error.attempts": "尝试次数过多。请让对方换一个新的一次性码。",
    "error.expired": "这个一次性码已经用过，请让对方提供新的代码。",
    "error.busy": "已有另一个浏览器连着这台电脑。",
    "error.rate": "尝试太频繁，请稍后再试。",
    "error.denied": "对方拒绝了这次连接。",
    "error.webrtc": "无法建立连接。请确认两台设备在同一网络或同一 Tailscale 中。",
    "error.protocol": "连接时出错，请重试。",
    "error.screen": "对方电脑当前无法共享屏幕。",
    "error.host": "无法确认对方的身份，连接已断开。网络中可能有人在篡改。",
    "error.network": "与对方的连接断开了。",
    "error.wasm": "这个浏览器无法运行所需的组件（WebAssembly）。",
    "error.format": "一次性码是 8 位字母和数字。",
  },
  en: {
    tagline: "Browser",
    title: "Control a computer from your browser",
    intro: "Enter the one-time code NexaDesk shows on the other computer. Once they accept, you can see and use it, with nothing to install.",
    codeLabel: "One-time code",
    codeHelp: "8 letters and digits, under “Allow remote control” in their NexaDesk window.",
    connect: "Connect",
    connecting: "Connecting…",
    verifying: "Checking the one-time code…",
    waiting: "Waiting for the other person to accept…",
    security: "The code is checked at both ends and never sent in the clear. Use this only on networks you trust, such as your LAN or Tailscale.",
    disconnect: "Disconnect",
    fullscreen: "Full screen",
    viewOnly: "View only",
    control: "Control",
    keysHint: "Click the picture to send your keyboard and mouse. Some system shortcuts (like Cmd+Tab) cannot leave the browser.",
    ended: "The session has ended",
    again: "Connect again",
    "error.code": "That one-time code is not right. Check it and try again.",
    "error.attempts": "Too many attempts. Ask the other person for a new code.",
    "error.expired": "This code has been used. Ask the other person for a new one.",
    "error.busy": "Another browser is already connected to this computer.",
    "error.rate": "Too many tries. Wait a moment and try again.",
    "error.denied": "The other person declined the connection.",
    "error.webrtc": "Could not connect. Make sure both devices are on the same network or Tailscale.",
    "error.protocol": "Something went wrong while connecting. Try again.",
    "error.screen": "The other computer cannot share its screen right now.",
    "error.host": "Could not verify the other computer, so the connection was closed. Someone on the network may be interfering.",
    "error.network": "The connection to the other computer was lost.",
    "error.wasm": "This browser cannot run a required component (WebAssembly).",
    "error.format": "The one-time code is 8 letters and digits.",
  },
  ja: {
    tagline: "ブラウザ",
    title: "ブラウザからコンピュータを操作",
    intro: "相手のコンピュータの NexaDesk に表示されているワンタイムコードを入力してください。相手が許可すると、画面を見て操作できます。インストールは不要です。",
    codeLabel: "ワンタイムコード",
    codeHelp: "相手の NexaDesk ウィンドウの「このMacの操作を許可」の下に表示される 8 文字の英数字です。",
    connect: "接続",
    connecting: "接続しています…",
    verifying: "ワンタイムコードを確認しています…",
    waiting: "相手の許可を待っています…",
    security: "コードは両端で照合され、平文では送信されません。LAN や Tailscale など、信頼できるネットワークでのみ使用してください。",
    disconnect: "切断",
    fullscreen: "フルスクリーン",
    viewOnly: "表示のみ",
    control: "操作可能",
    keysHint: "画面をクリックすると、キーボードとマウスが相手に送られます。一部のシステムショートカット（Cmd+Tab など）はブラウザから送れません。",
    ended: "接続が終了しました",
    again: "再接続",
    "error.code": "ワンタイムコードが正しくありません。確認してもう一度お試しください。",
    "error.attempts": "試行回数が多すぎます。相手に新しいコードを発行してもらってください。",
    "error.expired": "このコードは使用済みです。相手に新しいコードを発行してもらってください。",
    "error.busy": "別のブラウザがすでにこのコンピュータに接続しています。",
    "error.rate": "試行が多すぎます。しばらくしてからもう一度お試しください。",
    "error.denied": "相手が接続を拒否しました。",
    "error.webrtc": "接続できませんでした。両方のデバイスが同じネットワークまたは同じ Tailscale にあるか確認してください。",
    "error.protocol": "接続中にエラーが発生しました。もう一度お試しください。",
    "error.screen": "相手のコンピュータは現在画面を共有できません。",
    "error.host": "相手のコンピュータを確認できなかったため、接続を切断しました。ネットワーク上で改ざんされている可能性があります。",
    "error.network": "相手との接続が切れました。",
    "error.wasm": "このブラウザでは必要なコンポーネント（WebAssembly）を実行できません。",
    "error.format": "ワンタイムコードは 8 文字の英数字です。",
  },
};

function pick() {
  for (const tag of navigator.languages || [navigator.language || "en"]) {
    const l = tag.toLowerCase();
    if (l.startsWith("zh")) return "zh-Hans";
    if (l.startsWith("ja")) return "ja";
    if (l.startsWith("en")) return "en";
  }
  return "en";
}

export const lang = pick();

export function t(key) {
  return STRINGS[lang][key] ?? STRINGS.en[key] ?? key;
}

/** Fills every element carrying data-t="key". */
export function apply(root = document) {
  document.documentElement.lang = lang;
  for (const el of root.querySelectorAll("[data-t]")) el.textContent = t(el.dataset.t);
}
