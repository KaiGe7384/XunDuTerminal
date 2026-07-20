import { useCallback, useEffect, useRef, useState } from 'react'
import type { KeyboardEvent, PointerEvent } from 'react'
import { ClipboardList, Edit3, KeyRound, Monitor, Unplug, Upload, X } from 'lucide-react'
import {
  createNativeRdpSession,
  keyboardCodeToScancode,
  type IronRdpInput,
  type NativeRdpSession,
  type RemoteDesktopConnection,
} from './ironRdpBridge'
import { getCurrentWindow, invoke } from './tauriBridge'
import { upsertTransfer } from './operationsStore'

type ConnectionState = 'disconnected' | 'ready' | 'connecting' | 'connected' | 'error'
type RemoteDesktopStatus = 'disconnected' | 'connecting' | 'connected' | 'error'

type RdpFileTransferProgress = {
  totalBytes: number
  transferredBytes: number
  bytesPerSecond: number
  copiedFiles: number
  totalFiles: number
  currentFile: string
  completed: boolean
}

type RdpClipboardFileOffer = {
  totalFiles: number
  totalBytes: number
}

type RdpNativeClipboardProgress = Omit<RdpFileTransferProgress, 'bytesPerSecond' | 'copiedFiles'> & {
  accepted: boolean | null
}

const RDP_FIXED_RESOLUTION_HOSTS_KEY = 'xundu.rdp.fixedResolutionHosts.v1'
const RDP_RESIZE_DEBOUNCE_MS = 420

type Props = {
  widgetId: string
  sessionId: string
  connection?: RemoteDesktopConnection
  active: boolean
  autoConnect: boolean
  t: (text: string) => string
  onAutoConnectChange: (enabled: boolean) => void
  onSaveConnection: (connection: RemoteDesktopConnection) => void
  onStatusChange: (state: ConnectionState) => void
}

export default function RemoteDesktopWidget({
  widgetId,
  sessionId,
  connection,
  active,
  autoConnect,
  t,
  onAutoConnectChange,
  onSaveConnection,
  onStatusChange,
}: Props) {
  const [draft, setDraft] = useState<RemoteDesktopConnection>(() => connection ?? createDefaultConnection())
  const [editDraft, setEditDraft] = useState<RemoteDesktopConnection>(() => connection ?? createDefaultConnection())
  const [editing, setEditing] = useState(!connection || connection.protocol !== 'rdp')
  const [editError, setEditError] = useState('')
  const [status, setStatus] = useState<RemoteDesktopStatus>('disconnected')
  const [message, setMessage] = useState(connection?.protocol === 'vnc' ? t('当前内置引擎仅支持 RDP') : '')
  const [fileDragActive, setFileDragActive] = useState(false)
  const [fileTransferActive, setFileTransferActive] = useState(false)
  const [fileTransferProgress, setFileTransferProgress] = useState<RdpFileTransferProgress | null>(null)
  const viewportRef = useRef<HTMLDivElement | null>(null)
  const canvasRef = useRef<HTMLCanvasElement | null>(null)
  const sessionRef = useRef<NativeRdpSession | null>(null)
  const generationRef = useRef(0)
  const remoteSizeRef = useRef({ width: 1, height: 1 })
  const mouseFrameRef = useRef<number | null>(null)
  const pendingMouseRef = useRef<{ x: number; y: number } | null>(null)
  const resizeTimerRef = useRef<number | null>(null)
  const lastResizeRef = useRef({ width: 0, height: 0 })
  const pendingResizeRef = useRef<{ width: number; height: number } | null>(null)
  const resizeInFlightRef = useRef(false)
  const recoveryScheduledRef = useRef(false)
  const connectRef = useRef<((candidate?: RemoteDesktopConnection) => Promise<void>) | null>(null)
  const connectingRef = useRef(false)
  const fileTransferClearTimerRef = useRef<number | null>(null)
  const fileTransferRef = useRef<{ id: string; cancelled: boolean } | null>(null)
  const lastClipboardSequenceRef = useRef(0)
  const lastOfferedFilesRef = useRef('')

  useEffect(() => {
    if (!connection) return
    if (connection.protocol === 'vnc') {
      const migrated = { ...connection, protocol: 'rdp' as const, port: 3389 }
      setDraft(migrated)
      setEditDraft(migrated)
      setEditing(true)
      setMessage(t('VNC 尚未接入内置引擎，请改用 RDP'))
      return
    }
    setDraft(connection)
    if (!editing) setEditDraft(connection)
  }, [connection, editing, t])

  useEffect(() => () => {
    generationRef.current += 1
    stopRendering()
    sessionRef.current?.release()
    sessionRef.current = null
    const transfer = fileTransferRef.current
    if (transfer) {
      transfer.cancelled = true
      void invoke('rdp_cancel_file_transfer', { transferId: transfer.id }).catch(() => undefined)
      fileTransferRef.current = null
    }
    if (fileTransferClearTimerRef.current !== null) window.clearTimeout(fileTransferClearTimerRef.current)
  }, [])

  useEffect(() => {
    if (!connection || connection.protocol !== 'rdp' || !autoConnect) return
    const frame = window.requestAnimationFrame(() => {
      window.requestAnimationFrame(() => void connectRef.current?.(connection))
    })
    return () => window.cancelAnimationFrame(frame)
  }, [autoConnect, connection, sessionId])

  useEffect(() => {
    if (status !== 'connected') return
    const viewport = viewportRef.current
    if (!viewport) return

    if (usesFixedResolution({ host: draft.host, port: draft.port })) {
      pendingResizeRef.current = null
      return
    }

    let disposed = false

    const flushPendingResize = async () => {
      if (resizeInFlightRef.current) return
      resizeInFlightRef.current = true
      try {
        while (!disposed && pendingResizeRef.current) {
          const next = pendingResizeRef.current
          pendingResizeRef.current = null
          const session = sessionRef.current
          if (!session) return
          await session.sendInput({ type: 'resize', ...next, scaleFactor: 100 })
          lastResizeRef.current = next
        }
      } catch (error) {
        if (!disposed) setMessage(String(error))
      } finally {
        resizeInFlightRef.current = false
        if (!disposed && pendingResizeRef.current) void flushPendingResize()
      }
    }

    const syncSize = () => {
      resizeTimerRef.current = null
      const rect = viewport.getBoundingClientRect()
      const width = Math.max(320, Math.min(2560, Math.round(rect.width)))
      const height = Math.max(200, Math.min(1600, Math.round(rect.height)))
      if (rect.width < 2 || rect.height < 2) return
      const previous = lastResizeRef.current
      if (Math.abs(previous.width - width) < 8 && Math.abs(previous.height - height) < 8) return
      // Shrinking the remote framebuffer is unnecessary because the canvas can scale it down.
      // Some RDP servers emit malformed bitmap updates while shrinking, so only negotiate a
      // larger framebuffer and keep the last high-quality size for split layouts.
      if (width + 8 < previous.width || height + 8 < previous.height) return
      pendingResizeRef.current = { width, height }
      void flushPendingResize()
    }

    const scheduleSizeSync = () => {
      if (resizeTimerRef.current !== null) window.clearTimeout(resizeTimerRef.current)
      resizeTimerRef.current = window.setTimeout(syncSize, RDP_RESIZE_DEBOUNCE_MS)
    }
    const observer = new ResizeObserver(scheduleSizeSync)
    observer.observe(viewport)
    window.addEventListener('xundu:workbench-layout-settled', scheduleSizeSync)
    scheduleSizeSync()
    return () => {
      disposed = true
      observer.disconnect()
      window.removeEventListener('xundu:workbench-layout-settled', scheduleSizeSync)
      if (resizeTimerRef.current !== null) window.clearTimeout(resizeTimerRef.current)
      resizeTimerRef.current = null
      pendingResizeRef.current = null
    }
  }, [draft.host, draft.port, status, sessionId])

  useEffect(() => {
    if (status !== 'connected' || draft.viewOnly) return
    let disposed = false
    let syncing = false
    const syncFileClipboard = async () => {
      if (disposed || syncing || document.hidden || fileTransferRef.current) return
      syncing = true
      try {
        const sequence = await invoke<number>('rdp_clipboard_sequence_number')
        if (!sequence || sequence === lastClipboardSequenceRef.current) return
        const paths = await invoke<string[]>('rdp_clipboard_file_paths')
        const signature = paths.join('\u0000')
        if (!paths.length) {
          lastClipboardSequenceRef.current = sequence
          lastOfferedFilesRef.current = ''
          return
        }
        if (signature === lastOfferedFilesRef.current) return
        await offerRdpClipboardFiles(sessionId, paths, 1)
        if (!disposed) {
          lastClipboardSequenceRef.current = sequence
          lastOfferedFilesRef.current = signature
          setMessage(t('本地文件已同步到远程剪贴板'))
        }
      } catch {
        // Clipboard access is best-effort; the explicit paste action reports errors.
      } finally {
        syncing = false
      }
    }
    void syncFileClipboard()
    const timer = window.setInterval(() => void syncFileClipboard(), 350)
    return () => {
      disposed = true
      window.clearInterval(timer)
    }
  }, [draft.viewOnly, sessionId, status, t])

  const stopRendering = () => {
    if (mouseFrameRef.current !== null) window.cancelAnimationFrame(mouseFrameRef.current)
    mouseFrameRef.current = null
    pendingMouseRef.current = null
    if (resizeTimerRef.current !== null) window.clearTimeout(resizeTimerRef.current)
    resizeTimerRef.current = null
    pendingResizeRef.current = null
  }

  const cancelFileTransfer = useCallback((notify = true) => {
    const transfer = fileTransferRef.current
    if (!transfer || transfer.cancelled) return
    transfer.cancelled = true
    upsertTransfer({ id: transfer.id, bytesPerSecond: 0, message: t('正在取消文件传输') })
    if (notify) setMessage(t('正在取消文件传输'))
    void invoke('rdp_cancel_file_transfer', { transferId: transfer.id }).catch((error) => {
      if (notify) setMessage(`${t('取消文件传输失败')}: ${String(error)}`)
    })
  }, [t])

  const disconnect = (nextMessage = '', manual = false) => {
    cancelFileTransfer(false)
    if (manual) onAutoConnectChange(false)
    connectingRef.current = false
    generationRef.current += 1
    stopRendering()
    const session = sessionRef.current
    sessionRef.current = null
    if (session) {
      void session.sendInput({ type: 'releaseAll' }).catch(() => undefined)
      void session.disconnect().catch(() => undefined)
    }
    setStatus('disconnected')
    onStatusChange('ready')
    setMessage(nextMessage)
  }

  const normalizedEditDraft = (): RemoteDesktopConnection | null => {
    const host = editDraft.host.trim()
    const port = Math.round(Number(editDraft.port))
    if (!host || port < 1 || port > 65535 || !editDraft.username.trim() || !editDraft.password) {
      setEditError(t('请完整填写主机、端口、用户名和密码'))
      return null
    }
    return {
      ...editDraft,
      protocol: 'rdp',
      host,
      port,
      username: editDraft.username.trim(),
      domain: editDraft.domain.trim(),
      ignoreCertificate: true,
    }
  }

  const drawFrame = (frame: ArrayBuffer, generation: number) => {
    if (generationRef.current !== generation || frame.byteLength < 4) return
    const view = new DataView(frame)
    const width = view.getUint16(0, true)
    const height = view.getUint16(2, true)
    if (!width || !height || frame.byteLength !== 4 + width * height * 4) return
    const canvas = canvasRef.current
    const context = canvas?.getContext('2d', { alpha: false })
    if (!canvas || !context) return
    if (canvas.width !== width || canvas.height !== height) {
      canvas.width = width
      canvas.height = height
    }
    remoteSizeRef.current = { width, height }
    context.putImageData(new ImageData(new Uint8ClampedArray(frame, 4), width, height), 0, 0)
  }

  const connect = async (candidate?: RemoteDesktopConnection) => {
    if (connectingRef.current || sessionRef.current) return
    const nextConnection = candidate ?? normalizedEditDraft()
    if (!nextConnection) return
    if (nextConnection.protocol !== 'rdp') {
      setEditing(true)
      setMessage(t('当前内置引擎仅支持 RDP'))
      return
    }
    if (status === 'error') recoveryScheduledRef.current = false

    onSaveConnection(nextConnection)
    setDraft(nextConnection)
    setEditing(false)
    disconnect()
    connectingRef.current = true
    const generation = ++generationRef.current
    const viewport = viewportRef.current
    if (!viewport || !canvasRef.current) {
      setStatus('error')
      onStatusChange('error')
      setMessage(t('远程桌面画布尚未就绪'))
      connectingRef.current = false
      return
    }

    setStatus('connecting')
    onStatusChange('connecting')
    setMessage(t('正在启动内置 RDP 引擎'))
    let reachedConnected = false
    try {
      const session = await createNativeRdpSession({
        sessionId,
        connection: nextConnection,
        width: Math.max(320, Math.floor(viewport.clientWidth)),
        height: Math.max(200, Math.floor(viewport.clientHeight)),
        onStatus: (event) => {
          if (generationRef.current !== generation) return
          if (event.type === 'connecting') {
            setStatus('connecting')
            setMessage(t('正在建立 RDP 会话'))
          } else if (event.type === 'retrying') {
            setStatus('connecting')
            setMessage(t('NLA 连接未完成，正在自动尝试 TLS'))
          } else if (event.type === 'connected') {
            reachedConnected = true
            connectingRef.current = false
            remoteSizeRef.current = { width: event.width, height: event.height }
            lastResizeRef.current = { width: event.width, height: event.height }
            onAutoConnectChange(true)
            setStatus('connected')
            onStatusChange('connected')
            setMessage(usesFixedResolution(nextConnection)
              ? t('已启用固定分辨率兼容模式')
              : t('已通过内置 RDP 引擎连接'))
            window.requestAnimationFrame(() => canvasRef.current?.focus({ preventScroll: true }))
          } else if (event.type === 'error') {
            connectingRef.current = false
            const failedSession = sessionRef.current
            sessionRef.current = null
            if (event.code === 'frame_decode_failed' && reachedConnected && !recoveryScheduledRef.current) {
              recoveryScheduledRef.current = true
              rememberFixedResolution(nextConnection)
              setStatus('connecting')
              onStatusChange('connecting')
              setMessage(t('服务器画面自适应不兼容，正在以固定分辨率恢复'))
              void Promise.resolve(failedSession?.disconnect()).catch(() => undefined).then(async () => {
                await delay(180)
                if (generationRef.current !== generation) return
                await connectRef.current?.(nextConnection)
              })
              return
            }
            void failedSession?.disconnect().catch(() => undefined)
            setStatus('error')
            onStatusChange('error')
            setMessage(localizeRdpError(event.message, t, event.code))
          } else {
            connectingRef.current = false
            sessionRef.current = null
            setStatus('disconnected')
            onStatusChange('ready')
            setMessage(event.message || t('远程桌面已断开'))
          }
        },
        onFrame: (frame) => drawFrame(frame, generation),
      })
      if (generationRef.current !== generation) {
        await session.disconnect()
        return
      }
      sessionRef.current = session
    } catch (error) {
      if (generationRef.current !== generation) return
      connectingRef.current = false
      setStatus('error')
      onStatusChange('error')
      setMessage(localizeRdpError(String(error), t))
      return
    }

  }
  connectRef.current = connect

  const beginEditing = () => {
    setEditDraft(draft)
    setEditError('')
    setEditing(true)
  }

  const cancelEditing = () => {
    setEditDraft(draft)
    setEditError('')
    setEditing(false)
    window.requestAnimationFrame(() => canvasRef.current?.focus({ preventScroll: true }))
  }

  const saveEditing = () => {
    const normalized = normalizedEditDraft()
    if (!normalized) return
    const needsReconnect = !sameRemoteDesktopConnection(draft, normalized)
    const hasLiveSession = status === 'connected' || status === 'connecting'
    onSaveConnection(normalized)
    setDraft(normalized)
    setEditDraft(normalized)
    setEditError('')
    setEditing(false)
    if (hasLiveSession && !needsReconnect) {
      window.requestAnimationFrame(() => canvasRef.current?.focus({ preventScroll: true }))
      return
    }
    if (hasLiveSession) disconnect()
    window.requestAnimationFrame(() => window.requestAnimationFrame(() => void connectRef.current?.(normalized)))
  }

  const sendInput = (input: IronRdpInput) => {
    if (draft.viewOnly || status !== 'connected') return
    void sessionRef.current?.sendInput(input).catch((error) => setMessage(String(error)))
  }

  const pointerPosition = (event: PointerEvent<HTMLCanvasElement>) => {
    const rect = event.currentTarget.getBoundingClientRect()
    const remote = remoteSizeRef.current
    return {
      x: Math.max(0, Math.min(remote.width - 1, Math.round((event.clientX - rect.left) / rect.width * remote.width))),
      y: Math.max(0, Math.min(remote.height - 1, Math.round((event.clientY - rect.top) / rect.height * remote.height))),
    }
  }

  const sendRemoteText = async (text: string) => {
    const session = sessionRef.current
    if (!text || !session || status !== 'connected' || draft.viewOnly) return false
    await session.sendInput({ type: 'releaseAll' })
    await session.sendInput({ type: 'text', text })
    canvasRef.current?.focus({ preventScroll: true })
    return true
  }

  const sendRemotePasteShortcut = useCallback(async () => {
    const session = sessionRef.current
    const control = keyboardCodeToScancode('ControlLeft')
    const paste = keyboardCodeToScancode('KeyV')
    if (!session || !control || !paste || status !== 'connected' || draft.viewOnly) return false
    await session.sendInput({ type: 'releaseAll' })
    await session.sendInput({ type: 'key', ...control, down: true })
    await session.sendInput({ type: 'key', ...paste, down: true })
    await session.sendInput({ type: 'key', ...paste, down: false })
    await session.sendInput({ type: 'key', ...control, down: false })
    canvasRef.current?.focus({ preventScroll: true })
    return true
  }, [draft.viewOnly, status])

  const transferRemoteFiles = useCallback(async (paths: string[]) => {
    if (!paths.length || status !== 'connected' || draft.viewOnly || fileTransferRef.current) return
    const transferId = crypto.randomUUID()
    const transfer = { id: transferId, cancelled: false }
    fileTransferRef.current = transfer
    upsertTransfer({
      id: transferId,
      protocol: 'rdp',
      direction: 'upload',
      title: paths.length === 1 ? fileName(paths[0]) : `${paths.length} ${t('个文件')}`,
      source: paths.join('; '),
      destination: `${draft.host}:${draft.port}`,
      status: 'running',
      totalBytes: 0,
      transferredBytes: 0,
      bytesPerSecond: 0,
      copiedFiles: 0,
      totalFiles: paths.length,
      currentFile: '',
      message: t('正在传输文件到远程桌面'),
      resumable: false,
    }, {
      cancel: () => cancelFileTransfer(),
      retry: () => transferRemoteFiles(paths),
    })
    if (fileTransferClearTimerRef.current !== null) window.clearTimeout(fileTransferClearTimerRef.current)
    setFileTransferActive(true)
    setFileTransferProgress({
      totalBytes: 0,
      transferredBytes: 0,
      bytesPerSecond: 0,
      copiedFiles: 0,
      totalFiles: 0,
      currentFile: '',
      completed: false,
    })
    setMessage(t('正在传输文件到远程桌面'))
    try {
      const offer = await offerRdpClipboardFiles(sessionId, paths, 30)
      lastOfferedFilesRef.current = paths.join('\u0000')
      setFileTransferProgress({
        totalBytes: offer.totalBytes,
        transferredBytes: 0,
        bytesPerSecond: 0,
        copiedFiles: 0,
        totalFiles: offer.totalFiles,
        currentFile: '',
        completed: false,
      })
      upsertTransfer({
        id: transferId,
        totalBytes: offer.totalBytes,
        totalFiles: offer.totalFiles,
        message: t('正在等待远程桌面接收文件'),
      })
      setMessage(t('正在等待远程桌面接收文件'))
      const negotiationDeadline = performance.now() + 5_000
      while (!transfer.cancelled) {
        const negotiation = await invoke<RdpNativeClipboardProgress>('rdp_file_clipboard_progress')
        if (negotiation.accepted === true) break
        if (negotiation.accepted === false) throw new Error(t('远程桌面拒绝了文件剪贴板格式'))
        if (performance.now() >= negotiationDeadline) {
          throw new Error(t('远程桌面未确认文件剪贴板，请检查剪贴板重定向策略'))
        }
        await delay(120)
      }
      if (transfer.cancelled) throw new Error('文件传输已取消')
      if (!await sendRemotePasteShortcut()) throw new Error(t('RDP 会话当前无法接收文件'))

      const startedAt = performance.now()
      let lastSampleAt = startedAt
      let lastBytes = 0
      let transferStarted = offer.totalBytes === 0
      while (!transfer.cancelled) {
        await delay(250)
        const progress = await invoke<RdpNativeClipboardProgress>('rdp_file_clipboard_progress')
        const now = performance.now()
        const elapsedSeconds = Math.max(0.001, (now - lastSampleAt) / 1000)
        const bytesPerSecond = Math.max(0, Math.round((progress.transferredBytes - lastBytes) / elapsedSeconds))
        if (progress.transferredBytes > 0) transferStarted = true
        setFileTransferProgress({
          totalBytes: progress.totalBytes,
          transferredBytes: progress.transferredBytes,
          totalFiles: progress.totalFiles,
          currentFile: progress.currentFile,
          completed: progress.completed,
          bytesPerSecond,
          copiedFiles: progress.completed ? progress.totalFiles : 0,
        })
        upsertTransfer({
          id: transferId,
          status: progress.completed ? 'completed' : 'running',
          totalBytes: progress.totalBytes,
          transferredBytes: progress.transferredBytes,
          totalFiles: progress.totalFiles,
          copiedFiles: progress.completed ? progress.totalFiles : 0,
          currentFile: progress.currentFile,
          bytesPerSecond,
          message: progress.completed ? t('文件已传输到远程桌面') : t('正在传输文件到远程桌面'),
        })
        lastSampleAt = now
        lastBytes = progress.transferredBytes
        if (progress.completed) break
        if (!transferStarted && now - startedAt > 12_000) {
          throw new Error(t('远程桌面未请求文件，请检查剪贴板重定向策略'))
        }
        if (now - startedAt > 30 * 60_000) throw new Error(t('文件传输超时'))
      }
      if (transfer.cancelled) throw new Error('文件传输已取消')
      upsertTransfer({
        id: transferId,
        status: 'completed',
        transferredBytes: offer.totalBytes,
        totalBytes: offer.totalBytes,
        copiedFiles: offer.totalFiles,
        totalFiles: offer.totalFiles,
        currentFile: '',
        bytesPerSecond: 0,
        message: t('文件已传输到远程桌面'),
      })
      setMessage(`${t('文件已传输到远程桌面')} · ${offer.totalFiles}`)
      fileTransferClearTimerRef.current = window.setTimeout(() => {
        setFileTransferProgress(null)
        fileTransferClearTimerRef.current = null
      }, 1800)
    } catch (error) {
      setFileTransferProgress(null)
      const detail = String(error).replace(/^Error:\s*/i, '')
      upsertTransfer({
        id: transferId,
        status: transfer.cancelled || detail.includes('文件传输已取消') ? 'cancelled' : 'error',
        currentFile: '',
        bytesPerSecond: 0,
        message: transfer.cancelled || detail.includes('文件传输已取消')
          ? t('文件传输已取消')
          : `${t('文件传输失败')}: ${detail}`,
      })
      setMessage(transfer.cancelled || detail.includes('文件传输已取消')
        ? t('文件传输已取消')
        : `${t('文件传输失败')}: ${detail}`)
    } finally {
      if (fileTransferRef.current?.id === transferId) {
        fileTransferRef.current = null
        setFileTransferActive(false)
        setFileDragActive(false)
      }
    }
  }, [cancelFileTransfer, draft.host, draft.port, draft.viewOnly, sendRemotePasteShortcut, sessionId, status, t])

  const pasteRemoteClipboard = async () => {
    if (status !== 'connected' || draft.viewOnly) return
    try {
      const paths = await invoke<string[]>('rdp_clipboard_file_paths')
      if (paths.length > 0) {
        await transferRemoteFiles(paths)
        return
      }
    } catch {
      // File clipboard access may be unavailable; continue with text clipboard paste.
    }
    try {
      const text = await navigator.clipboard.readText()
      if (!text) {
        setMessage(t('剪贴板中没有可粘贴的文本'))
        return
      }
      if (await sendRemoteText(text)) setMessage(t('剪贴板文本已粘贴到远程'))
    } catch (error) {
      setMessage(`${t('剪贴板同步失败')}: ${String(error)}`)
    }
  }

  const sendKeyboardEvent = (event: KeyboardEvent<HTMLCanvasElement>, down: boolean) => {
    if (event.nativeEvent.isComposing || event.key === 'Process') return
    if (event.code === 'KeyV' && (event.ctrlKey || event.metaKey)) {
      event.preventDefault()
      event.stopPropagation()
      if (down) void pasteRemoteClipboard()
      return
    }
    const scancode = keyboardCodeToScancode(event.code)
    if (!scancode) return
    event.preventDefault()
    event.stopPropagation()
    sendInput({ type: 'key', ...scancode, down })
  }

  const pasteDroppedText = async (text: string) => {
    if (!text || status !== 'connected' || draft.viewOnly) return
    try {
      if (await sendRemoteText(text)) setMessage(t('拖放文本已粘贴到远程'))
    } catch (error) {
      setMessage(`${t('剪贴板同步失败')}: ${String(error)}`)
    }
  }

  useEffect(() => {
    let disposed = false
    let unlisten: (() => void) | undefined
    const appWindow = getCurrentWindow()
    void appWindow.scaleFactor().then((scaleFactor) => appWindow.onDragDropEvent((event: {
      payload: { type: string; paths?: string[]; position?: { x: number; y: number } }
    }) => {
      if (disposed) return
      const payload = event.payload
      if (payload.type === 'leave') {
        setFileDragActive(false)
        return
      }
      const position = payload.position
      const viewport = viewportRef.current
      if (!position || !viewport) return
      const rect = viewport.getBoundingClientRect()
      const x = position.x / Math.max(1, scaleFactor)
      const y = position.y / Math.max(1, scaleFactor)
      const inside = x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom
      if (payload.type === 'enter' || payload.type === 'over') {
        setFileDragActive(inside && status === 'connected' && !draft.viewOnly)
        return
      }
      if (payload.type === 'drop') {
        setFileDragActive(false)
        if (inside && payload.paths?.length) void transferRemoteFiles(payload.paths)
      }
    })).then((cleanup) => {
      if (disposed) cleanup()
      else unlisten = cleanup
    }).catch(() => undefined)
    return () => {
      disposed = true
      unlisten?.()
    }
  }, [draft.viewOnly, status, transferRemoteFiles])

  return (
    <div className={`remote-desktop-shell ${active ? 'active' : ''}`} data-status={status} data-remote-desktop-widget-id={widgetId}>
      <div className="remote-desktop-controls">
        <span className={`connection-dot ${status}`} />
        <strong>RDP</strong><span>{draft.host}:{draft.port}</span>
        <button type="button" onClick={() => void pasteRemoteClipboard()} disabled={status !== 'connected' || draft.viewOnly} title={t('粘贴本地剪贴板到远程')}><ClipboardList size={13} /></button>
        <button type="button" onClick={() => sendInput({ type: 'ctrlAltDelete' })} disabled={status !== 'connected' || draft.viewOnly} title="Ctrl+Alt+Del"><KeyRound size={13} /></button>
        <button type="button" onClick={beginEditing} title={t('编辑连接')}><Edit3 size={13} /></button>
        {status === 'connected' || status === 'connecting'
          ? <button type="button" onClick={() => disconnect(t('已断开远程桌面'), true)} title={t('断开')}><Unplug size={13} /></button>
          : <button type="button" onClick={() => void connect(draft)} title={t('连接')}><Monitor size={13} /></button>}
      </div>
      <div className="remote-desktop-viewport" ref={viewportRef}>
        <div className="remote-desktop-display">
          <canvas ref={canvasRef} tabIndex={0} aria-label={t('远程桌面画面')}
            onPointerDown={(event) => { event.currentTarget.focus({ preventScroll: true }); event.currentTarget.setPointerCapture(event.pointerId); sendInput({ type: 'mouseMove', ...pointerPosition(event) }); sendInput({ type: 'mouseButton', button: event.button, down: true }) }}
            onPointerMove={(event) => { pendingMouseRef.current = pointerPosition(event); if (mouseFrameRef.current === null) mouseFrameRef.current = window.requestAnimationFrame(() => { mouseFrameRef.current = null; const position = pendingMouseRef.current; pendingMouseRef.current = null; if (position) sendInput({ type: 'mouseMove', ...position }) }) }}
            onPointerUp={(event) => sendInput({ type: 'mouseButton', button: event.button, down: false })}
            onContextMenu={(event) => { event.preventDefault(); event.stopPropagation() }}
            onDragEnter={(event) => { event.preventDefault(); event.stopPropagation(); setFileDragActive(status === 'connected' && !draft.viewOnly) }}
            onDragOver={(event) => { event.preventDefault(); event.stopPropagation(); event.dataTransfer.dropEffect = 'copy'; setFileDragActive(status === 'connected' && !draft.viewOnly) }}
            onDragLeave={(event) => { event.preventDefault(); event.stopPropagation(); setFileDragActive(false) }}
            onDrop={(event) => {
              event.preventDefault()
              event.stopPropagation()
              setFileDragActive(false)
              if (event.dataTransfer.files.length > 0) {
                const paths = Array.from(event.dataTransfer.files)
                  .map((file) => (file as File & { path?: string }).path ?? '')
                  .filter(Boolean)
                if (paths.length > 0) void transferRemoteFiles(paths)
                else setMessage(t('请从系统文件管理器拖入文件'))
                return
              }
              const text = event.dataTransfer.getData('text/plain') || event.dataTransfer.getData('text/uri-list')
              if (text) void pasteDroppedText(text)
            }}
            onWheel={(event) => { event.preventDefault(); sendInput({ type: 'wheel', deltaX: Math.max(-120, Math.min(120, Math.round(-event.deltaX))), deltaY: Math.max(-120, Math.min(120, Math.round(-event.deltaY))) }) }}
            onKeyDown={(event) => sendKeyboardEvent(event, true)} onKeyUp={(event) => sendKeyboardEvent(event, false)}
            onCompositionEnd={(event) => sendInput({ type: 'text', text: event.data })}
            onBlur={() => sendInput({ type: 'releaseAll' })} />
        </div>
        {status !== 'connected' && (
          <div className={`remote-desktop-status ${status}`}>
            <Monitor size={24} />
            <strong>{status === 'connecting' ? t('正在连接') : status === 'error' ? t('连接失败') : t('远程桌面待命')}</strong>
            {message && <span>{message}</span>}
            {status !== 'connecting' && (
              <button type="button" onClick={() => void connect(draft)}>
                <Monitor size={14} />
                {t(status === 'error' ? '重新连接' : '连接远程桌面')}
              </button>
            )}
          </div>
        )}
        {fileDragActive && (
          <div className="remote-desktop-file-drop">
            <Upload size={26} />
            <strong>{t('释放后传输到远程桌面')}</strong>
            <span>{t('支持文件和文件夹')}</span>
          </div>
        )}
        {fileTransferProgress && (
          <div className="remote-desktop-transfer-progress" role="status" aria-live="polite">
            <div className="remote-desktop-transfer-heading">
              <strong>{fileTransferActive ? t('正在传输文件') : t('传输完成')}</strong>
              <span>{formatTransferPercent(fileTransferProgress)}%</span>
              {fileTransferActive && (
                <button type="button" onClick={() => cancelFileTransfer()} title={t('取消文件传输')} aria-label={t('取消文件传输')}>
                  <X size={12} />
                </button>
              )}
            </div>
            <div className="remote-desktop-transfer-track">
              <span style={{ width: `${formatTransferPercent(fileTransferProgress)}%` }} />
            </div>
            <div className="remote-desktop-transfer-detail">
              <span title={fileTransferProgress.currentFile}>{fileTransferProgress.currentFile ? fileName(fileTransferProgress.currentFile) : t('正在准备文件')}</span>
              <span>{formatBytes(fileTransferProgress.transferredBytes)} / {formatBytes(fileTransferProgress.totalBytes)}</span>
              <span>{formatBytes(fileTransferProgress.bytesPerSecond)}/s</span>
            </div>
          </div>
        )}
      </div>
      {status === 'connected' && message && <span className="remote-desktop-message">{message}</span>}
      {editing && (
        <div className="remote-desktop-editor-overlay">
          <form className="remote-desktop-form" data-remote-desktop-widget-id={widgetId} onSubmit={(event) => {
            event.preventDefault()
            saveEditing()
          }}>
            <div className="remote-desktop-protocol" role="group" aria-label={t('远程桌面协议')}>
              <button className="active" type="button">RDP</button>
              <button type="button" disabled title={t('VNC 内置引擎尚未接入')}>VNC</button>
            </div>
            <div className="remote-desktop-fields">
              <label><span>{t('主机')}</span><input autoFocus value={editDraft.host} onChange={(event) => setEditDraft({ ...editDraft, host: event.target.value })} placeholder="192.168.1.20" /></label>
              <label><span>{t('端口')}</span><input type="number" min="1" max="65535" value={editDraft.port} onChange={(event) => setEditDraft({ ...editDraft, port: Number(event.target.value) })} /></label>
              <label><span>{t('用户名')}</span><input value={editDraft.username} onChange={(event) => setEditDraft({ ...editDraft, username: event.target.value })} autoComplete="username" /></label>
              <label><span>{t('密码')}</span><input type="password" value={editDraft.password} onChange={(event) => setEditDraft({ ...editDraft, password: event.target.value })} autoComplete="current-password" /></label>
              <label><span>{t('域')}</span><input value={editDraft.domain} onChange={(event) => setEditDraft({ ...editDraft, domain: event.target.value })} /></label>
            </div>
            <details className="remote-desktop-advanced">
              <summary>{t('安全与交互')}</summary>
              <div className="remote-desktop-fields">
                <label><span>{t('安全模式')}</span><select value={editDraft.security} onChange={(event) => setEditDraft({ ...editDraft, security: event.target.value as RemoteDesktopConnection['security'] })}><option value="any">Auto</option><option value="nla">NLA</option><option value="tls">TLS</option></select></label>
              </div>
              <label className="remote-desktop-check"><input type="checkbox" checked={editDraft.viewOnly} onChange={(event) => setEditDraft({ ...editDraft, viewOnly: event.target.checked })} />{t('只看模式')}</label>
            </details>
            {editError && <p className="remote-desktop-form-message">{editError}</p>}
            <div className="remote-desktop-form-actions">
              {connection && <button type="button" onClick={cancelEditing}>{t('取消')}</button>}
              <button className="primary" type="submit"><Monitor size={14} />{t('保存并连接')}</button>
            </div>
          </form>
        </div>
      )}
    </div>
  )
}

function sameRemoteDesktopConnection(left: RemoteDesktopConnection, right: RemoteDesktopConnection) {
  return left.protocol === right.protocol
    && left.host === right.host
    && left.port === right.port
    && left.username === right.username
    && left.password === right.password
    && left.domain === right.domain
    && left.security === right.security
    && left.ignoreCertificate === right.ignoreCertificate
    && left.viewOnly === right.viewOnly
}

function formatTransferPercent(progress: RdpFileTransferProgress) {
  if (progress.completed) return 100
  if (progress.totalBytes <= 0) return 0
  return Math.min(100, Math.max(0, Math.round(progress.transferredBytes / progress.totalBytes * 100)))
}

function formatBytes(value: number) {
  if (!Number.isFinite(value) || value <= 0) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  const index = Math.min(units.length - 1, Math.floor(Math.log(value) / Math.log(1024)))
  const amount = value / 1024 ** index
  return `${amount >= 100 || index === 0 ? amount.toFixed(0) : amount.toFixed(1)} ${units[index]}`
}

function fileName(path: string) {
  return path.split(/[\\/]/).filter(Boolean).at(-1) ?? path
}

function delay(milliseconds: number) {
  return new Promise<void>((resolve) => window.setTimeout(resolve, milliseconds))
}

async function offerRdpClipboardFiles(sessionId: string, paths: string[], attempts: number) {
  let lastError: unknown
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    try {
      return await invoke<RdpClipboardFileOffer>('rdp_offer_clipboard_files', { sessionId, paths })
    } catch (error) {
      lastError = error
      if (!String(error).includes('剪贴板通道尚未就绪') || attempt + 1 >= attempts) throw error
      await delay(120)
    }
  }
  throw lastError
}

function createDefaultConnection(): RemoteDesktopConnection {
  return { protocol: 'rdp', host: '', port: 3389, username: '', password: '', domain: '', security: 'any', ignoreCertificate: true, viewOnly: false }
}

function remoteDesktopEndpoint(connection: Pick<RemoteDesktopConnection, 'host' | 'port'>) {
  return `${connection.host.trim().toLocaleLowerCase()}:${connection.port}`
}

function usesFixedResolution(connection: Pick<RemoteDesktopConnection, 'host' | 'port'>) {
  try {
    const endpoints = JSON.parse(localStorage.getItem(RDP_FIXED_RESOLUTION_HOSTS_KEY) ?? '[]')
    return Array.isArray(endpoints) && endpoints.includes(remoteDesktopEndpoint(connection))
  } catch {
    return false
  }
}

function rememberFixedResolution(connection: Pick<RemoteDesktopConnection, 'host' | 'port'>) {
  try {
    const endpoint = remoteDesktopEndpoint(connection)
    const stored = JSON.parse(localStorage.getItem(RDP_FIXED_RESOLUTION_HOSTS_KEY) ?? '[]')
    const endpoints = Array.isArray(stored) ? stored.filter((value): value is string => typeof value === 'string') : []
    localStorage.setItem(RDP_FIXED_RESOLUTION_HOSTS_KEY, JSON.stringify([...new Set([...endpoints, endpoint])]))
  } catch {
    // Storage may be unavailable in hardened webviews; the current recovery still succeeds.
  }
}

function localizeRdpError(message: string, t: (text: string) => string, code?: string) {
  if (code === 'authentication_failed') return t('RDP 身份验证失败，请检查用户名、密码和域')
  if (code === 'connection_refused') return t('目标主机拒绝了 RDP 连接，请检查端口和远程桌面服务')
  if (code === 'timeout') return t('RDP 连接超时，请检查网络和防火墙')
  if (code === 'frame_decode_failed') return t('服务器返回了不完整的桌面画面，请重新连接或使用固定分辨率兼容模式')
  if (code === 'remote_closed_during_startup') return t('远程服务器在桌面初始化时关闭了连接，请检查账号权限或服务器会话策略')
  if (code === 'remote_closed') return t('远程服务器已关闭 RDP 会话')
  const normalized = message.toLowerCase()
  if (normalized.includes('connection refused')) return t('目标主机拒绝了 RDP 连接，请检查端口和远程桌面服务')
  if (normalized.includes('credentials') || normalized.includes('logon') || normalized.includes('authentication')) return t('RDP 身份验证失败，请检查用户名、密码和域')
  if (normalized.includes('timed out') || normalized.includes('timeout')) return t('RDP 连接超时，请检查网络和防火墙')
  if (normalized.includes('.cargo') || normalized.includes('close_notify') || normalized.includes('docs.rs/rustls')) return t('远程服务器在桌面初始化时关闭了连接，请检查账号权限或服务器会话策略')
  return message.replace(/^RDP (connection|session) (failed|closed):\s*/i, '')
}
