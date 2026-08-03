import { useEffect, useRef } from "react";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { desktopAvailable, startTerminal } from "./bridge";
import { TerminalInputQueue, forwardDroppedInput } from "./terminalInput";

interface TerminalPaneProps {
  tabId: string;
  deviceName: string;
  active: boolean;
  command?: string;
  onStarted: (tabId: string, sessionId: string) => void;
  onActivity: (message: string) => void;
}

export function TerminalPane({ tabId, deviceName, active, command, onStarted, onActivity }: TerminalPaneProps) {
  const host = useRef<HTMLDivElement>(null);
  const terminal = useRef<Terminal | null>(null);
  const fit = useRef<FitAddon | null>(null);
  const visible = useRef(active);
  const syncSize = useRef<() => void>(() => {});

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
    visible.current = active;
    if (!active || !terminal.current || !fit.current) return;
    requestAnimationFrame(() => {
      syncSize.current();
      terminal.current?.focus();
    });
  }, [active]);

  return <div className="terminal-host" ref={host} aria-label={`Interactive terminal for ${deviceName}`} />;
}
