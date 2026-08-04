import { useEffect, useRef } from "react";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import type { ITheme } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { desktopAvailable, startTerminal } from "./bridge";
import { TerminalInputQueue, forwardDroppedInput } from "./terminalInput";

const DARK_THEME: ITheme = {
  background: "#14110D",
  foreground: "#E7E0D4",
  cursor: "#D97757",
  cursorAccent: "#14110D",
  selectionBackground: "#3A2A20",
  black: "#14110D",
  brightBlack: "#6E6557",
  red: "#DB7368",
  brightRed: "#EC9384",
  green: "#86B89E",
  brightGreen: "#A6D2BC",
  yellow: "#E0A961",
  brightYellow: "#F0C88A",
  blue: "#7FA6C9",
  brightBlue: "#A6C6E0",
  magenta: "#C79AC0",
  brightMagenta: "#DDB6D6",
  cyan: "#83BEB3",
  brightCyan: "#A6D6CC",
  white: "#CDC5B8",
  brightWhite: "#EFE9DD",
};

// Warm-paper counterpart: cream ground, espresso ink, deep-clay cursor, and an
// ANSI ramp darkened so 16-color output stays legible on light.
const LIGHT_THEME: ITheme = {
  background: "#ECE5D6",
  foreground: "#3A3025",
  cursor: "#B4502F",
  cursorAccent: "#ECE5D6",
  selectionBackground: "rgba(180, 80, 47, .20)",
  black: "#3A3025",
  brightBlack: "#7A6F5C",
  red: "#B04A38",
  brightRed: "#C25E4A",
  green: "#4E7D5F",
  brightGreen: "#5C9E7B",
  yellow: "#9A6B24",
  brightYellow: "#B0832E",
  blue: "#3D6E93",
  brightBlue: "#5385A8",
  magenta: "#8A5684",
  brightMagenta: "#A06E9A",
  cyan: "#3C837A",
  brightCyan: "#4E9A90",
  white: "#B7AC96",
  brightWhite: "#EDE7D9",
};

const themeFor = (mode: "dark" | "light"): ITheme => (mode === "light" ? LIGHT_THEME : DARK_THEME);

// Status banners use raw SGR so they read on either ground: fixed 256-palette
// indices on espresso, truecolor equivalents on paper.
const MESSAGE_COLORS = {
  dark: {
    connecting: "\x1b[38;5;180m",
    connected: "\x1b[38;5;151m",
    preview: "\x1b[38;5;222m",
    closed: "\x1b[38;5;173m",
    error: "\x1b[38;5;174m",
  },
  light: {
    connecting: "\x1b[38;2;154;107;36m",
    connected: "\x1b[38;2;62;110;80m",
    preview: "\x1b[38;2;150;104;35m",
    closed: "\x1b[38;2;176;74;56m",
    error: "\x1b[38;2;176;74;56m",
  },
} as const;

interface TerminalPaneProps {
  tabId: string;
  deviceName: string;
  active: boolean;
  command?: string;
  theme: "dark" | "light";
  onStarted: (tabId: string, sessionId: string) => void;
  onActivity: (message: string) => void;
}

export function TerminalPane({ tabId, deviceName, active, command, theme, onStarted, onActivity }: TerminalPaneProps) {
  const host = useRef<HTMLDivElement>(null);
  const terminal = useRef<Terminal | null>(null);
  const fit = useRef<FitAddon | null>(null);
  const visible = useRef(active);
  const syncSize = useRef<() => void>(() => {});
  const themeRef = useRef(theme);
  themeRef.current = theme;

  useEffect(() => {
    const term = new Terminal({
      cursorBlink: true,
      cursorStyle: "bar",
      convertEol: true,
      fontFamily: "SFMono-Regular, Menlo, Monaco, Consolas, monospace",
      fontSize: 13,
      lineHeight: 1.35,
      scrollback: 8000,
      theme: themeFor(themeRef.current),
    });
    const fitAddon = new FitAddon();
    terminal.current = term;
    fit.current = fitAddon;
    term.loadAddon(fitAddon);
    if (host.current) {
      term.open(host.current);
      fitAddon.fit();
    }
    term.focus();
    term.writeln(`${MESSAGE_COLORS[themeRef.current].connecting}Connecting to ${deviceName}...\x1b[0m`);

    const encoder = new TextEncoder();
    let socket: WebSocket | undefined;
    let resizeObserver: ResizeObserver | undefined;
    let disposed = false;
    let terminalReady = false;
    const inputQueue = new TerminalInputQueue(encoder);

    const flushInput = () => {
      const openSocket = socket;
      if (!terminalReady || openSocket?.readyState !== WebSocket.OPEN) return;
      try {
        inputQueue.flush((bytes) => openSocket.send(bytes));
      } catch {
        // OPEN can change between the readyState check and send(). The queue
        // retains the failed item so no key press silently disappears.
      }
    };
    const queueInput = (data: string) => {
      inputQueue.enqueue(data);
      flushInput();
    };

    const sendResize = () => {
      if (!visible.current || socket?.readyState !== WebSocket.OPEN) return;
      socket.send(JSON.stringify({ t: "resize", rows: term.rows, cols: term.cols }));
    };
    const fitVisibleTerminal = () => {
      if (!visible.current) return;
      fitAddon.fit();
      sendResize();
    };
    syncSize.current = fitVisibleTerminal;

    const start = async () => {
      try {
        const started = await startTerminal(deviceName, term.cols, term.rows, command);
        if (disposed) return;
        onStarted(tabId, started.sessionId);
        term.writeln(`${MESSAGE_COLORS[themeRef.current].connected}Connected via ${started.transport.toUpperCase()}\x1b[0m\r\n`);
        onActivity(`${deviceName}: terminal connected via ${started.transport.toUpperCase()}`);

        if (!desktopAvailable) {
          term.writeln(`${MESSAGE_COLORS[themeRef.current].preview}Browser preview: terminal input is not sent to a device.\x1b[0m`);
          term.write("root@rk3588-lab:~# ");
          return;
        }

        socket = new WebSocket(started.wsUrl);
        socket.binaryType = "arraybuffer";
        socket.onopen = () => {
          sendResize();
        };
        socket.onmessage = (event) => {
          if (event.data === '{"t":"terminal-ready"}') {
            terminalReady = true;
            flushInput();
            term.focus();
          } else if (typeof event.data === "string") term.write(event.data);
          else term.write(new Uint8Array(event.data));
        };
        socket.onerror = () => {
          if (!disposed) onActivity(`${deviceName}: terminal WebSocket error`);
        };
        socket.onclose = () => {
          terminalReady = false;
          if (!disposed) {
            term.writeln(`\r\n${MESSAGE_COLORS[themeRef.current].closed}Terminal connection closed.\x1b[0m`);
            onActivity(`${deviceName}: terminal connection closed`);
          }
        };
      } catch (error) {
        term.writeln(`\r\n${MESSAGE_COLORS[themeRef.current].error}Unable to start terminal: ${String(error)}\x1b[0m`);
        onActivity(`${deviceName}: terminal start failed`);
      }
    };
    void start();

    // Every xterm input event enters the FIFO before any send is attempted.
    // This covers ordinary keys, paste, completion and terminal control bytes.
    const dataDisposable = term.onData((data) => {
      if (!desktopAvailable) {
        term.write(data);
        return;
      }
      queueInput(data);
    });

    // Safety net for macOS WKWebView: it intermittently routes fast keystrokes
    // through the marked-text/insertText path (keydown keyCode 229), after which
    // xterm 6.x drops the character before onData (see forwardDroppedInput). We
    // claim the earlier `beforeinput` event and feed the byte into the same FIFO;
    // preventDefault stops xterm's own `input` handler from double-sending.
    const textarea = term.textarea;
    const onBeforeInput = (event: InputEvent) => {
      if (!desktopAvailable) return;
      forwardDroppedInput(event, queueInput);
    };
    textarea?.addEventListener("beforeinput", onBeforeInput, true);

    if (host.current) {
      resizeObserver = new ResizeObserver(() => {
        fitVisibleTerminal();
      });
      resizeObserver.observe(host.current);
    }

    return () => {
      disposed = true;
      dataDisposable.dispose();
      textarea?.removeEventListener("beforeinput", onBeforeInput, true);
      resizeObserver?.disconnect();
      socket?.close();
      syncSize.current = () => {};
      term.dispose();
      terminal.current = null;
      fit.current = null;
    };
  }, [command, deviceName, onActivity, onStarted, tabId]);

  useEffect(() => {
    const term = terminal.current;
    if (term) term.options.theme = themeFor(theme);
  }, [theme]);

  useEffect(() => {
    visible.current = active;
    if (!active || !terminal.current || !fit.current) return;
    requestAnimationFrame(() => {
      syncSize.current();
      terminal.current?.focus();
    });
  }, [active]);

  return <div className="terminal-host" ref={host} aria-label={`Interactive terminal for ${deviceName}`} />;
}
