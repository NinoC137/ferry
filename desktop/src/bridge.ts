import { invoke } from "@tauri-apps/api/core";
import type { BlackboxView, DeviceCandidate, DeviceForm, DeviceSummary, ForwardView, OperationResult, ProbeResult, TerminalStarted, TopRow, TransferRequest, WorkflowPlan, WorkflowRequest } from "./types";

const isDesktop = "__TAURI_INTERNALS__" in window;

const demoDevices: DeviceSummary[] = [
  {
    name: "rk3588-lab",
    transport: "ssh",
    endpoint: "root@192.168.1.37:22",
    online: true,
    status: "SSH port reachable",
    hostname: "rk3588-lab",
    os: "linux",
    kernel: "6.1",
    arch: "aarch64",
    lastIp: "192.168.1.37",
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
    os: "",
    kernel: "",
    arch: "",
    lastIp: "",
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

export async function discoverLocalDevices(): Promise<DeviceCandidate[]> {
  return isDesktop ? invoke<DeviceCandidate[]>("discover_local_devices") : [];
}

export async function startTerminal(name: string, cols: number, rows: number, command?: string): Promise<TerminalStarted> {
  if (isDesktop) return invoke<TerminalStarted>("start_terminal", { name, cols, rows, command });
  return {
    sessionId: `preview-${name}-${Date.now()}`,
    title: name,
    transport: (demoDevices.find((item) => item.name === name)?.transport ?? "ssh"),
    wsUrl: "",
  };
}

export const transfer = (request: TransferRequest) => invoke<OperationResult>("transfer", { request });
export const listForwards = () => invoke<ForwardView[]>("list_forwards");
export const addForward = (name: string, spec: string) => invoke<OperationResult>("add_forward", { name, spec });
export const removeForward = (id: string) => invoke<OperationResult>("remove_forward", { id });
export const topSnapshot = () => invoke<TopRow[]>("top_snapshot");
export const blackboxes = () => invoke<BlackboxView[]>("blackboxes");
export const setBlackbox = (name: string, enabled: boolean) => invoke<OperationResult>("set_blackbox", { name, enabled });
export const blackboxBlame = (name: string, lines = 80) => invoke<string>("blackbox_blame", { name, lines });
export const workflowPreview = (kind: string, device: string, nat: boolean, persist: boolean, bootOk: boolean, mode: string) => invoke<WorkflowPlan>("workflow_preview", { kind, device, nat, persist, bootOk, mode });
export const workflowExecute = (request: WorkflowRequest) => invoke<OperationResult>("workflow_execute", { request });
