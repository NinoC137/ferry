import { useEffect, useRef } from "react";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { desktopAvailable, startTerminal } from "./bridge";

interface TerminalPaneProps {
  tabId: string;
  deviceName: string;
  active: boolean;
  onStarted: (tabId: string, sessionId: string) => void;
  onActivity: (message: string) => void;
}

export function TerminalPane({ tabId, deviceName, active, onStarted, onActivity }: TerminalPaneProps) {
  const host = useRef<HTMLDivElement>(null);
  const terminal = useRef<Terminal | null>(null);
  const fit = useRef<FitAddon | null>(null);

  useEffect(() => {
    const term = new Terminal({
      cursorBlink: true,
      cursorStyle: "bar",
      convertEol: true,
      fontFamily: "SFMono-Regular, Menlo, Monaco, Consolas, monospace",
      fontSize: 13,
      lineHeight: 1.35,
      scrollback: 8000,
      theme: {
        background: "#0b1115",
        foreground: "#d9e2e8",
        cursor: "#7ee0c2",
        selectionBackground: "#285b61",
        black: "#0b1115",
        brightBlack: "#71818b",
        green: "#7ee0c2",
        brightGreen: "#a4f2d7",
        yellow: "#e8bd67",
        brightYellow: "#f5d58d",
        blue: "#79b8ff",
        brightBlue: "#a8d3ff",
        red: "#ef8a88",
        brightRed: "#ffb4b2",
      },
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
    term.writeln(`\x1b[38;5;151mConnecting to ${deviceName}...\x1b[0m`);

    const encoder = new TextEncoder();
    let socket: WebSocket | undefined;
    let resizeObserver: ResizeObserver | undefined;
    let disposed = false;
    const pendingInput: Uint8Array[] = [];

    const sendResize = () => {
      if (socket?.readyState !== WebSocket.OPEN) return;
      socket.send(JSON.stringify({ t: "resize", rows: term.rows, cols: term.cols }));
    };

    const start = async () => {
      try {
        const started = await startTerminal(deviceName, term.cols, term.rows);
        if (disposed) return;
        onStarted(tabId, started.sessionId);
        term.writeln(`\x1b[38;5;110mConnected via ${started.transport.toUpperCase()}\x1b[0m\r\n`);
        onActivity(`${deviceName}: terminal connected via ${started.transport.toUpperCase()}`);

        if (!desktopAvailable) {
          term.writeln("\x1b[38;5;221mBrowser preview: terminal input is not sent to a device.\x1b[0m");
          term.write("root@rk3588-lab:~# ");
          return;
        }

        socket = new WebSocket(started.wsUrl);
        socket.binaryType = "arraybuffer";
        socket.onopen = () => {
          sendResize();
          for (const bytes of pendingInput.splice(0)) socket?.send(bytes);
          term.focus();
        };
        socket.onmessage = (event) => {
          if (typeof event.data === "string") term.write(event.data);
          else term.write(new Uint8Array(event.data));
        };
        socket.onerror = () => {
          if (!disposed) onActivity(`${deviceName}: terminal WebSocket error`);
        };
        socket.onclose = () => {
          if (!disposed) {
            term.writeln("\r\n\x1b[38;5;203mTerminal connection closed.\x1b[0m");
            onActivity(`${deviceName}: terminal connection closed`);
          }
        };
      } catch (error) {
        term.writeln(`\r\n\x1b[31mUnable to start terminal: ${String(error)}\x1b[0m`);
        onActivity(`${deviceName}: terminal start failed`);
      }
    };
    void start();

    // This is intentionally a direct byte stream. WebSocket preserves frame
    // order, so fast typing, paste, Tab completion, and escape sequences reach
    // the PTY exactly as xterm generated them.
    const dataDisposable = term.onData((data) => {
      if (!desktopAvailable) {
        term.write(data);
        return;
      }
      const bytes = encoder.encode(data);
      if (socket?.readyState === WebSocket.OPEN) socket.send(bytes);
      else pendingInput.push(bytes);
    });

    if (host.current) {
      resizeObserver = new ResizeObserver(() => {
        fitAddon.fit();
        sendResize();
      });
      resizeObserver.observe(host.current);
    }

    return () => {
      disposed = true;
      dataDisposable.dispose();
      resizeObserver?.disconnect();
      socket?.close();
      term.dispose();
      terminal.current = null;
      fit.current = null;
    };
  }, [deviceName, onActivity, onStarted, tabId]);

  useEffect(() => {
    if (!active || !terminal.current || !fit.current) return;
    requestAnimationFrame(() => {
      fit.current?.fit();
      terminal.current?.focus();
    });
  }, [active]);

  return <div className="terminal-host" ref={host} aria-label={`Interactive terminal for ${deviceName}`} />;
}
