import { Channel } from '@tauri-apps/api/core'
import { invoke, isSandboxMode } from './tauriBridge'

export type RemoteDesktopProtocol = 'rdp' | 'vnc'
export type RemoteDesktopSecurity = 'any' | 'nla' | 'tls'
export type RemoteDesktopConnection = {
  protocol: RemoteDesktopProtocol
  host: string
  port: number
  username: string
  password: string
  domain: string
  security: RemoteDesktopSecurity
  ignoreCertificate: boolean
  viewOnly: boolean
}
export type IronRdpStatusEvent =
  | { type: 'connecting' }
  | { type: 'retrying'; from: 'nla' | 'tls'; to: 'nla' | 'tls' }
  | { type: 'connected'; width: number; height: number }
  | { type: 'error'; code?: string; message: string }
  | { type: 'closed'; message: string }
export type IronRdpInput =
  | { type: 'mouseMove'; x: number; y: number }
  | { type: 'mouseButton'; button: number; down: boolean }
  | { type: 'wheel'; deltaX: number; deltaY: number }
  | { type: 'key'; code: number; extended: boolean; down: boolean }
  | { type: 'text'; text: string }
  | { type: 'resize'; width: number; height: number; scaleFactor: number }
  | { type: 'releaseAll' }
  | { type: 'ctrlAltDelete' }
type MessageChannel<T> = { onmessage: (message: T) => void }
type RawNativeRdpSession = { disconnect: () => Promise<void>; sendInput: (input: IronRdpInput) => Promise<void> }
export type NativeRdpSession = RawNativeRdpSession & { release: () => void }

type NativeRdpSessionOptions = {
  sessionId: string
  connection: RemoteDesktopConnection
  width: number
  height: number
  onStatus: (event: IronRdpStatusEvent) => void
  onFrame: (frame: ArrayBuffer) => void
}

type RdpSubscriber = Pick<NativeRdpSessionOptions, 'onStatus' | 'onFrame'>
type PooledRdpSession = {
  connectionKey: string
  raw?: Promise<RawNativeRdpSession>
  subscribers: Set<RdpSubscriber>
  references: number
  status?: IronRdpStatusEvent
  frame?: ArrayBuffer
  releaseTimer?: number
  disposed: boolean
}

const nativeRdpSessions = new Map<string, PooledRdpSession>()
const RDP_LAYOUT_REMOUNT_GRACE_MS = 2_500

async function startNativeRdpSession({ sessionId, connection, width, height, onStatus, onFrame }: NativeRdpSessionOptions): Promise<RawNativeRdpSession> {
  if (connection.protocol !== 'rdp') throw new Error('当前内置引擎仅支持 RDP，VNC 尚未接入')
  const attempts: Array<'nla' | 'tls'> = connection.security === 'any'
    ? ['nla', 'tls']
    : [connection.security]
  const retainedChannels: unknown[] = []
  let disposed = false
  let connected = false
  let attemptVersion = 0

  const startAttempt = async (index: number): Promise<void> => {
    const security = attempts[index]
    const version = ++attemptVersion
    const statusChannel = createMessageChannel((event: Exclude<IronRdpStatusEvent, { type: 'retrying' }>) => {
      if (disposed || version !== attemptVersion) return
      if (event.type === 'connecting') {
        if (index === 0) onStatus(event)
        return
      }
      if (event.type === 'connected') {
        connected = true
        onStatus(event)
        return
      }
      if (!connected && (event.type === 'error' || event.type === 'closed') && index + 1 < attempts.length && isRetryableStartupFailure(event)) {
        const nextSecurity = attempts[index + 1]
        onStatus({ type: 'retrying', from: security, to: nextSecurity })
        attemptVersion += 1
        void invoke('rdp_disconnect', { sessionId })
          .catch(() => undefined)
          .then(() => startAttempt(index + 1))
          .catch((error) => onStatus({ type: 'error', code: 'rdp_failed', message: String(error) }))
        return
      }
      if (!connected && event.type === 'closed') {
        onStatus({ type: 'error', code: 'remote_closed_during_startup', message: event.message })
        return
      }
      onStatus(event)
    })
    const frameChannel = createMessageChannel((frame: ArrayBuffer | Uint8Array | number[]) => {
      if (disposed || version !== attemptVersion) return
      if (frame instanceof ArrayBuffer) return onFrame(frame)
      const bytes = frame instanceof Uint8Array ? frame : Uint8Array.from(frame)
      onFrame(bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer)
    })
    retainedChannels.push(statusChannel, frameChannel)
    await invoke('rdp_connect', {
      sessionId,
      host: connection.host,
      port: connection.port,
      username: connection.username,
      password: connection.password,
      domain: connection.domain,
      security,
      ignoreCertificate: connection.ignoreCertificate,
      width: clamp(width, 320, 2560),
      height: clamp(height, 200, 1600),
      onStatus: statusChannel,
      onFrame: frameChannel,
    })
  }

  await startAttempt(0)
  return {
    disconnect: () => {
      disposed = true
      attemptVersion += 1
      retainedChannels.length = 0
      return invoke('rdp_disconnect', { sessionId })
    },
    sendInput: (input) => invoke('rdp_input', { sessionId, input }),
  }
}

export async function createNativeRdpSession(options: NativeRdpSessionOptions): Promise<NativeRdpSession> {
  const { sessionId, connection, onStatus, onFrame } = options
  const connectionKey = JSON.stringify([
    connection.protocol,
    connection.host,
    connection.port,
    connection.username,
    connection.password,
    connection.domain,
    connection.security,
  ])
  const subscriber = { onStatus, onFrame }
  let pooled = nativeRdpSessions.get(sessionId)
  const reusable = pooled
    && !pooled.disposed
    && pooled.connectionKey === connectionKey
    && pooled.status?.type !== 'error'
    && pooled.status?.type !== 'closed'

  if (pooled && !reusable) {
    await disposePooledRdpSession(sessionId, pooled)
    pooled = undefined
  }

  const reused = Boolean(pooled)
  if (!pooled) {
    pooled = {
      connectionKey,
      subscribers: new Set([subscriber]),
      references: 0,
      status: { type: 'connecting' },
      disposed: false,
    }
    nativeRdpSessions.set(sessionId, pooled)
    const current = pooled
    current.raw = startNativeRdpSession({
      ...options,
      onStatus: (event) => {
        if (current.disposed) return
        current.status = event
        current.subscribers.forEach((listener) => listener.onStatus(event))
      },
      onFrame: (frame) => {
        if (current.disposed) return
        current.frame = frame
        current.subscribers.forEach((listener) => listener.onFrame(frame))
      },
    })
  } else {
    pooled.subscribers.add(subscriber)
  }

  if (pooled.releaseTimer !== undefined) {
    window.clearTimeout(pooled.releaseTimer)
    pooled.releaseTimer = undefined
  }
  pooled.references += 1

  if (reused) {
    if (pooled.status) onStatus(pooled.status)
    if (pooled.frame) onFrame(pooled.frame)
  }

  let raw: RawNativeRdpSession
  try {
    raw = await pooled.raw!
  } catch (error) {
    pooled.subscribers.delete(subscriber)
    pooled.references = Math.max(0, pooled.references - 1)
    if (nativeRdpSessions.get(sessionId) === pooled) nativeRdpSessions.delete(sessionId)
    pooled.disposed = true
    throw error
  }

  let released = false
  const detach = () => {
    if (released) return false
    released = true
    pooled!.subscribers.delete(subscriber)
    pooled!.references = Math.max(0, pooled!.references - 1)
    return true
  }

  return {
    sendInput: (input) => raw.sendInput(input),
    disconnect: async () => {
      detach()
      await disposePooledRdpSession(sessionId, pooled!)
    },
    release: () => {
      if (!detach() || pooled!.references > 0 || pooled!.disposed) return
      pooled!.releaseTimer = window.setTimeout(() => {
        if (pooled!.references === 0) void disposePooledRdpSession(sessionId, pooled!)
      }, RDP_LAYOUT_REMOUNT_GRACE_MS)
    },
  }
}

async function disposePooledRdpSession(sessionId: string, pooled: PooledRdpSession) {
  if (pooled.disposed) return
  pooled.disposed = true
  if (pooled.releaseTimer !== undefined) window.clearTimeout(pooled.releaseTimer)
  pooled.subscribers.clear()
  if (nativeRdpSessions.get(sessionId) === pooled) nativeRdpSessions.delete(sessionId)
  try {
    await (await pooled.raw)?.disconnect()
  } catch {
    // The backend may already have removed a failed or closed session.
  }
}

function isRetryableStartupFailure(event: { type: 'error'; code?: string; message: string } | { type: 'closed'; message: string }) {
  if (event.type === 'closed') return true
  if (event.code === 'authentication_failed' || event.code === 'connection_refused') return false
  if (event.code === 'timeout' || event.code === 'remote_closed_during_startup' || event.code === 'rdp_failed' || event.code === 'rdp_worker_stopped') return true
  const message = event.message.toLowerCase()
  return message.includes('unexpected eof') || message.includes('close_notify') || message.includes('closed')
}

export function createMessageChannel<T>(onmessage: (message: T) => void): Channel<T> | MessageChannel<T> {
  return isSandboxMode ? { onmessage } : new Channel<T>(onmessage)
}
function clamp(value: number, minimum: number, maximum: number) {
  return Number.isFinite(value) ? Math.min(maximum, Math.max(minimum, Math.round(value))) : minimum
}

const SCANCODES: Record<string, [number, boolean]> = {
  Escape:[0x01,false], Digit1:[0x02,false], Digit2:[0x03,false], Digit3:[0x04,false], Digit4:[0x05,false], Digit5:[0x06,false], Digit6:[0x07,false], Digit7:[0x08,false], Digit8:[0x09,false], Digit9:[0x0a,false], Digit0:[0x0b,false], Minus:[0x0c,false], Equal:[0x0d,false], Backspace:[0x0e,false], Tab:[0x0f,false],
  KeyQ:[0x10,false], KeyW:[0x11,false], KeyE:[0x12,false], KeyR:[0x13,false], KeyT:[0x14,false], KeyY:[0x15,false], KeyU:[0x16,false], KeyI:[0x17,false], KeyO:[0x18,false], KeyP:[0x19,false], BracketLeft:[0x1a,false], BracketRight:[0x1b,false], Enter:[0x1c,false], ControlLeft:[0x1d,false],
  KeyA:[0x1e,false], KeyS:[0x1f,false], KeyD:[0x20,false], KeyF:[0x21,false], KeyG:[0x22,false], KeyH:[0x23,false], KeyJ:[0x24,false], KeyK:[0x25,false], KeyL:[0x26,false], Semicolon:[0x27,false], Quote:[0x28,false], Backquote:[0x29,false], ShiftLeft:[0x2a,false], Backslash:[0x2b,false],
  KeyZ:[0x2c,false], KeyX:[0x2d,false], KeyC:[0x2e,false], KeyV:[0x2f,false], KeyB:[0x30,false], KeyN:[0x31,false], KeyM:[0x32,false], Comma:[0x33,false], Period:[0x34,false], Slash:[0x35,false], ShiftRight:[0x36,false], NumpadMultiply:[0x37,false], AltLeft:[0x38,false], Space:[0x39,false], CapsLock:[0x3a,false],
  F1:[0x3b,false], F2:[0x3c,false], F3:[0x3d,false], F4:[0x3e,false], F5:[0x3f,false], F6:[0x40,false], F7:[0x41,false], F8:[0x42,false], F9:[0x43,false], F10:[0x44,false], NumLock:[0x45,false], ScrollLock:[0x46,false], Numpad7:[0x47,false], Numpad8:[0x48,false], Numpad9:[0x49,false], NumpadSubtract:[0x4a,false], Numpad4:[0x4b,false], Numpad5:[0x4c,false], Numpad6:[0x4d,false], NumpadAdd:[0x4e,false], Numpad1:[0x4f,false], Numpad2:[0x50,false], Numpad3:[0x51,false], Numpad0:[0x52,false], NumpadDecimal:[0x53,false], F11:[0x57,false], F12:[0x58,false],
  NumpadEnter:[0x1c,true], ControlRight:[0x1d,true], NumpadDivide:[0x35,true], AltRight:[0x38,true], Home:[0x47,true], ArrowUp:[0x48,true], PageUp:[0x49,true], ArrowLeft:[0x4b,true], ArrowRight:[0x4d,true], End:[0x4f,true], ArrowDown:[0x50,true], PageDown:[0x51,true], Insert:[0x52,true], Delete:[0x53,true], MetaLeft:[0x5b,true], MetaRight:[0x5c,true], ContextMenu:[0x5d,true],
}
export function keyboardCodeToScancode(code: string) {
  const mapping = SCANCODES[code]
  return mapping ? { code: mapping[0], extended: mapping[1] } : null
}
