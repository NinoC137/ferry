export type Transport = "ssh" | "adb" | "serial";

export interface DeviceSummary {
  name: string;
  transport: Transport;
  endpoint: string;
  online: boolean;
  status: string;
  hostname: string;
  os: string;
  kernel: string;
  arch: string;
  lastIp: string;
  lastSeen: number;
  hasPassword: boolean;
  blackboxRunning: boolean;
}

export interface DeviceForm {
  name: string;
  transport: Transport;
  host: string;
  port: number;
  user: string;
  key: string;
  legacy: boolean;
  adbSerial: string;
  dev: string;
  baud: number;
  dest: string;
  notes: string;
}

export interface ProbeResult {
  online: boolean;
  detail: string;
}

export interface TerminalStarted {
  sessionId: string;
  title: string;
  transport: Transport;
  wsUrl: string;
}
export interface OperationResult { ok: boolean; detail: string; }
export interface TransferRequest { name: string; direction: "push" | "pull"; local: string; remote: string; force: boolean; resume: boolean; verify: boolean; }
export interface ForwardView { id: string; device: string; channel: string; detail: string; alive: boolean; }
export interface TopRow { name: string; online: boolean; cpu: string; memory: string; temperature: string; load: string; }
export interface BlackboxView { name: string; running: boolean; incidents: number; logPath: string; }
export interface WorkflowPlan { kind: string; device: string; preflightOk: boolean; preflight: string; steps: string[]; rollback: string; }
export interface WorkflowRequest { kind: string; device: string; nat: boolean; persist: boolean; bootOk: boolean; mode: string; confirmed: boolean; }
export interface DeviceCandidate { transport: Transport; value: string; detail: string; }
export interface ScanHit { ip: string; open: number[]; banner: string; mac: string; knownAs: string; hostname: string; via: string; legacy: boolean; }

export const newDevice = (): DeviceForm => ({
  name: "new-board",
  transport: "ssh",
  host: "192.168.1.37",
  port: 22,
  user: "root",
  key: "",
  legacy: false,
  adbSerial: "",
  dev: "",
  baud: 115200,
  dest: "/tmp",
  notes: "",
});
