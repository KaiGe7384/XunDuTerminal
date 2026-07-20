import { invoke } from './tauriBridge'

export type CredentialScope = 'ssh' | 'rdp-profile' | 'rdp-widget'

export type CredentialSecretRecord = {
  id: string
  userName: string
  secret?: string
}
export type CredentialVaultStatus = {
  backend: string
  persistent: boolean
}

type CredentialEntry = {
  key: string
  userName: string
  secret: string
}

export function credentialKey(scope: CredentialScope, id: string) {
  return `${scope}:${id}`
}

export function getCredentialVaultStatus() {
  return invoke<CredentialVaultStatus>('credential_vault_status')
}

export async function migrateAndHydrateCredentials(
  scope: CredentialScope,
  records: CredentialSecretRecord[],
) {
  const legacyEntries = records
    .filter((record) => Boolean(record.secret))
    .map((record): CredentialEntry => ({
      key: credentialKey(scope, record.id),
      userName: record.userName,
      secret: record.secret ?? '',
    }))
  if (legacyEntries.length > 0) {
    await invoke('credential_store_many', { entries: legacyEntries })
  }

  const keys = records.map((record) => credentialKey(scope, record.id))
  if (keys.length === 0) return new Map<string, string>()
  const secrets = await invoke<Record<string, string>>('credential_get_many', { keys })
  return new Map(records.map((record) => [record.id, secrets[credentialKey(scope, record.id)] ?? '']))
}

export async function syncCredentials(
  scope: CredentialScope,
  records: CredentialSecretRecord[],
  previousIds: ReadonlySet<string>,
) {
  const entries = records
    .filter((record) => Boolean(record.secret))
    .map((record): CredentialEntry => ({
      key: credentialKey(scope, record.id),
      userName: record.userName,
      secret: record.secret ?? '',
    }))
  const currentIds = new Set(records.map((record) => record.id))
  const deleteKeys = [
    ...[...previousIds]
      .filter((id) => !currentIds.has(id))
      .map((id) => credentialKey(scope, id)),
    ...records
      .filter((record) => !record.secret)
      .map((record) => credentialKey(scope, record.id)),
  ]

  if (entries.length > 0) await invoke('credential_store_many', { entries })
  if (deleteKeys.length > 0) await invoke('credential_delete_many', { keys: deleteKeys })
}
