// Rust 명령 호출. 명령 이름과 인자는 gui/src-tauri/src/commands.rs와 같게 둡니다.
import { invoke } from '@tauri-apps/api/core';

export type DriveState = 'mounted' | 'unmounted' | 'failed';
export type BackendKind = 'local' | 'webdav' | 'sftp' | 's3' | 'ftp';

export interface DriveRow {
  id: string;
  label: string;
  mountPoint: string;
  backend: string;
  state: DriveState;
  message: string | null;
  pendingUploads: number;
  activeDownloads: number;
  autoMount: boolean;
  readOnly: boolean;
  configured: boolean;
}

export interface StatusView {
  locked: boolean;
  locking: boolean;
  pinConfigured: boolean;
  running: boolean;
  agentVersion: string | null;
  configError: string | null;
  drives: DriveRow[];
}

export interface MountResult {
  id: string;
  error: string | null;
}

export interface CacheSettings {
  metadataTtlSecs: number;
  blockSizeMib: number;
  memoryLimitMib: number;
  pollIntervalSecs: number;
  diskLimitMib: number;
  readaheadBlocks: number;
}

export interface TransferSettings {
  uploadKibPerSec: number;
  downloadKibPerSec: number;
}

export interface SecretInfo {
  field: string;
  label: string;
  source: 'credential' | 'env' | 'none';
  reference: string | null;
  stored: boolean;
}

export interface DriveDetail {
  id: string;
  label: string;
  mountPoint: string;
  readOnly: boolean;
  autoMount: boolean;
  backend: BackendKind;
  connection: { label: string; value: string }[];
  connectionEdit: Record<string, string>;
  secrets: SecretInfo[];
  cache: CacheSettings;
  transfer: TransferSettings;
  proxy: { url: string; username: string | null } | null;
}

export interface ProxyInput {
  url: string;
  username: string;
  password: string;
}

export type BackendInput =
  | { type: 'local'; root: string }
  | { type: 'webdav'; url: string; username: string; password: string; pinnedCertSha256: string }
  | {
      type: 'sftp';
      host: string;
      port: number;
      username: string;
      password: string;
      privateKey: string;
      keyPassphrase: string;
      root: string;
      knownHosts: string;
    }
  | {
      type: 's3';
      endpoint: string;
      region: string;
      bucket: string;
      prefix: string;
      accessKeyId: string;
      secretAccessKey: string;
      pathStyle: boolean;
    }
  | {
      type: 'ftp';
      host: string;
      port: number;
      username: string;
      password: string;
      tls: 'none' | 'explicit' | 'implicit';
      encoding: string;
      root: string;
    };

export interface NewDrive {
  id: string;
  label: string;
  mountPoint: string;
  readOnly: boolean;
  autoMount: boolean;
  proxy: ProxyInput | null;
  backend: BackendInput;
}

export interface DrivePatch {
  label: string;
  mountPoint: string;
  readOnly: boolean;
  autoMount: boolean;
  cache: CacheSettings;
  transfer: TransferSettings;
  proxy: ProxyInput | null;
}

export interface SettingsView {
  lockOnSessionLock: boolean;
  idleLockMinutes: number;
  explorerMenu: boolean;
  autostart: boolean;
  autostartCommand: string | null;
  configPath: string;
  logDir: string;
  cliPath: string;
  cliFound: boolean;
  guiVersion: string;
  agentRunning: boolean;
  agentVersion: string | null;
}

/** Rust 쪽 오류 문자열을 Error로 바꿉니다. */
async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(cmd, args);
  } catch (e) {
    throw new Error(typeof e === 'string' ? e : String(e), { cause: e });
  }
}

export const api = {
  status: () => call<StatusView>('agent_status'),
  lock: () => call<void>('lock_drives'),
  unlock: (pin: string) => call<void>('unlock_drives', { pin }),
  setPin: (current: string, newPin: string) => call<void>('set_lock_pin', { current, new: newPin }),
  mount: (id: string) => call<void>('mount_drive', { id }),
  mountAll: () => call<MountResult[]>('mount_all'),
  unmount: (id: string) => call<void>('unmount_drive', { id }),
  drive: (id: string) => call<DriveDetail>('get_drive', { id }),
  addDrive: (drive: NewDrive) => call<string>('add_drive', { drive }),
  updateDrive: (id: string, patch: DrivePatch) => call<void>('update_drive', { id, patch }),
  updateConnection: (id: string, fields: Record<string, string>) => call<string>('update_connection', { id, fields }),
  setSecret: (id: string, field: string, value: string) =>
    call<void>('set_drive_secret', { id, field, value }),
  deleteDrive: (id: string) => call<number>('delete_drive', { id }),
  testConnection: (backend: BackendInput, proxy: ProxyInput | null) =>
    call<string>('test_connection', { backend, proxy }),
  checkDrive: (id: string) => call<string>('check_drive', { id }),
  freeLetters: () => call<string[]>('free_drive_letters'),
  settings: () => call<SettingsView>('get_settings'),
  setExplorerMenu: (enabled: boolean) => call<SettingsView>('set_explorer_menu', { enabled }),
  launchDrive: () => call<string | null>('launch_drive'),
  setAutostart: (enabled: boolean) => call<SettingsView>('set_autostart', { enabled }),
  setAutoLock: (sessionLock: boolean, idleMinutes: number) => call<SettingsView>('set_auto_lock', { sessionLock, idleMinutes }),
  stopAgent: () => call<boolean>('stop_agent'),
};

export const BACKEND_LABEL: Record<string, string> = {
  local: '로컬 폴더',
  webdav: 'WebDAV',
  sftp: 'SFTP',
  s3: 'S3 호환',
  ftp: 'FTP',
};

export const STATE_LABEL: Record<DriveState, string> = {
  mounted: '마운트됨',
  unmounted: '해제됨',
  failed: '실패',
};

export function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
