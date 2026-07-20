use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const CREDENTIAL_TARGET_PREFIX: &str = "XunDuTerminal/";
const MAX_CREDENTIAL_KEY_BYTES: usize = 200;
const MAX_CREDENTIAL_SECRET_BYTES: usize = 2048;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialEntry {
    key: String,
    user_name: String,
    secret: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialVaultStatus {
    backend: &'static str,
    persistent: bool,
}

#[tauri::command]
pub fn credential_vault_status() -> CredentialVaultStatus {
    CredentialVaultStatus {
        backend: if cfg!(target_os = "windows") {
            "windows-credential-manager"
        } else {
            "unsupported"
        },
        persistent: cfg!(target_os = "windows"),
    }
}

#[tauri::command]
pub fn credential_store_many(entries: Vec<CredentialEntry>) -> Result<(), String> {
    for entry in entries {
        validate_key(&entry.key)?;
        if entry.secret.as_bytes().len() > MAX_CREDENTIAL_SECRET_BYTES {
            return Err(format!(
                "Credential secret is too large for the system vault: {} bytes",
                entry.secret.as_bytes().len()
            ));
        }
        platform::store(&entry.key, &entry.user_name, &entry.secret)?;
    }
    Ok(())
}

#[tauri::command]
pub fn credential_get_many(keys: Vec<String>) -> Result<HashMap<String, String>, String> {
    let mut secrets = HashMap::new();
    for key in keys {
        validate_key(&key)?;
        if let Some(secret) = platform::read(&key)? {
            secrets.insert(key, secret);
        }
    }
    Ok(secrets)
}

#[tauri::command]
pub fn credential_delete_many(keys: Vec<String>) -> Result<(), String> {
    for key in keys {
        validate_key(&key)?;
        platform::delete(&key)?;
    }
    Ok(())
}

fn validate_key(key: &str) -> Result<(), String> {
    if key.is_empty() || key.len() > MAX_CREDENTIAL_KEY_BYTES {
        return Err("Credential key is empty or too long".into());
    }
    if !key
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
    {
        return Err("Credential key contains unsupported characters".into());
    }
    Ok(())
}

fn target_name(key: &str) -> String {
    format!("{CREDENTIAL_TARGET_PREFIX}{key}")
}

#[cfg(target_os = "windows")]
mod platform {
    use super::target_name;
    use std::{ptr, slice};
    use windows::{
        core::{HRESULT, PCWSTR, PWSTR},
        Win32::{
            Foundation::ERROR_NOT_FOUND,
            Security::Credentials::{
                CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW,
                CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
            },
        },
    };

    pub fn store(key: &str, user_name: &str, secret: &str) -> Result<(), String> {
        let mut target = to_wide(&target_name(key));
        let mut user = to_wide(if user_name.trim().is_empty() {
            "XunDuTerminal"
        } else {
            user_name
        });
        let mut secret_bytes = secret.as_bytes().to_vec();
        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target.as_mut_ptr()),
            CredentialBlobSize: secret_bytes.len() as u32,
            CredentialBlob: secret_bytes.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            UserName: PWSTR(user.as_mut_ptr()),
            ..Default::default()
        };

        unsafe { CredWriteW(&credential, 0) }
            .map_err(|error| format!("Windows Credential Manager write failed: {error}"))
    }

    pub fn read(key: &str) -> Result<Option<String>, String> {
        let target = to_wide(&target_name(key));
        let mut credential_ptr: *mut CREDENTIALW = ptr::null_mut();
        let result = unsafe {
            CredReadW(
                PCWSTR(target.as_ptr()),
                CRED_TYPE_GENERIC,
                None,
                &mut credential_ptr,
            )
        };

        if let Err(error) = result {
            if error.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0) {
                return Ok(None);
            }
            return Err(format!("Windows Credential Manager read failed: {error}"));
        }
        if credential_ptr.is_null() {
            return Ok(None);
        }

        let secret = unsafe {
            let credential = &*credential_ptr;
            let bytes = if credential.CredentialBlob.is_null() || credential.CredentialBlobSize == 0
            {
                &[][..]
            } else {
                slice::from_raw_parts(
                    credential.CredentialBlob,
                    credential.CredentialBlobSize as usize,
                )
            };
            let value = String::from_utf8(bytes.to_vec())
                .map_err(|_| "Credential data is not valid UTF-8".to_string());
            CredFree(credential_ptr.cast());
            value?
        };
        Ok(Some(secret))
    }

    pub fn delete(key: &str) -> Result<(), String> {
        let target = to_wide(&target_name(key));
        match unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None) } {
            Ok(()) => Ok(()),
            Err(error) if error.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0) => Ok(()),
            Err(error) => Err(format!("Windows Credential Manager delete failed: {error}")),
        }
    }

    fn to_wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }
}

#[cfg(not(target_os = "windows"))]
mod platform {
    pub fn store(_key: &str, _user_name: &str, _secret: &str) -> Result<(), String> {
        Err("The system credential vault is not supported on this platform yet".into())
    }

    pub fn read(_key: &str) -> Result<Option<String>, String> {
        Err("The system credential vault is not supported on this platform yet".into())
    }

    pub fn delete(_key: &str) -> Result<(), String> {
        Err("The system credential vault is not supported on this platform yet".into())
    }
}
