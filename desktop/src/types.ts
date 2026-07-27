export type Transport = "ssh" | "adb" | "serial";

export interface DeviceSummary {
  name: string;
  transport: Transport;
  endpoint: string;
  online: boolean;
  status: string;
  hostname: string;
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
