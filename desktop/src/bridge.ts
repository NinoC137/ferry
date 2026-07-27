import { invoke } from "@tauri-apps/api/core";
import type { DeviceForm, DeviceSummary, ProbeResult, TerminalStarted } from "./types";

const isDesktop = "__TAURI_INTERNALS__" in window;

const demoDevices: DeviceSummary[] = [
  {
    name: "rk3588-lab",
    transport: "ssh",
    endpoint: "root@192.168.1.37:22",
    online: true,
    status: "SSH port reachable",
    hostname: "rk3588-lab",
    lastSeen: Math.floor(Date.now() / 1000),
    hasPassword: false,
    blackboxRunning: false,
  },
  {
    name: "console-mcu",
    transport: "serial",
    endpoint: "/dev/cu.usbserial-1420@1500000",
    online: true,
    status: "Serial device present; black box is recording",
    hostname: "",
    lastSeen: Math.floor(Date.now() / 1000) - 600,
    hasPassword: false,
    blackboxRunning: true,
  },
];

export const desktopAvailable = isDesktop;

export async function listDevices(): Promise<DeviceSummary[]> {
  return isDesktop ? invoke<DeviceSummary[]>("list_devices") : demoDevices;
}

export async function getDevice(name: string): Promise<DeviceForm> {
  if (isDesktop) return invoke<DeviceForm>("get_device", { name });
  const d = demoDevices.find((item) => item.name === name) ?? demoDevices[0];
  return {
    name: d.name,
    transport: d.transport,
    host: d.transport === "ssh" ? "192.168.1.37" : "",
    port: 22,
    user: "root",
    key: "~/.ssh/id_ed25519",
    legacy: false,
    adbSerial: "",
    dev: d.transport === "serial" ? "/dev/cu.usbserial-1420" : "",
    baud: d.transport === "serial" ? 1500000 : 115200,
    dest: "/tmp",
    notes: "Demo profile in browser preview",
  };
}

export async function saveDevice(form: DeviceForm): Promise<DeviceForm> {
  return isDesktop ? invoke<DeviceForm>("save_device", { form }) : form;
}

export async function checkConnection(name: string): Promise<ProbeResult> {
  if (isDesktop) return invoke<ProbeResult>("check_connection", { name });
  const device = demoDevices.find((item) => item.name === name) ?? demoDevices[0];
  return { online: device.online, detail: device.status };
}

export async function startTerminal(name: string, cols: number, rows: number): Promise<TerminalStarted> {
  if (isDesktop) return invoke<TerminalStarted>("start_terminal", { name, cols, rows });
  return {
    sessionId: `preview-${name}-${Date.now()}`,
    title: name,
    transport: (demoDevices.find((item) => item.name === name)?.transport ?? "ssh"),
    wsUrl: "",
  };
}
