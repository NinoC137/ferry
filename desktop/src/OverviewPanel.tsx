import { Cpu, Fingerprint, MonitorDot, RefreshCcw, Router } from "lucide-react";
import type { DeviceSummary } from "./types";

interface Props { devices: DeviceSummary[]; busy: boolean; onRefresh: () => void; onSelect: (name: string) => void; }

export function OverviewPanel({ devices, busy, onRefresh, onSelect }: Props) {
  const online = devices.filter((device) => device.online).length;
  const fingerprints = devices.filter((device) => device.hostname || device.kernel || device.arch).length;
  return <section className="overview-panel">
    <header className="overview-header"><div><strong>Ferry Fleet Overview</strong><span>Parallel reachability checks and saved device fingerprints</span></div><button className="command-button" onClick={onRefresh} disabled={busy}><RefreshCcw size={15} className={busy ? "spin" : ""} />Refresh fleet</button></header>
    <div className="overview-scroll">
      <div className="overview-metrics"><div><MonitorDot size={18} /><strong>{online}/{devices.length}</strong><span>reachable</span></div><div><Fingerprint size={18} /><strong>{fingerprints}</strong><span>identified</span></div><div><Router size={18} /><strong>{devices.filter((device) => device.transport === "ssh").length}</strong><span>SSH profiles</span></div></div>
      <div className="overview-list">{devices.map((device) => <button className="overview-device" key={device.name} onClick={() => onSelect(device.name)}><div className="overview-device-title"><span className={`status-dot ${device.online ? "online" : "offline"}`} /><strong>{device.name}</strong><span className="transport-badge">{device.transport}</span><span>{device.status}</span></div><div className="overview-endpoint">{device.endpoint}</div><div className="fingerprint-grid"><span><Fingerprint size={13} />{device.hostname || "identity not collected"}</span><span><Cpu size={13} />{[device.os, device.kernel, device.arch].filter(Boolean).join(" · ") || "platform unknown"}</span>{device.lastIp && <span>last IP {device.lastIp}</span>}</div></button>)}</div>
      {!devices.length && <div className="overview-empty">No profiles yet. Add a device to begin the fleet overview.</div>}
    </div>
  </section>;
}
