import { useCallback, useEffect, useMemo, useState } from "react";
import { createRoot } from "react-dom/client";
import {
  Activity,
  Bot,
  CircleAlert,
  CircleCheck,
  CircleDotDashed,
  CircleX,
  Clipboard,
  FileKey2,
  Gauge,
  LoaderCircle,
  LayoutDashboard,
  KeyRound,
  MonitorCog,
  Network,
  PackagePlus,
  Plus,
  RefreshCcw,
  Radar,
  Save,
  Server,
  Settings2,
  TerminalSquare,
  Usb,
  X,
} from "lucide-react";
import { checkConnection, discoverLocalDevices, getDevice, listDevices, saveDevice, setupSshKey } from "./bridge";
import { OperationsPanel } from "./OperationsPanel";
import { OverviewPanel } from "./OverviewPanel";
import { PluginsPanel } from "./PluginsPanel";
import { ScanPanel } from "./ScanPanel";
import { TerminalPane } from "./TerminalPane";
import type { DeviceCandidate, DeviceForm, DeviceSummary } from "./types";
import { newDevice } from "./types";
import "./styles.css";

interface TerminalTab {
  id: string;
  deviceName: string;
  sessionId: string;
  title?: string;
  command?: string;
}

const iconForTransport = (transport: string) => {
  if (transport === "serial") return Usb;
  if (transport === "adb") return Bot;
  return Server;
};

function App() {
  const [devices, setDevices] = useState<DeviceSummary[]>([]);
  const [selected, setSelected] = useState<string>("");
  const [form, setForm] = useState<DeviceForm>(newDevice());
  const [tabs, setTabs] = useState<TerminalTab[]>([]);
  const [activeTab, setActiveTab] = useState<string>("");
  const [activity, setActivity] = useState<string[]>(["Desktop workspace ready."]);
  const [busy, setBusy] = useState(false);
  const [connectionMessage, setConnectionMessage] = useState("");
  const [view, setView] = useState<"terminal" | "operations" | "overview" | "scan" | "plugins">("overview");
  const [creating, setCreating] = useState(false);
  const [candidates, setCandidates] = useState<DeviceCandidate[]>([]);
  const [keyPassword, setKeyPassword] = useState("");
  const [keyBusy, setKeyBusy] = useState(false);
  const [keyConfirmOpen, setKeyConfirmOpen] = useState(false);

  const addActivity = useCallback((message: string) => {
    setActivity((entries) => [`${new Date().toLocaleTimeString()}  ${message}`, ...entries].slice(0, 30));
  }, []);

  const refreshDevices = useCallback(async () => {
    setBusy(true);
    try {
      const result = await listDevices();
      setDevices(result);
      if (!creating) setSelected((current) => current || result[0]?.name || "");
    } catch (error) {
      addActivity(`Unable to load devices: ${String(error)}`);
    } finally {
      setBusy(false);
    }
  }, [addActivity, creating]);

  useEffect(() => {
    void refreshDevices();
  }, [refreshDevices]);

  useEffect(() => {
    if (!selected || creating) return;
    void getDevice(selected)
      .then((device) => setForm(device))
      .catch((error) => addActivity(`Unable to load ${selected}: ${String(error)}`));
  }, [addActivity, creating, selected]);

  const selectedDevice = useMemo(() => devices.find((device) => device.name === selected), [devices, selected]);

  const setField = <K extends keyof DeviceForm>(field: K, value: DeviceForm[K]) => {
    setForm((current) => ({ ...current, [field]: value }));
  };

  const selectDevice = (name: string) => {
    setCreating(false);
    setSelected(name);
    setConnectionMessage("");
  };

  const beginNewDevice = (seed = newDevice()) => {
    setCreating(true);
    setSelected("");
    setForm(seed);
    setCandidates([]);
    setConnectionMessage("Draft profile: choose a transport and save it.");
  };

  const discover = async () => {
    try {
      const found = await discoverLocalDevices();
      setCandidates(found);
      setConnectionMessage(found.length ? `${found.length} local ADB/serial candidate(s) found.` : "No local ADB or serial candidates found.");
    } catch (error) {
      setConnectionMessage(`Discovery failed: ${String(error)}`);
    }
  };

  const useCandidate = (candidate: DeviceCandidate) => {
    const draft = newDevice();
    draft.transport = candidate.transport;
    if (candidate.transport === "adb") draft.adbSerial = candidate.value;
    if (candidate.transport === "serial") draft.dev = candidate.value;
    setForm(draft);
    setCreating(true);
  };

  const save = async () => {
    try {
      const saved = await saveDevice(form);
      setForm(saved);
      setCreating(false);
      setSelected(saved.name);
      addActivity(`${saved.name}: profile saved`);
      await refreshDevices();
    } catch (error) {
      addActivity(`Save failed: ${String(error)}`);
    }
  };

  const testConnection = async () => {
    if (!selected) return;
    setConnectionMessage("Checking connection...");
    try {
      const result = await checkConnection(selected);
      setConnectionMessage(result.detail);
      addActivity(`${selected}: ${result.detail}`);
      await refreshDevices();
    } catch (error) {
      setConnectionMessage(String(error));
      addActivity(`${selected}: connection test failed`);
    }
  };

  const installPublicKey = async () => {
    if (!selected || form.transport !== "ssh") return;
    setKeyConfirmOpen(false);
    setKeyBusy(true);
    try {
      const result = await setupSshKey(selected, keyPassword);
      setKeyPassword("");
      setConnectionMessage(result.detail);
      addActivity(`${selected}: passwordless SSH verified`);
      await refreshDevices();
    } catch (error) {
      setConnectionMessage(`Public-key setup failed: ${String(error)}`);
      addActivity(`${selected}: public-key setup failed`);
    } finally {
      setKeyBusy(false);
    }
  };

  const openTerminal = () => {
    if (!selected) return;
    const id = `pending-${Date.now()}`;
    setTabs((current) => [...current, { id, deviceName: selected, sessionId: "" }]);
    setActiveTab(id);
    setView("terminal");
  };

  const openTask = (title: string, command: string) => {
    if (!selected) return;
    const id = `task-${Date.now()}`;
    setTabs((current) => [...current, { id, deviceName: selected, sessionId: "", title, command }]);
    setActiveTab(id);
    setView("terminal");
  };

  const started = useCallback((tabId: string, sessionId: string) => {
    setTabs((current) => current.map((tab) => (tab.id === tabId ? { ...tab, sessionId } : tab)));
  }, []);

  const closeTab = (id: string) => {
    setTabs((current) => {
      const next = current.filter((tab) => tab.id !== id);
      if (activeTab === id) setActiveTab(next.at(-1)?.id ?? "");
      return next;
    });
  };

  return (
    <main className="app-shell">
      <aside className="sidebar">
        <header className="brand-row">
          <button className="brand-mark" title="Open fleet overview" onClick={() => setView("overview")}><Network size={19} /></button>
          <div>
            <strong>Ferry</strong>
            <span>DEVICE WORKBENCH</span>
          </div>
          <button className="icon-button" title="Refresh device status" onClick={() => void refreshDevices()} disabled={busy}>
            <RefreshCcw size={16} className={busy ? "spin" : ""} />
          </button>
        </header>

        <section className="sidebar-section devices-section">
          <div className="section-label"><span>DEVICES</span><button className="icon-button small" title="Add device profile" onClick={() => beginNewDevice()}><Plus size={16} /></button></div>
          <div className="device-list">
            {creating && <button className="device-row selected draft-device" onClick={() => beginNewDevice()}><span className="status-dot" /><Plus size={16} /><span className="device-copy"><strong>New device</strong><small>Unsaved profile</small></span></button>}
            {devices.map((device) => {
              const Icon = iconForTransport(device.transport);
              return <button className={`device-row ${selected === device.name ? "selected" : ""}`} key={device.name} onClick={() => selectDevice(device.name)}>
                <span className={`status-dot ${device.online ? "online" : "offline"}`} />
                <Icon size={16} />
                <span className="device-copy"><strong>{device.name}</strong><small>{device.hostname || device.endpoint}</small></span>
                {device.blackboxRunning && <span title="Black box recording"><CircleDotDashed size={15} className="amber" /></span>}
              </button>;
            })}
            {!devices.length && <div className="empty-list">No device profiles yet.</div>}
          </div>
        </section>

        <section className="sidebar-section quick-actions">
          <div className="section-label">QUICK ACTIONS</div>
          <div className="action-grid">
            <button onClick={openTerminal} disabled={!selected}><TerminalSquare size={16} />Shell</button>
            <button onClick={testConnection} disabled={!selected}><CircleCheck size={16} />Probe</button>
            <button onClick={() => setView("operations")} disabled={!selected}><Clipboard size={16} />Deploy</button>
            <button onClick={() => setView("operations")} disabled={!selected}><Network size={16} />Share</button>
          </div>
        </section>

        <section className="sidebar-section profile-section">
          <div className="section-label"><span>{creating ? "NEW DEVICE" : "DEVICE SETTINGS"}</span><Settings2 size={15} /></div>
          <div className="form-scroll">
            {creating && <><button className="discover-button" onClick={() => void discover()}><RefreshCcw size={14} />Discover local ADB / serial</button>{candidates.map((candidate) => <button className="candidate-row" key={`${candidate.transport}-${candidate.value}`} onClick={() => useCandidate(candidate)}><span>{candidate.transport.toUpperCase()}</span><strong>{candidate.value}</strong><small>{candidate.detail}</small></button>)}</>}
            <label>Name<input value={form.name} onChange={(event) => setField("name", event.target.value)} placeholder="rk3588-lab" /></label>
            <label>Transport<select value={form.transport} onChange={(event) => setField("transport", event.target.value as DeviceForm["transport"])}><option value="ssh">SSH</option><option value="adb">ADB</option><option value="serial">Serial</option></select></label>
            {form.transport === "ssh" && <>
              <div className="split-fields"><label>Host<input value={form.host} onChange={(event) => setField("host", event.target.value)} /></label><label>Port<input type="number" value={form.port} onChange={(event) => setField("port", Number(event.target.value))} /></label></div>
              <label>User<input value={form.user} onChange={(event) => setField("user", event.target.value)} /></label>
              <label>Identity file<input value={form.key} onChange={(event) => setField("key", event.target.value)} placeholder="~/.ssh/id_ed25519" /></label>
              {!creating && <div className="key-setup"><label>Current password (one-time)<input type="password" value={keyPassword} onChange={(event) => setKeyPassword(event.target.value)} placeholder="Only used to install the key" autoComplete="current-password" /></label><button className="key-setup-button" disabled={keyBusy} onClick={() => setKeyConfirmOpen(true)}><KeyRound size={15} />{keyBusy ? "Installing key..." : "Install and verify public key"}</button><p>Never saved by the desktop app. Leave blank only when SSH already accepts a key.</p></div>}
              <label className="toggle-row"><input type="checkbox" checked={form.legacy} onChange={(event) => setField("legacy", event.target.checked)} /><span>Legacy SSH algorithms</span><span title="Only use for isolated legacy boards"><CircleAlert size={14} /></span></label>
            </>}
            {form.transport === "adb" && <label>ADB serial<input value={form.adbSerial} onChange={(event) => setField("adbSerial", event.target.value)} placeholder="USB serial or IP:port" /></label>}
            {form.transport === "serial" && <><label>Serial path<input value={form.dev} onChange={(event) => setField("dev", event.target.value)} placeholder="/dev/cu.usbserial-1420" /></label><label>Baud rate<input type="number" value={form.baud} onChange={(event) => setField("baud", Number(event.target.value))} /></label></>}
            <label>Deploy directory<input value={form.dest} onChange={(event) => setField("dest", event.target.value)} /></label>
            <label>Notes<textarea value={form.notes} onChange={(event) => setField("notes", event.target.value)} rows={2} /></label>
            <button className="save-button" onClick={() => void save()}><Save size={16} />Save profile</button>
          </div>
        </section>
      </aside>

      <section className="workspace">
        <header className="workspace-header">
          <div className="selected-title">
            <span className={`status-dot ${selectedDevice?.online ? "online" : "offline"}`} />
            <div><strong>{selectedDevice?.name || "Select a device"}</strong><span>{selectedDevice?.endpoint || "Create or select a device profile"}</span></div>
          </div>
          <div className="header-tools">
            {connectionMessage && <span className="connection-message">{connectionMessage}</span>}
            <button className={`header-mode ${view === "overview" ? "active" : ""}`} title="Open fleet overview" onClick={() => setView("overview")}><LayoutDashboard size={16} /></button>
            <button className={`header-mode ${view === "scan" ? "active" : ""}`} title="Discover devices" onClick={() => setView("scan")}><Radar size={16} /></button>
            <button className={`header-mode ${view === "operations" ? "active" : ""}`} title="Open operations workbench" onClick={() => setView("operations")}><Clipboard size={16} /></button>
            <button className={`header-mode ${view === "plugins" ? "active" : ""}`} title="Open plugins" onClick={() => setView("plugins")}><PackagePlus size={16} /></button>
            <button className="command-button" onClick={openTerminal} disabled={!selected}><TerminalSquare size={16} />New terminal</button>
          </div>
        </header>

        <div className="tab-strip" role="tablist" aria-label="Terminal sessions">
          {tabs.map((tab) => <button key={tab.id} role="tab" aria-selected={activeTab === tab.id} className={`terminal-tab ${activeTab === tab.id && view === "terminal" ? "active" : ""}`} onClick={() => { setActiveTab(tab.id); setView("terminal"); }}><TerminalSquare size={14} />{tab.title || tab.deviceName}<span className="tab-close" title="Close terminal" onClick={(event) => { event.stopPropagation(); closeTab(tab.id); }}><X size={13} /></span></button>)}
          {!tabs.length && <span className="tab-hint">Interactive sessions</span>}
        </div>

        <div className="workspace-body">
          <div className="terminal-stage">
            <div className={`workspace-layer ${view === "terminal" ? "active" : ""}`}>
              {tabs.map((tab) => <div className={`terminal-panel ${activeTab === tab.id ? "active" : ""}`} key={tab.id}><TerminalPane tabId={tab.id} deviceName={tab.deviceName} command={tab.command} active={activeTab === tab.id && view === "terminal"} onStarted={started} onActivity={addActivity} /></div>)}
              {!tabs.length && <div className="empty-terminal"><MonitorCog size={32} /><h1>Open a device terminal</h1><p>Choose a profile, then start an SSH, ADB, or serial session.</p><button className="command-button" onClick={openTerminal} disabled={!selected}><TerminalSquare size={16} />Start terminal</button></div>}
            </div>
            <div className={`workspace-layer ${view === "overview" ? "active" : ""}`}><OverviewPanel devices={devices} busy={busy} onRefresh={() => void refreshDevices()} onSelect={selectDevice} /></div>
            <div className={`workspace-layer ${view === "scan" ? "active" : ""}`}><ScanPanel onDraft={beginNewDevice} onActivity={addActivity} /></div>
            <div className={`workspace-layer ${view === "operations" ? "active" : ""}`}><OperationsPanel device={selected} active={view === "operations"} onActivity={addActivity} onOpenTask={openTask} /></div>
            <div className={`workspace-layer ${view === "plugins" ? "active" : ""}`}><PluginsPanel device={selected} transport={selectedDevice?.transport ?? ""} hasPassword={selectedDevice?.hasPassword ?? false} active={view === "plugins"} onActivity={addActivity} /></div>
          </div>
          <aside className="activity-panel">
            <div className="activity-title"><Activity size={16} />Activity</div>
            <div className="activity-list">{activity.map((item, index) => <p key={`${item}-${index}`}>{item}</p>)}</div>
            <div className="session-legend"><Gauge size={14} />PTY terminal · xterm renderer</div>
            {selectedDevice?.hasPassword && <div className="security-note"><FileKey2 size={14} />Password profile detected. The terminal prompts interactively; it is not exposed to the UI.</div>}
          </aside>
        </div>
        <footer className="statusbar"><span><span className="status-dot online" /> Desktop ready</span><span>{selectedDevice?.transport?.toUpperCase() ?? "NO DEVICE"}</span><span>{tabs.length} terminal {tabs.length === 1 ? "session" : "sessions"}</span></footer>
      </section>
      {keyConfirmOpen && <div className="modal-backdrop" role="presentation"><section className="confirm-modal" role="dialog" aria-modal="true" aria-labelledby="key-confirm-title"><KeyRound size={20} /><h2 id="key-confirm-title">Install SSH public key?</h2><p>Ferry will add this computer's public key to <code>{selected}</code>, including the Dropbear key path when present, then verify login with password authentication disabled.</p><p>The one-time password is not saved.</p><div className="modal-actions"><button onClick={() => setKeyConfirmOpen(false)}>Cancel</button><button className="confirm-button" onClick={() => void installPublicKey()}>Install and verify</button></div></section></div>}
    </main>
  );
}

const rootContainer = document.getElementById("root")!;
const rootWindow = window as typeof window & { __ferryRoot?: ReturnType<typeof createRoot> };
const root = rootWindow.__ferryRoot ?? createRoot(rootContainer);
rootWindow.__ferryRoot = root;
root.render(<App />);
