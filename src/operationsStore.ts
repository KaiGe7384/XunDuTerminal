export type TransferProtocol = 'local' | 'sftp' | 'rdp'
export type TransferDirection = 'copy' | 'download' | 'upload'
export type TransferStatus = 'queued' | 'running' | 'completed' | 'error' | 'cancelled'

export type TransferRecord = {
  id: string
  protocol: TransferProtocol
  direction: TransferDirection
  title: string
  source: string
  destination: string
  status: TransferStatus
  totalBytes: number
  transferredBytes: number
  bytesPerSecond: number
  copiedFiles: number
  totalFiles: number
  currentFile: string
  message: string
  startedAt: number
  updatedAt: number
  resumable: boolean
}

export type TransferActions = {
  cancel?: () => void | Promise<void>
  retry?: () => void | Promise<void>
}

export type OperationNotification = {
  id: string
  title: string
  message: string
  level: 'info' | 'success' | 'error'
  createdAt: number
  read: boolean
}

const TRANSFER_STORAGE_KEY = 'xundu.phase2.transfers.v1'
const NOTIFICATION_STORAGE_KEY = 'xundu.phase2.notifications.v1'
const SYSTEM_NOTIFICATION_KEY = 'xundu.phase2.systemNotifications.v1'
const MAX_TRANSFER_HISTORY = 100
const MAX_NOTIFICATION_HISTORY = 100

const transferListeners = new Set<() => void>()
const notificationListeners = new Set<() => void>()
const transferActions = new Map<string, TransferActions>()

let transferSnapshot = loadTransfers()
let notificationSnapshot = loadNotifications()

export function subscribeTransfers(listener: () => void) {
  transferListeners.add(listener)
  return () => transferListeners.delete(listener)
}

export function getTransfersSnapshot() {
  return transferSnapshot
}

export function upsertTransfer(
  record: Pick<TransferRecord, 'id'> & Partial<Omit<TransferRecord, 'id'>>,
  actions?: TransferActions,
) {
  const now = Date.now()
  const current = transferSnapshot.find((item) => item.id === record.id)
  const transferredBytes = finiteNonNegative(record.transferredBytes ?? current?.transferredBytes ?? 0)
  const totalBytes = finiteNonNegative(record.totalBytes ?? current?.totalBytes ?? 0)
  const next: TransferRecord = {
    id: record.id,
    protocol: record.protocol ?? current?.protocol ?? 'local',
    direction: record.direction ?? current?.direction ?? 'copy',
    title: record.title ?? current?.title ?? '文件传输',
    source: record.source ?? current?.source ?? '',
    destination: record.destination ?? current?.destination ?? '',
    status: record.status ?? current?.status ?? 'queued',
    totalBytes,
    transferredBytes: totalBytes > 0 ? Math.min(totalBytes, Math.max(current?.transferredBytes ?? 0, transferredBytes)) : transferredBytes,
    bytesPerSecond: finiteNonNegative(record.bytesPerSecond ?? current?.bytesPerSecond ?? 0),
    copiedFiles: Math.floor(finiteNonNegative(record.copiedFiles ?? current?.copiedFiles ?? 0)),
    totalFiles: Math.floor(finiteNonNegative(record.totalFiles ?? current?.totalFiles ?? 0)),
    currentFile: record.currentFile ?? current?.currentFile ?? '',
    message: record.message ?? current?.message ?? '',
    startedAt: record.startedAt ?? current?.startedAt ?? now,
    updatedAt: now,
    resumable: record.resumable ?? current?.resumable ?? false,
  }
  transferSnapshot = [next, ...transferSnapshot.filter((item) => item.id !== next.id)]
    .sort((left, right) => right.updatedAt - left.updatedAt)
    .slice(0, MAX_TRANSFER_HISTORY)
  if (actions) transferActions.set(next.id, { ...transferActions.get(next.id), ...actions })
  if (isTerminalTransferStatus(next.status)) {
    const existing = transferActions.get(next.id)
    transferActions.set(next.id, { retry: existing?.retry })
  }
  persistJson(TRANSFER_STORAGE_KEY, transferSnapshot)
  transferListeners.forEach((listener) => listener())
  if (next.status === 'error' && current?.status !== 'error') {
    notifyOperation('传输失败', next.message || next.title, 'error')
  }
  if (next.status === 'completed' && current?.status !== 'completed') {
    notifyOperation('传输完成', next.title, 'success')
  }
}

export async function cancelTransfer(id: string) {
  const action = transferActions.get(id)?.cancel
  if (!action) return false
  await action()
  return true
}

export async function retryTransfer(id: string) {
  const action = transferActions.get(id)?.retry
  if (!action) return false
  await action()
  return true
}

export function canCancelTransfer(id: string) {
  return Boolean(transferActions.get(id)?.cancel)
}

export function canRetryTransfer(id: string) {
  return Boolean(transferActions.get(id)?.retry)
}

export function clearFinishedTransfers() {
  const runningIds = new Set(
    transferSnapshot
      .filter((item) => item.status === 'queued' || item.status === 'running')
      .map((item) => item.id),
  )
  transferSnapshot = transferSnapshot.filter((item) => runningIds.has(item.id))
  for (const id of transferActions.keys()) {
    if (!runningIds.has(id)) transferActions.delete(id)
  }
  persistJson(TRANSFER_STORAGE_KEY, transferSnapshot)
  transferListeners.forEach((listener) => listener())
}

export function subscribeNotifications(listener: () => void) {
  notificationListeners.add(listener)
  return () => notificationListeners.delete(listener)
}

export function getNotificationsSnapshot() {
  return notificationSnapshot
}

export function notifyOperation(
  title: string,
  message: string,
  level: OperationNotification['level'] = 'info',
) {
  const duplicate = notificationSnapshot.find((item) =>
    item.title === title && item.message === message && Date.now() - item.createdAt < 1_500,
  )
  if (duplicate) return duplicate.id
  const notification: OperationNotification = {
    id: crypto.randomUUID(),
    title,
    message,
    level,
    createdAt: Date.now(),
    read: false,
  }
  notificationSnapshot = [notification, ...notificationSnapshot].slice(0, MAX_NOTIFICATION_HISTORY)
  persistJson(NOTIFICATION_STORAGE_KEY, notificationSnapshot)
  notificationListeners.forEach((listener) => listener())
  if (isSystemNotificationEnabled()) void sendBrowserNotification(notification)
  return notification.id
}

export function markNotificationsRead() {
  if (notificationSnapshot.every((item) => item.read)) return
  notificationSnapshot = notificationSnapshot.map((item) => ({ ...item, read: true }))
  persistJson(NOTIFICATION_STORAGE_KEY, notificationSnapshot)
  notificationListeners.forEach((listener) => listener())
}

export function clearNotifications() {
  notificationSnapshot = []
  persistJson(NOTIFICATION_STORAGE_KEY, notificationSnapshot)
  notificationListeners.forEach((listener) => listener())
}

export function isSystemNotificationEnabled() {
  try {
    return localStorage.getItem(SYSTEM_NOTIFICATION_KEY) === 'true'
  } catch {
    return false
  }
}

export async function setSystemNotificationEnabled(enabled: boolean) {
  if (enabled && typeof Notification !== 'undefined' && Notification.permission === 'default') {
    const permission = await Notification.requestPermission()
    if (permission !== 'granted') return false
  }
  try {
    localStorage.setItem(SYSTEM_NOTIFICATION_KEY, String(enabled))
  } catch {
    return false
  }
  return true
}

function loadTransfers(): TransferRecord[] {
  const parsed = readJson(TRANSFER_STORAGE_KEY)
  if (!Array.isArray(parsed)) return []
  return parsed
    .map(normalizeTransfer)
    .filter((item): item is TransferRecord => Boolean(item))
    .map((item) => item.status === 'queued' || item.status === 'running'
      ? { ...item, status: 'error' as const, bytesPerSecond: 0, message: '应用退出时传输尚未完成，可重新发起任务' }
      : item)
    .slice(0, MAX_TRANSFER_HISTORY)
}

function normalizeTransfer(source: unknown): TransferRecord | null {
  if (!source || typeof source !== 'object') return null
  const item = source as Partial<TransferRecord>
  if (typeof item.id !== 'string' || !item.id || typeof item.title !== 'string') return null
  const protocols: TransferProtocol[] = ['local', 'sftp', 'rdp']
  const directions: TransferDirection[] = ['copy', 'download', 'upload']
  const statuses: TransferStatus[] = ['queued', 'running', 'completed', 'error', 'cancelled']
  return {
    id: item.id,
    protocol: protocols.includes(item.protocol as TransferProtocol) ? item.protocol as TransferProtocol : 'local',
    direction: directions.includes(item.direction as TransferDirection) ? item.direction as TransferDirection : 'copy',
    title: item.title,
    source: typeof item.source === 'string' ? item.source : '',
    destination: typeof item.destination === 'string' ? item.destination : '',
    status: statuses.includes(item.status as TransferStatus) ? item.status as TransferStatus : 'error',
    totalBytes: finiteNonNegative(item.totalBytes),
    transferredBytes: finiteNonNegative(item.transferredBytes),
    bytesPerSecond: finiteNonNegative(item.bytesPerSecond),
    copiedFiles: Math.floor(finiteNonNegative(item.copiedFiles)),
    totalFiles: Math.floor(finiteNonNegative(item.totalFiles)),
    currentFile: typeof item.currentFile === 'string' ? item.currentFile : '',
    message: typeof item.message === 'string' ? item.message : '',
    startedAt: finiteTimestamp(item.startedAt),
    updatedAt: finiteTimestamp(item.updatedAt),
    resumable: item.resumable === true,
  }
}

function loadNotifications(): OperationNotification[] {
  const parsed = readJson(NOTIFICATION_STORAGE_KEY)
  if (!Array.isArray(parsed)) return []
  return parsed.flatMap((source) => {
    if (!source || typeof source !== 'object') return []
    const item = source as Partial<OperationNotification>
    if (typeof item.id !== 'string' || typeof item.title !== 'string' || typeof item.message !== 'string') return []
    const level: OperationNotification['level'] = item.level === 'success' || item.level === 'error' ? item.level : 'info'
    return [{
      id: item.id,
      title: item.title,
      message: item.message,
      level,
      createdAt: finiteTimestamp(item.createdAt),
      read: item.read === true,
    }]
  }).slice(0, MAX_NOTIFICATION_HISTORY)
}

async function sendBrowserNotification(notification: OperationNotification) {
  if (typeof Notification === 'undefined' || Notification.permission !== 'granted') return
  try {
    new Notification(notification.title, { body: notification.message, tag: notification.id })
  } catch {
    // Application notifications remain available when the system API is unavailable.
  }
}

function isTerminalTransferStatus(status: TransferStatus) {
  return status === 'completed' || status === 'error' || status === 'cancelled'
}

function finiteNonNegative(value: unknown) {
  const parsed = Number(value)
  return Number.isFinite(parsed) ? Math.max(0, parsed) : 0
}

function finiteTimestamp(value: unknown) {
  const parsed = Number(value)
  return Number.isFinite(parsed) && parsed > 0 ? parsed : Date.now()
}

function readJson(key: string): unknown {
  try {
    const raw = localStorage.getItem(key)
    return raw ? JSON.parse(raw) : null
  } catch {
    return null
  }
}

function persistJson(key: string, value: unknown) {
  try {
    localStorage.setItem(key, JSON.stringify(value))
  } catch {
    // Task history persistence is best-effort and must not block active sessions.
  }
}
