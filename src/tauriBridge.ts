import { convertFileSrc as tauriConvertFileSrc, invoke as tauriInvoke } from '@tauri-apps/api/core'
import { listen as tauriListen } from '@tauri-apps/api/event'
import { getCurrentWindow as tauriGetCurrentWindow } from '@tauri-apps/api/window'

type EventCallback<T> = (event: { payload: T }) => void
type UnlistenFn = () => void

type MockFileEntry = {
  name: string
  path: string
  is_dir: boolean
  size: number
  modified: string
  permissions: string
  file_type: string
}

type MockStats = {
  user: string
  home_dir: string
  os: string
  shell: string
  process_count: number
  cpu_usage: number
  memory_used: number
  memory_total: number
  disk_used: number
  disk_total: number
  network_received: number
  network_transmitted: number
}

type MockProcessEntry = {
  pid: number
  name: string
  cpu_usage: number
  memory: number
  status: string
  command: string
}

const sandboxListeners = new Map<string, Set<EventCallback<unknown>>>()
const mockSshTimers = new Map<string, number>()
const mockSshInputs = new Map<string, string>()
const mockSshPrompts = new Map<string, string>()
const mockRdpTimers = new Map<string, number>()
const mockRdpStatusChannels = new Map<string, unknown>()
const mockCancelledRdpTransfers = new Set<string>()
const mockCancelledFileDownloads = new Set<string>()
const sandboxDragDropHandlers = new Set<(event: { payload: SandboxDragDropPayload }) => void>()
const mockSshTunnels = new Map<string, { id: string; pid: number; startedAt: number; mode: string; listenPort: number }>()
let mockRdpClipboardProgressStep = 0
let mockRdpClipboardCancelled = false
let mockRdpClipboardPasteStarted = false

type SandboxDragDropPayload = {
  type: string
  paths?: string[]
  position?: { x: number; y: number }
}

export const isSandboxMode =
  import.meta.env.VITE_XUNDU_SANDBOX === '1'
  || (typeof window !== 'undefined' && !('__TAURI_INTERNALS__' in window))

const sandboxBackgroundPreview = `data:image/svg+xml;charset=UTF-8,${encodeURIComponent(`
  <svg xmlns="http://www.w3.org/2000/svg" width="1600" height="1000" viewBox="0 0 1600 1000">
    <defs>
      <linearGradient id="base" x1="0" y1="0" x2="1" y2="1">
        <stop stop-color="#07111e"/><stop offset="0.48" stop-color="#122947"/><stop offset="1" stop-color="#261d43"/>
      </linearGradient>
      <radialGradient id="blue"><stop stop-color="#42b7ff" stop-opacity=".72"/><stop offset="1" stop-color="#42b7ff" stop-opacity="0"/></radialGradient>
      <radialGradient id="violet"><stop stop-color="#a878ff" stop-opacity=".58"/><stop offset="1" stop-color="#a878ff" stop-opacity="0"/></radialGradient>
    </defs>
    <rect width="1600" height="1000" fill="url(#base)"/>
    <ellipse cx="440" cy="270" rx="620" ry="420" fill="url(#blue)"/>
    <ellipse cx="1320" cy="760" rx="650" ry="470" fill="url(#violet)"/>
  </svg>
`)}`

export function convertFileAssetSrc(filePath: string) {
  if (!filePath) return ''
  if (filePath.startsWith('data:') || filePath.startsWith('blob:')) return filePath
  return isSandboxMode ? sandboxBackgroundPreview : tauriConvertFileSrc(filePath)
}

export function getCurrentWindow() {
  if (!isSandboxMode) return tauriGetCurrentWindow()

  return {
    startDragging: () => Promise.resolve(),
    close: () => Promise.resolve(),
    minimize: () => Promise.resolve(),
    toggleMaximize: () => Promise.resolve(),
    scaleFactor: () => Promise.resolve(1),
    onDragDropEvent: (handler: (event: { payload: SandboxDragDropPayload }) => void) => {
      const sandboxWindow = window as typeof window & {
        __XUNDU_SANDBOX_DROP_FILES__?: (paths: string[], x: number, y: number) => void
        __XUNDU_SANDBOX_DRAG_FILES__?: (type: string, paths: string[], x: number, y: number) => void
      }
      sandboxDragDropHandlers.add(handler)
      const dispatch = (type: string, paths: string[], x: number, y: number) => {
        const event = { payload: { type, paths, position: { x, y } } }
        sandboxDragDropHandlers.forEach((listener) => listener(event))
      }
      sandboxWindow.__XUNDU_SANDBOX_DROP_FILES__ = (paths, x, y) => dispatch('drop', paths, x, y)
      sandboxWindow.__XUNDU_SANDBOX_DRAG_FILES__ = dispatch
      return Promise.resolve(() => {
        sandboxDragDropHandlers.delete(handler)
        if (sandboxDragDropHandlers.size === 0) {
          delete sandboxWindow.__XUNDU_SANDBOX_DROP_FILES__
          delete sandboxWindow.__XUNDU_SANDBOX_DRAG_FILES__
        }
      })
    },
  }
}

export function listen<T>(eventName: string, callback: EventCallback<T>): Promise<UnlistenFn> {
  if (!isSandboxMode) return tauriListen(eventName, callback)

  const listeners = sandboxListeners.get(eventName) ?? new Set<EventCallback<unknown>>()
  const typedCallback = callback as EventCallback<unknown>
  listeners.add(typedCallback)
  sandboxListeners.set(eventName, listeners)

  return Promise.resolve(() => {
    listeners.delete(typedCallback)
  })
}

export function invoke<T = unknown>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!isSandboxMode) return tauriInvoke<T>(command, args)
  return mockInvoke<T>(command, args ?? {})
}

function emitSandbox<T>(eventName: string, payload: T) {
  const listeners = sandboxListeners.get(eventName)
  if (!listeners) return
  listeners.forEach((callback) => callback({ payload }))
}

async function mockInvoke<T>(command: string, args: Record<string, unknown>): Promise<T> {
  await delay(mockLatency(command))

  switch (command) {
    case 'credential_vault_status':
      return { backend: 'sandbox-session-vault', persistent: true } as T
    case 'credential_store_many': {
      const vault = readMockCredentialVault()
      const entries = Array.isArray(args.entries) ? args.entries : []
      entries.forEach((entry) => {
        if (!entry || typeof entry !== 'object') return
        const record = entry as { key?: unknown; secret?: unknown }
        const key = String(record.key ?? '')
        if (key) vault[key] = String(record.secret ?? '')
      })
      writeMockCredentialVault(vault)
      return undefined as T
    }
    case 'credential_get_many': {
      const vault = readMockCredentialVault()
      const keys = Array.isArray(args.keys) ? args.keys.map(String) : []
      return Object.fromEntries(keys.filter((key) => key in vault).map((key) => [key, vault[key]])) as T
    }
    case 'credential_delete_many': {
      const vault = readMockCredentialVault()
      const keys = Array.isArray(args.keys) ? args.keys.map(String) : []
      keys.forEach((key) => delete vault[key])
      writeMockCredentialVault(vault)
      return undefined as T
    }
    case 'diag_log_frontend':
    case 'ssh_register_auth_profiles':
    case 'set_remote_aux_limit':
    case 'local_shell_resize':
    case 'ssh_resize':
      return undefined as T
    case 'check_app_update': {
      const available = Boolean((window as typeof window & { __XUNDU_SANDBOX_UPDATE_AVAILABLE__?: boolean }).__XUNDU_SANDBOX_UPDATE_AVAILABLE__)
      return available
        ? {
            currentVersion: '0.2.0',
            latestVersion: '0.3.0',
            updateAvailable: true,
            status: 'available',
            notes: '稳定性优化与更新流程测试。',
            releaseUrl: 'https://github.com/KaiGe7384/XunDuTerminal/releases/tag/v0.3.0',
            publishedAt: '2026-07-22T00:00:00Z',
            installer: {
              url: 'https://github.com/KaiGe7384/XunDuTerminal/releases/download/v0.3.0/XunDuTerminal_0.3.0_x64-setup.exe',
              sha256: '0'.repeat(64),
              size: 8 * 1024 * 1024,
            },
          } as T
        : {
            currentVersion: '0.2.0',
            latestVersion: '0.2.0',
            updateAvailable: false,
            status: 'current',
            notes: null,
            releaseUrl: null,
            publishedAt: null,
            installer: null,
          } as T
    }
    case 'download_app_update': {
      const transferId = String(args.transferId ?? '')
      const totalBytes = Number(args.size) || 8 * 1024 * 1024
      const version = String(args.version ?? '0.3.0')
      const fileName = `XunDuTerminal_${version}_x64-setup.exe`
      postSandboxChannel(args.onProgress, {
        totalBytes,
        transferredBytes: 0,
        bytesPerSecond: 0,
        copiedFiles: 0,
        totalFiles: 1,
        currentFile: fileName,
        completed: false,
      })
      for (let step = 1; step <= 4; step += 1) {
        await delay(70)
        if (mockCancelledFileDownloads.delete(transferId)) throw new Error('文件下载已取消')
        postSandboxChannel(args.onProgress, {
          totalBytes,
          transferredBytes: totalBytes * step / 4,
          bytesPerSecond: 4 * 1024 * 1024,
          copiedFiles: step === 4 ? 1 : 0,
          totalFiles: 1,
          currentFile: step === 4 ? '' : fileName,
          completed: step === 4,
        })
      }
      const installerPath = `C:/Users/sandbox/AppData/Local/com.xundu.terminal/cache/updates/${fileName}`
      const sandboxWindow = window as typeof window & { __XUNDU_SANDBOX_UPDATE_DOWNLOADS__?: string[] }
      sandboxWindow.__XUNDU_SANDBOX_UPDATE_DOWNLOADS__ = [...(sandboxWindow.__XUNDU_SANDBOX_UPDATE_DOWNLOADS__ ?? []), installerPath]
      return { installerPath, totalBytes } as T
    }
    case 'launch_app_update': {
      const sandboxWindow = window as typeof window & { __XUNDU_SANDBOX_UPDATE_LAUNCHES__?: string[] }
      sandboxWindow.__XUNDU_SANDBOX_UPDATE_LAUNCHES__ = [
        ...(sandboxWindow.__XUNDU_SANDBOX_UPDATE_LAUNCHES__ ?? []),
        String(args.installerPath ?? ''),
      ]
      return undefined as T
    }
    case 'open_external_url': {
      const sandboxWindow = window as typeof window & { __XUNDU_SANDBOX_EXTERNAL_URLS__?: string[] }
      sandboxWindow.__XUNDU_SANDBOX_EXTERNAL_URLS__ = [
        ...(sandboxWindow.__XUNDU_SANDBOX_EXTERNAL_URLS__ ?? []),
        String(args.url ?? ''),
      ]
      return undefined as T
    }
    case 'save_text_export': {
      const sandboxWindow = window as typeof window & { __XUNDU_SANDBOX_TEXT_EXPORT__?: { name: string; content: string } }
      sandboxWindow.__XUNDU_SANDBOX_TEXT_EXPORT__ = {
        name: String(args.suggestedName ?? 'XunDuTerminal-export.json'),
        content: String(args.content ?? ''),
      }
      return `C:/Users/sandbox/Downloads/${sandboxWindow.__XUNDU_SANDBOX_TEXT_EXPORT__.name}` as T
    }
    case 'open_text_import':
      return ((window as typeof window & { __XUNDU_SANDBOX_IMPORT_TEXT__?: string }).__XUNDU_SANDBOX_IMPORT_TEXT__ ?? null) as T
    case 'choose_app_background': {
      const sandboxWindow = window as typeof window & { __XUNDU_SANDBOX_BACKGROUND_SELECTIONS__?: number }
      sandboxWindow.__XUNDU_SANDBOX_BACKGROUND_SELECTIONS__ = (sandboxWindow.__XUNDU_SANDBOX_BACKGROUND_SELECTIONS__ ?? 0) + 1
      return {
        path: `C:/Users/sandbox/AppData/Local/com.xundu.terminal/background/workspace-background-${sandboxWindow.__XUNDU_SANDBOX_BACKGROUND_SELECTIONS__}.png`,
        name: 'aurora-workspace.png',
      } as T
    }
    case 'clear_app_background': {
      const sandboxWindow = window as typeof window & { __XUNDU_SANDBOX_BACKGROUND_CLEARS__?: number }
      sandboxWindow.__XUNDU_SANDBOX_BACKGROUND_CLEARS__ = (sandboxWindow.__XUNDU_SANDBOX_BACKGROUND_CLEARS__ ?? 0) + 1
      return undefined as T
    }
    case 'ssh_execute_command': {
      const commandText = String(args.command ?? '')
      const server = `${String(args.user ?? 'root')}@${String(args.host ?? 'sandbox')}`
      const sandboxWindow = window as typeof window & { __XUNDU_SANDBOX_BATCH_COMMANDS__?: Array<{ server: string; command: string }> }
      sandboxWindow.__XUNDU_SANDBOX_BATCH_COMMANDS__ = [
        ...(sandboxWindow.__XUNDU_SANDBOX_BATCH_COMMANDS__ ?? []),
        { server, command: commandText },
      ]
      if (/\b(false|exit\s+[1-9])\b/.test(commandText)) {
        return { output: `sandbox error: ${commandText}`, exitCode: 1, durationMs: 80, timedOut: false } as T
      }
      return { output: `${server}: sandbox batch output: ${commandText}`, exitCode: 0, durationMs: 80, timedOut: false } as T
    }
    case 'ssh_tunnel_start': {
      const id = String(args.tunnelId ?? '')
      const status = {
        id,
        pid: 9000 + mockSshTunnels.size,
        startedAt: Date.now(),
        mode: String(args.mode ?? 'local'),
        listenPort: Number(args.listenPort) || 0,
      }
      mockSshTunnels.set(id, status)
      return status as T
    }
    case 'ssh_tunnel_stop':
      mockSshTunnels.delete(String(args.tunnelId ?? ''))
      return undefined as T
    case 'ssh_tunnel_list':
      return [...mockSshTunnels.values()] as T
    case 'phase2_stress_terminal_output': {
      const sessionId = String(args.sessionId ?? 'local-terminal-1')
      const requestedBytes = Math.min(100 * 1024 * 1024, Math.max(0, Number(args.bytes) || 0))
      const chunk = `${'X'.repeat(64 * 1024 - 2)}\r\n`
      let emitted = 0
      while (emitted < requestedBytes) {
        const data = chunk.slice(0, Math.min(chunk.length, requestedBytes - emitted))
        emitSandbox('local:data', { session_id: sessionId, data })
        emitted += data.length
        if (emitted % (1024 * 1024) === 0) await delay(0)
      }
      emitSandbox('local:data', { session_id: sessionId, data: '\r\nPHASE2_STRESS_DONE\r\n' })
      return emitted as T
    }
    case 'local_home_dir':
      return 'C:/Users/sandbox' as T
    case 'choose_ssh_private_key':
      return 'C:/Users/sandbox/.ssh/id_ed25519' as T
    case 'export_diagnostics':
      return 'C:/Users/sandbox/Downloads/xundu-diagnostics.log' as T
    case 'ssh_import_config':
      return [{
        name: 'sandbox-config-host',
        host: 'config.example.test',
        user: 'deploy',
        port: 2222,
        group: 'SSH Config',
        auth: 'Key',
        privateKeyPath: 'C:/Users/sandbox/.ssh/id_ed25519',
        password: '',
      }] as T
    case 'local_detect_cli_tools':
      return [
        { id: 'claude', name: 'Claude Code', command: 'claude' },
        { id: 'codex', name: 'Codex', command: 'codex' },
        { id: 'kiro', name: 'Kiro CLI', command: 'kiro-cli' },
      ] as T
    case 'remote_detect_cli_tools':
      return [
        { id: 'claude', name: 'Claude Code', command: 'claude' },
        { id: 'codex', name: 'Codex', command: 'codex' },
      ] as T
    case 'local_list_drives':
      return [
        mockEntry('C:\\', 'C:\\', true, 256 * 1024 * 1024 * 1024, 0, '本地磁盘'),
        mockEntry('F:\\', 'F:\\', true, 512 * 1024 * 1024 * 1024, 1, '本地磁盘'),
      ] as T
    case 'local_shell_start':
      startMockLocalShell(String(args.sessionId ?? 'local'))
      return undefined as T
    case 'local_shell_write':
      writeMockLocalShell(String(args.sessionId ?? 'local'), String(args.data ?? ''))
      return undefined as T
    case 'local_shell_stop':
      emitSandbox('local:closed', {
        session_id: String(args.sessionId ?? 'local'),
        message: 'Sandbox local shell closed',
      })
      return undefined as T
    case 'ssh_connect':
      startMockSsh(String(args.sessionId ?? 'ssh'), args)
      return undefined as T
    case 'ssh_write':
      writeMockSsh(String(args.sessionId ?? 'ssh'), String(args.data ?? ''))
      return undefined as T
    case 'ssh_disconnect':
      stopMockSsh(String(args.sessionId ?? 'ssh'), 'Sandbox SSH session disconnected')
      return undefined as T
    case 'rdp_connect':
      startMockRdp(String(args.sessionId ?? 'desktop'), args)
      return undefined as T
    case 'rdp_input':
      if (args.input && typeof args.input === 'object' && (args.input as { type?: string }).type === 'text') {
        const sandboxWindow = window as typeof window & { __XUNDU_SANDBOX_RDP_TEXTS__?: string[] }
        const text = String((args.input as { text?: unknown }).text ?? '')
        sandboxWindow.__XUNDU_SANDBOX_RDP_TEXTS__ = [...(sandboxWindow.__XUNDU_SANDBOX_RDP_TEXTS__ ?? []), text]
      }
      if (args.input && typeof args.input === 'object' && (args.input as { type?: string }).type === 'resize') {
        const sandboxWindow = window as typeof window & {
          __XUNDU_SANDBOX_RDP_RESIZES__?: Array<{ width: number; height: number }>
          __XUNDU_SANDBOX_RDP_FAIL_NEXT_RESIZE__?: boolean
        }
        const input = args.input as { width?: unknown; height?: unknown }
        sandboxWindow.__XUNDU_SANDBOX_RDP_RESIZES__ = [
          ...(sandboxWindow.__XUNDU_SANDBOX_RDP_RESIZES__ ?? []),
          { width: Number(input.width) || 0, height: Number(input.height) || 0 },
        ]
        if (sandboxWindow.__XUNDU_SANDBOX_RDP_FAIL_NEXT_RESIZE__) {
          sandboxWindow.__XUNDU_SANDBOX_RDP_FAIL_NEXT_RESIZE__ = false
          const sessionId = String(args.sessionId ?? 'desktop')
          const statusChannel = mockRdpStatusChannels.get(sessionId)
          postSandboxChannel(statusChannel, {
            type: 'error',
            code: 'frame_decode_failed',
            message: 'The remote server returned an incomplete desktop frame',
          })
          stopMockRdp(sessionId, false)
        }
      }
      if (args.input && typeof args.input === 'object') {
        const input = args.input as { type?: string; code?: number; down?: boolean }
        if (input.type === 'key' && input.code === 0x2f && input.down) mockRdpClipboardPasteStarted = true
      }
      return undefined as T
    case 'rdp_disconnect':
      stopMockRdp(String(args.sessionId ?? 'desktop'))
      return undefined as T
    case 'rdp_clipboard_file_paths':
      return ((window as typeof window & { __XUNDU_SANDBOX_CLIPBOARD_FILES__?: string[] }).__XUNDU_SANDBOX_CLIPBOARD_FILES__ ?? []) as T
    case 'rdp_clipboard_sequence_number':
      return ((window as typeof window & { __XUNDU_SANDBOX_CLIPBOARD_SEQUENCE__?: number }).__XUNDU_SANDBOX_CLIPBOARD_SEQUENCE__ ?? 1) as T
    case 'rdp_cancel_file_transfer':
      mockCancelledRdpTransfers.add(String(args.transferId ?? ''))
      mockRdpClipboardCancelled = true
      return undefined as T
    case 'rdp_offer_clipboard_files': {
      const paths = Array.isArray(args.paths) ? args.paths.map(String) : []
      const sandboxWindow = window as typeof window & { __XUNDU_SANDBOX_RDP_UPLOADS__?: string[][] }
      sandboxWindow.__XUNDU_SANDBOX_RDP_UPLOADS__ = [...(sandboxWindow.__XUNDU_SANDBOX_RDP_UPLOADS__ ?? []), paths]
      mockRdpClipboardProgressStep = 0
      mockRdpClipboardCancelled = false
      mockRdpClipboardPasteStarted = false
      return { totalFiles: paths.length, totalBytes: 16 * 1024 * 1024 } as T
    }
    case 'rdp_file_clipboard_progress':
      if (mockRdpClipboardCancelled) throw new Error('文件传输已取消')
      if (!mockRdpClipboardPasteStarted) {
        return {
          totalBytes: 16 * 1024 * 1024,
          transferredBytes: 0,
          totalFiles: 1,
          currentFile: '',
          completed: false,
          accepted: true,
        } as T
      }
      mockRdpClipboardProgressStep += 1
      return (mockRdpClipboardProgressStep < 3
        ? {
            totalBytes: 16 * 1024 * 1024,
            transferredBytes: 4 * 1024 * 1024,
            totalFiles: 1,
            currentFile: 'release.zip',
            completed: false,
            accepted: true,
          }
        : {
            totalBytes: 16 * 1024 * 1024,
            transferredBytes: 16 * 1024 * 1024,
            totalFiles: 1,
            currentFile: 'release.zip',
            completed: true,
            accepted: true,
          }) as T
    case 'rdp_upload_files': {
      const paths = Array.isArray(args.paths) ? args.paths.map(String) : []
      const transferId = String(args.transferId ?? '')
      const sandboxWindow = window as typeof window & { __XUNDU_SANDBOX_RDP_UPLOADS__?: string[][] }
      sandboxWindow.__XUNDU_SANDBOX_RDP_UPLOADS__ = [...(sandboxWindow.__XUNDU_SANDBOX_RDP_UPLOADS__ ?? []), paths]
      postSandboxChannel(args.onProgress, {
        totalBytes: 16 * 1024 * 1024,
        transferredBytes: 4 * 1024 * 1024,
        bytesPerSecond: 2 * 1024 * 1024,
        copiedFiles: 0,
        totalFiles: paths.length,
        currentFile: paths[0] ?? '',
        completed: false,
      })
      for (let step = 0; step < 4; step += 1) {
        await delay(50)
        if (mockCancelledRdpTransfers.delete(transferId)) throw new Error('文件传输已取消')
      }
      postSandboxChannel(args.onProgress, {
        totalBytes: 16 * 1024 * 1024,
        transferredBytes: 16 * 1024 * 1024,
        bytesPerSecond: 6 * 1024 * 1024,
        copiedFiles: paths.length,
        totalFiles: paths.length,
        currentFile: '',
        completed: true,
      })
      return { copiedFiles: paths.length, destination: '\\\\sandbox-rdp\\C$\\Users\\sandbox-user\\Desktop' } as T
    }
    case 'local_list_dir':
      return mockDirectory(String(args.path ?? 'C:/Users/sandbox')) as T
    case 'remote_list_dir': {
      const sandboxWindow = window as typeof window & {
        __XUNDU_SANDBOX_HOST_KEY_MISMATCH__?: boolean
        __XUNDU_SANDBOX_KEX_FAILURES_REMAINING__?: number
        __XUNDU_SANDBOX_REMOTE_LIST_SUCCESSES__?: number
      }
      if (sandboxWindow.__XUNDU_SANDBOX_HOST_KEY_MISMATCH__ && String(args.host ?? '') === '192.0.2.198') {
        throw new Error('SSH host key mismatch for sandbox.example:22; the connection was blocked (00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff:10:21:32:43:54:65:76:87:98:a9:ba:cb:dc:ed:fe:0f)')
      }
      const kexFailuresRemaining = sandboxWindow.__XUNDU_SANDBOX_KEX_FAILURES_REMAINING__ ?? 0
      if (kexFailuresRemaining > 0) {
        sandboxWindow.__XUNDU_SANDBOX_KEX_FAILURES_REMAINING__ = kexFailuresRemaining - 1
        throw new Error('SSH handshake failed: [Session(-5)] Unable to exchange encryption keys')
      }
      sandboxWindow.__XUNDU_SANDBOX_REMOTE_LIST_SUCCESSES__ = (sandboxWindow.__XUNDU_SANDBOX_REMOTE_LIST_SUCCESSES__ ?? 0) + 1
      return {
        path: String(args.path ?? '/root') === '~' ? '/root' : String(args.path ?? '/root'),
        entries: mockDirectory(String(args.path ?? '/root'), true),
      } as T
    }
    case 'ssh_replace_known_host': {
      const sandboxWindow = window as typeof window & {
        __XUNDU_SANDBOX_HOST_KEY_MISMATCH__?: boolean
        __XUNDU_SANDBOX_HOST_KEY_REPLACEMENTS__?: number
      }
      sandboxWindow.__XUNDU_SANDBOX_HOST_KEY_MISMATCH__ = false
      sandboxWindow.__XUNDU_SANDBOX_HOST_KEY_REPLACEMENTS__ = (sandboxWindow.__XUNDU_SANDBOX_HOST_KEY_REPLACEMENTS__ ?? 0) + 1
      return String(args.expectedFingerprint ?? '') as T
    }
    case 'local_read_file':
    case 'remote_read_file': {
      const path = String(args.path ?? 'sandbox.json')
      const sandboxWindow = window as typeof window & { __XUNDU_SANDBOX_FILE_READS__?: Record<string, number> }
      const reads = sandboxWindow.__XUNDU_SANDBOX_FILE_READS__ ?? {}
      const revision = (reads[path] ?? 0) + 1
      reads[path] = revision
      sandboxWindow.__XUNDU_SANDBOX_FILE_READS__ = reads
      return mockFileContent(path, revision) as T
    }
    case 'local_write_file':
    case 'remote_write_file':
      return undefined as T
    case 'choose_file_download_destination':
      return `C:/Users/sandbox/Downloads/${String(args.suggestedName ?? 'download')}` as T
    case 'choose_file_upload_sources':
      return ['C:/Users/sandbox/Uploads/release.zip', 'C:/Users/sandbox/Uploads/checksums.txt'] as T
    case 'cancel_file_download':
      mockCancelledFileDownloads.add(String(args.transferId ?? ''))
      return undefined as T
    case 'local_download_path':
    case 'remote_download_path': {
      const transferId = String(args.transferId ?? '')
      const destination = String(args.destination ?? 'C:/Users/sandbox/Downloads/download')
      const source = String(args.remotePath ?? args.source ?? 'download')
      const totalBytes = 8 * 1024 * 1024
      const sandboxWindow = window as typeof window & { __XUNDU_SANDBOX_FILE_DOWNLOADS__?: string[] }
      sandboxWindow.__XUNDU_SANDBOX_FILE_DOWNLOADS__ = [...(sandboxWindow.__XUNDU_SANDBOX_FILE_DOWNLOADS__ ?? []), source]
      postSandboxChannel(args.onProgress, {
        totalBytes,
        transferredBytes: 0,
        bytesPerSecond: 0,
        copiedFiles: 0,
        totalFiles: 1,
        currentFile: source,
        completed: false,
      })
      for (let step = 1; step <= 4; step += 1) {
        await delay(70)
        if (mockCancelledFileDownloads.delete(transferId)) throw new Error('文件下载已取消')
        postSandboxChannel(args.onProgress, {
          totalBytes,
          transferredBytes: totalBytes * step / 4,
          bytesPerSecond: 4 * 1024 * 1024,
          copiedFiles: step === 4 ? 1 : 0,
          totalFiles: 1,
          currentFile: step === 4 ? '' : source,
          completed: step === 4,
        })
      }
      return { destination, copiedFiles: 1, totalFiles: 1, totalBytes } as T
    }
    case 'remote_upload_paths': {
      const transferId = String(args.transferId ?? '')
      const sources = Array.isArray(args.sources) ? args.sources.map(String) : []
      const remoteDirectory = String(args.remoteDirectory ?? '/root')
      const totalBytes = 12 * 1024 * 1024
      const totalFiles = sources.reduce((count, source) => count + (/folder/i.test(source) ? 3 : 1), 0)
      const sandboxWindow = window as typeof window & { __XUNDU_SANDBOX_FILE_UPLOADS__?: string[][] }
      sandboxWindow.__XUNDU_SANDBOX_FILE_UPLOADS__ = [...(sandboxWindow.__XUNDU_SANDBOX_FILE_UPLOADS__ ?? []), sources]
      postSandboxChannel(args.onProgress, {
        totalBytes,
        transferredBytes: 0,
        bytesPerSecond: 0,
        copiedFiles: 0,
        totalFiles,
        currentFile: sources[0] ?? '',
        completed: false,
      })
      for (let step = 1; step <= 8; step += 1) {
        await delay(180)
        if (mockCancelledFileDownloads.delete(transferId)) throw new Error('文件上传已取消')
        postSandboxChannel(args.onProgress, {
          totalBytes,
          transferredBytes: totalBytes * step / 8,
          bytesPerSecond: 3 * 1024 * 1024,
          copiedFiles: step === 8 ? totalFiles : 0,
          totalFiles,
          currentFile: step === 8 ? '' : sources[Math.min(sources.length - 1, Math.floor(step / 4))] ?? '',
          completed: step === 8,
        })
      }
      return {
        destination: remoteDirectory === '~' ? '/root' : remoteDirectory,
        copiedFiles: totalFiles,
        totalFiles,
        totalBytes,
      } as T
    }
    case 'local_rename_path':
    case 'remote_rename_path':
      return String(args.path ?? '').replace(/[^/\\]+$/, String(args.newName ?? 'renamed')) as T
    case 'local_delete_path':
    case 'remote_delete_path':
      return undefined as T
    case 'local_compress_paths':
      return 'C:/Users/sandbox/archive.zip' as T
    case 'local_extract_archive':
      return 'C:/Users/sandbox/archive' as T
    case 'local_system_stats':
      return mockStats('localhost') as T
    case 'remote_system_stats':
      return mockStats(String(args.host ?? 'sandbox-server')) as T
    case 'local_process_list':
      return mockProcesses(false) as T
    case 'remote_process_list':
      return mockProcesses(true) as T
    default:
      console.info(`[sandbox invoke] ${command}`, args)
      return undefined as T
  }
}

function readMockCredentialVault() {
  try {
    return JSON.parse(sessionStorage.getItem('__xundu.sandbox.credentials') ?? '{}') as Record<string, string>
  } catch {
    return {} as Record<string, string>
  }
}

function writeMockCredentialVault(vault: Record<string, string>) {
  sessionStorage.setItem('__xundu.sandbox.credentials', JSON.stringify(vault))
}

function mockProcesses(remote: boolean): MockProcessEntry[] {
  const names = remote
    ? ['node', 'python3', 'sshd', 'nginx', 'redis-server', 'dockerd', 'containerd', 'systemd']
    : ['XunDuTerminal.exe', 'Code.exe', 'chrome.exe', 'powershell.exe', 'explorer.exe', 'node.exe']
  return Array.from({ length: 42 }, (_, index) => {
    const name = names[index % names.length]
    return {
      pid: (remote ? 1100 : 4200) + index * 17,
      name,
      cpu_usage: Math.max(0.1, 28 - index * 0.58),
      memory: (18 + (index % 11) * 13) * 1024 * 1024,
      status: index % 7 === 0 ? 'Sleep' : 'Run',
      command: remote ? `/usr/bin/${name} --worker=${index}` : `C:\\Program Files\\${name} --worker=${index}`,
    }
  })
}

function startMockLocalShell(sessionId: string) {
  emitSandbox('local:data', {
    session_id: sessionId,
    data: '本地终端 1\r\n本地终端已启动。\r\n\r\nC:\\Users\\sandbox>',
  })
}

function writeMockLocalShell(sessionId: string, data: string) {
  const sandboxWindow = window as typeof window & { __XUNDU_SANDBOX_LOCAL_WRITES__?: Array<{ sessionId: string; data: string }> }
  sandboxWindow.__XUNDU_SANDBOX_LOCAL_WRITES__ = [
    ...(sandboxWindow.__XUNDU_SANDBOX_LOCAL_WRITES__ ?? []),
    { sessionId, data },
  ]
  const command = data.replace(/\r/g, '').trim()
  if (!command) {
    emitSandbox('local:data', { session_id: sessionId, data: '\r\nC:\\Users\\sandbox>' })
    return
  }
  emitSandbox('local:data', {
    session_id: sessionId,
    data: `\r\n${command}\r\nsandbox: ${command} 已执行\r\nC:\\Users\\sandbox>`,
  })
}

function startMockSsh(sessionId: string, args: Record<string, unknown>) {
  const sandboxWindow = window as typeof window & { __XUNDU_SANDBOX_SSH_CONNECTS__?: number }
  sandboxWindow.__XUNDU_SANDBOX_SSH_CONNECTS__ = (sandboxWindow.__XUNDU_SANDBOX_SSH_CONNECTS__ ?? 0) + 1
  const host = String(args.host ?? '127.0.0.1')
  const user = String(args.user ?? 'root')
  stopMockSsh(sessionId, '', false)
  mockSshInputs.set(sessionId, '')
  mockSshPrompts.set(sessionId, `${user}@${host}:~# `)

  window.setTimeout(() => {
    emitSandbox('ssh:connected', { session_id: sessionId, message: 'SSH connected' })
    emitSandbox('ssh:data', {
      session_id: sessionId,
      data: [
        `Welcome to Sandbox Linux 24.04 (${host})`,
        'System load: 0.13        Processes: 168',
        'Usage of /: 22.8%        Memory usage: 8%',
        '',
        `${user}@${host}:~# `,
      ].join('\r\n'),
    })
  }, 120)

  let tick = 0
  const timer = window.setInterval(() => {
    tick += 1
    emitSandbox('ssh:health', {
      session_id: sessionId,
      connected: true,
      idle_ms: tick * 250,
      write_idle_ms: tick * 500,
      connected_ms: tick * 1000,
      total_read: 1200 + tick * 96,
      total_written: tick * 8,
    })
    if (tick <= 8) {
      emitSandbox('ssh:data', {
        session_id: sessionId,
        data: `\r\n${new Date().toLocaleTimeString('zh-CN', { hour12: false })} sandbox stream ${tick}\r\n${user}@${host}:~# `,
      })
    }
  }, 1000)
  mockSshTimers.set(sessionId, timer)
}

function writeMockSsh(sessionId: string, data: string) {
  const sandboxWindow = window as typeof window & { __XUNDU_SANDBOX_SSH_WRITES__?: Array<{ sessionId: string; data: string }> }
  sandboxWindow.__XUNDU_SANDBOX_SSH_WRITES__ = [
    ...(sandboxWindow.__XUNDU_SANDBOX_SSH_WRITES__ ?? []),
    { sessionId, data },
  ]
  if (!data || data.startsWith('\x1b')) return
  const prompt = mockSshPrompts.get(sessionId) ?? 'root@sandbox:~# '
  let input = mockSshInputs.get(sessionId) ?? ''
  let output = ''

  for (const character of data) {
    if (character === '\r' || character === '\n') {
      const command = input.trim()
      output += '\r\n'
      if (command === 'bt') {
        output += [
          '================宝塔面板命令行================',
          ...Array.from({ length: 42 }, (_, index) => `(${index + 1}) 宝塔面板测试菜单项 ${index + 1}`),
          '(0) 取消',
          '================================================',
          '请输入命令编号：',
        ].join('\r\n')
      } else {
        if (command) output += `sandbox output: ${command}\r\n`
        output += prompt
      }
      input = ''
      continue
    }
    if (character === '\x7f' || character === '\b') {
      if (input) {
        input = input.slice(0, -1)
        output += '\b \b'
      }
      continue
    }
    if (character === '\x15') {
      input = ''
      output += `\x1b[2K\r${prompt}`
      continue
    }
    if (character === '\x03') {
      input = ''
      output += `^C\r\n${prompt}`
      continue
    }
    if (character.charCodeAt(0) < 32) continue
    input += character
    output += character
  }

  mockSshInputs.set(sessionId, input)
  if (output) emitSandbox('ssh:data', { session_id: sessionId, data: output })
}

function stopMockSsh(sessionId: string, message: string, emitClosed = true) {
  const timer = mockSshTimers.get(sessionId)
  if (timer !== undefined) window.clearInterval(timer)
  mockSshTimers.delete(sessionId)
  mockSshInputs.delete(sessionId)
  mockSshPrompts.delete(sessionId)
  if (emitClosed && message) {
    emitSandbox('ssh:closed', { session_id: sessionId, message })
  }
}

function startMockRdp(sessionId: string, args: Record<string, unknown>) {
  const sandboxWindow = window as typeof window & { __XUNDU_SANDBOX_RDP_CONNECTS__?: number }
  sandboxWindow.__XUNDU_SANDBOX_RDP_CONNECTS__ = (sandboxWindow.__XUNDU_SANDBOX_RDP_CONNECTS__ ?? 0) + 1
  stopMockRdp(sessionId, false)
  mockRdpStatusChannels.set(sessionId, args.onStatus)
  postSandboxChannel(args.onStatus, { type: 'connecting' })
  const timer = window.setTimeout(() => {
    if (String(args.host) === 'sandbox-rdp' && String(args.security) === 'nla') {
      postSandboxChannel(args.onStatus, {
        type: 'error',
        code: 'timeout',
        message: 'Sandbox NLA handshake timed out',
      })
      return
    }
    const width = Math.max(320, Math.min(960, Number(args.width) || 640))
    const height = Math.max(200, Math.min(540, Number(args.height) || 360))
    const frame = new Uint8Array(4 + width * height * 4)
    const header = new DataView(frame.buffer)
    header.setUint16(0, width, true)
    header.setUint16(2, height, true)
    for (let y = 0; y < height; y += 1) {
      for (let x = 0; x < width; x += 1) {
        const offset = 4 + (y * width + x) * 4
        const panel = x > 36 && x < width - 36 && y > 36 && y < height - 36
        frame[offset] = panel ? 24 + Math.round(18 * x / width) : 8
        frame[offset + 1] = panel ? 73 + Math.round(34 * y / height) : 10
        frame[offset + 2] = panel ? 116 + Math.round(28 * x / width) : 14
        frame[offset + 3] = 255
      }
    }
    postSandboxChannel(args.onStatus, { type: 'connected', width, height })
    postSandboxChannel(args.onFrame, frame)
  }, 80)
  mockRdpTimers.set(sessionId, timer)
}

function stopMockRdp(sessionId: string, emitClosed = true) {
  const timer = mockRdpTimers.get(sessionId)
  if (timer !== undefined) window.clearTimeout(timer)
  mockRdpTimers.delete(sessionId)
  mockRdpStatusChannels.delete(sessionId)
  if (emitClosed) {
    // The real backend sends this through the session channel before dropping it.
  }
}

function postSandboxChannel(channel: unknown, message: unknown) {
  if (!channel || typeof channel !== 'object') return
  const callback = (channel as { onmessage?: (value: unknown) => void }).onmessage
  callback?.(message)
}

function mockDirectory(path: string, remote = false): MockFileEntry[] {
  const normalizedPath = path && path !== '~' ? path : remote ? '/root' : 'C:/Users/sandbox'
  const base = remote ? normalizedPath.replace(/\/$/, '') : normalizedPath.replace(/\\$/, '')
  const joiner = remote ? '/' : '/'
  const folders = ['.cache', '.config', '.ssh', '.vscode-server', 'KiroX_Cli_2', 'logs', 'output']
  const files = ['results.json', 'run.log', 'profile.sh', 'accounts.csv', 'notes.md']
  const stressCount = typeof window === 'undefined'
    ? 0
    : Math.min(10_000, Math.max(0, Number((window as typeof window & { __XUNDU_SANDBOX_DIRECTORY_SIZE__?: number }).__XUNDU_SANDBOX_DIRECTORY_SIZE__) || 0))
  if (stressCount > 0) {
    return Array.from({ length: stressCount }, (_, index) => {
      const name = `stress-${String(index).padStart(5, '0')}.log`
      return mockEntry(name, `${base}${joiner}${name}`, false, 1024 + index, index)
    })
  }
  return [
    ...folders.map((name, index) => mockEntry(name, `${base}${joiner}${name}`, true, 0, index)),
    ...files.map((name, index) => mockEntry(name, `${base}${joiner}${name}`, false, 1024 * (index + 3), index + folders.length)),
  ]
}

function mockEntry(name: string, path: string, isDir: boolean, size: number, index: number, fileType?: string): MockFileEntry {
  return {
    name,
    path,
    is_dir: isDir,
    size,
    modified: `07/${String(8 - (index % 6)).padStart(2, '0')} ${String(12 + index).padStart(2, '0')}:2${index % 10}`,
    permissions: isDir ? 'drwxr-xr-x' : '-rw-r--r--',
    file_type: fileType ?? (isDir ? '文件夹' : path.endsWith('.zip') ? 'ZIP' : '文件'),
  }
}

function mockFileContent(path: string, revision = 1) {
  if (path.endsWith('.json')) {
    return JSON.stringify({
      sandbox: true,
      path,
      revision,
      items: Array.from({ length: 8 }, (_, index) => ({ id: index + 1, status: 'ok' })),
    }, null, 2)
  }
  return `# Sandbox file\n\nPath: ${path}\n\n这里是前端沙盒模拟内容，不会读写真实服务器。`
}

function mockStats(host: string): MockStats {
  const pulse = Math.floor(Date.now() / 1000) % 10
  return {
    user: host === 'localhost' ? 'sandbox' : 'root',
    home_dir: host === 'localhost' ? 'C:/Users/sandbox' : '/root',
    os: host === 'localhost' ? 'windows' : 'linux',
    shell: host === 'localhost' ? 'cmd.exe' : '/bin/bash',
    process_count: 140 + pulse,
    cpu_usage: 2 + pulse,
    memory_used: (4 + pulse) * 1024 * 1024 * 1024,
    memory_total: 32 * 1024 * 1024 * 1024,
    disk_used: 92 * 1024 * 1024 * 1024,
    disk_total: 256 * 1024 * 1024 * 1024,
    network_received: 1024 * 1024 * pulse,
    network_transmitted: 512 * 1024 * pulse,
  }
}

function mockLatency(command: string) {
  if (command.includes('list_dir')) return 120
  if (command.includes('system_stats')) return 90
  if (command.includes('ssh_connect')) return 80
  return 12
}

function delay(ms: number) {
  return new Promise((resolve) => window.setTimeout(resolve, ms))
}
