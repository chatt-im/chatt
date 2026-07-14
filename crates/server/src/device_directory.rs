//! Durable append-only account/device key directory.
//!
//! The server is only a relay and availability authority: every transition is
//! signed by the account authority and clients independently validate the same
//! chain. Server-side validation prevents honest-operation rollback, forks, and
//! reactivation before a statement reaches peers.

use std::{fs, path::PathBuf};

use hashbrown::HashSet;
use rpc::{
    crypto::{decode_hex, encode_hex},
    e2e::{
        AccountKeyAction, AccountKeyStatement, DeviceKeyStatus, ValidatedAccountLedger,
        account_statement_hash,
    },
    ids::{DeviceId, LedgerHash, UserId},
};
use toml_spanner::Toml;

use crate::config::atomic_write_toml;

const DIRECTORY_FILE: &str = "e2e-device-directory.toml";

#[derive(Debug)]
pub struct DeviceDirectory {
    path: Option<PathBuf>,
    server_public_key: Vec<u8>,
    accounts: Vec<AccountEntry>,
}

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
struct AccountEntry {
    user_id: u64,
    statements: Vec<String>,
    #[toml(default)]
    recovery_bundle: String,
    #[toml(default)]
    credentials: Vec<DeviceCredentialEntry>,
}

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
struct DeviceCredentialEntry {
    token_hash: String,
    #[toml(default)]
    device_id: String,
    #[toml(default)]
    password_epoch: u32,
}

#[derive(Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
struct DirectoryFile {
    #[toml(default)]
    accounts: Vec<AccountEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectoryAppend {
    Unchanged,
    Advanced {
        roster_epoch: u64,
        head: LedgerHash,
        revoked_credentials: Vec<String>,
    },
}

impl DeviceDirectory {
    pub fn in_memory(server_public_key: Vec<u8>) -> Self {
        Self {
            path: None,
            server_public_key,
            accounts: Vec::new(),
        }
    }

    pub fn open(data_dir: Option<PathBuf>, server_public_key: Vec<u8>) -> Result<Self, String> {
        let Some(dir) = data_dir else {
            return Ok(Self::in_memory(server_public_key));
        };
        let path = dir.join(DIRECTORY_FILE);
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path: Some(path),
                    server_public_key,
                    accounts: Vec::new(),
                });
            }
            Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
        };
        let source = path.display().to_string();
        let arena = toml_spanner::Arena::new();
        let mut doc = toml_spanner::parse(&content, &arena)
            .map_err(|error| format!("failed to parse {source}: {error}"))?;
        let file: DirectoryFile = doc.to().map_err(|error| {
            let errors: Vec<String> = error.errors.iter().map(ToString::to_string).collect();
            format!("failed to deserialize {source}: {}", errors.join(", "))
        })?;
        let directory = Self {
            path: Some(path),
            server_public_key,
            accounts: file.accounts,
        };
        directory.validate(&source)?;
        Ok(directory)
    }

    pub fn chain_for(&self, user_id: UserId) -> Result<Vec<AccountKeyStatement>, String> {
        self.entry(user_id)
            .map(Self::decode_statements)
            .unwrap_or_else(|| Ok(Vec::new()))
    }

    pub fn chain_after(
        &self,
        user_id: UserId,
        after: Option<LedgerHash>,
    ) -> Result<(Option<LedgerHash>, Vec<AccountKeyStatement>), String> {
        let statements = self.chain_for(user_id)?;
        let Some(after) = after else {
            return Ok((None, statements));
        };
        let Some(index) = statements
            .iter()
            .position(|statement| account_statement_hash(statement) == after)
        else {
            // A client checkpoint unknown to this server is evidence of a
            // restore or fork. Returning the full chain lets the client report
            // that mismatch; it must not treat the response as a valid suffix.
            return Ok((None, statements));
        };
        Ok((Some(after), statements[index + 1..].to_vec()))
    }

    pub fn validated(&self, user_id: UserId) -> Result<Option<ValidatedAccountLedger>, String> {
        let statements = self.chain_for(user_id)?;
        if statements.is_empty() {
            return Ok(None);
        }
        ValidatedAccountLedger::validate(&self.server_public_key, user_id, &statements).map(Some)
    }

    pub fn append(
        &mut self,
        user_id: UserId,
        statement: AccountKeyStatement,
    ) -> Result<DirectoryAppend, String> {
        let mut accounts = self.accounts.clone();
        let existing = accounts
            .iter_mut()
            .find(|entry| entry.user_id == user_id.0);
        let mut statements = match existing.as_ref() {
            Some(entry) => Self::decode_statements(entry)?,
            None => Vec::new(),
        };
        if statements
            .last()
            .is_some_and(|current| current == &statement)
        {
            return Ok(DirectoryAppend::Unchanged);
        }
        let revoked_device = match &statement.body.action {
            AccountKeyAction::RevokeDevice { device_id } => Some(*device_id),
            _ => None,
        };
        statements.push(statement);
        let validated = ValidatedAccountLedger::validate(
            &self.server_public_key,
            user_id,
            &statements,
        )?;
        let encoded: Vec<String> = statements
            .iter()
            .map(|statement| encode_hex(&jsony::to_binary(statement)))
            .collect();
        let mut revoked_credentials = Vec::new();
        match existing {
            Some(entry) => {
                entry.statements = encoded;
                if let Some(device_id) = revoked_device {
                    let encoded_device_id = encode_hex(&device_id.0);
                    entry.credentials.retain(|credential| {
                        let keep = credential.device_id != encoded_device_id;
                        if !keep {
                            revoked_credentials.push(credential.token_hash.clone());
                        }
                        keep
                    });
                }
            }
            None => accounts.push(AccountEntry {
                user_id: user_id.0,
                statements: encoded,
                recovery_bundle: String::new(),
                credentials: Vec::new(),
            }),
        }
        self.save_state(&accounts)?;
        self.accounts = accounts;
        Ok(DirectoryAppend::Advanced {
            roster_epoch: validated.roster_epoch,
            head: validated.head,
            revoked_credentials,
        })
    }

    pub fn authenticate_credential(
        &self,
        secret: &str,
    ) -> Option<(UserId, Option<DeviceId>, u32, String)> {
        self.accounts.iter().find_map(|account| {
            account.credentials.iter().find_map(|credential| {
                crate::config::verify_secret_hash(&credential.token_hash, secret).then(|| {
                    let device_id = decode_device_id(&credential.device_id);
                    (
                        UserId(account.user_id),
                        device_id,
                        credential.password_epoch,
                        credential.token_hash.clone(),
                    )
                })
            })
        })
    }

    pub fn credential(
        &self,
        token_hash: &str,
    ) -> Option<(UserId, Option<DeviceId>, u32)> {
        self.accounts.iter().find_map(|account| {
            account
                .credentials
                .iter()
                .find(|credential| credential.token_hash == token_hash)
                .map(|credential| {
                    let device_id = (!credential.device_id.is_empty())
                        .then(|| decode_hex(&credential.device_id).ok())
                        .flatten()
                        .and_then(|bytes| <[u8; 16]>::try_from(bytes).ok())
                        .map(DeviceId);
                    (
                        UserId(account.user_id),
                        device_id,
                        credential.password_epoch,
                    )
                })
        })
    }

    pub fn add_credential(
        &mut self,
        user_id: UserId,
        token_hash: String,
        device_id: Option<DeviceId>,
        password_epoch: u32,
    ) -> Result<(), String> {
        let mut accounts = self.accounts.clone();
        if accounts.iter().any(|account| {
            account
                .credentials
                .iter()
                .any(|credential| credential.token_hash == token_hash)
        }) {
            return Err("device bearer token is already registered".to_string());
        }
        let account = account_mut_or_insert(&mut accounts, user_id);
        account.credentials.push(DeviceCredentialEntry {
            token_hash,
            device_id: device_id
                .map(|device_id| encode_hex(&device_id.0))
                .unwrap_or_default(),
            password_epoch,
        });
        self.save_state(&accounts)?;
        self.accounts = accounts;
        Ok(())
    }

    pub fn bind_credential(
        &mut self,
        user_id: UserId,
        token_hash: &str,
        device_id: DeviceId,
    ) -> Result<(), String> {
        let mut accounts = self.accounts.clone();
        let account = accounts
            .iter_mut()
            .find(|account| account.user_id == user_id.0)
            .ok_or_else(|| "account credential disappeared".to_string())?;
        let credential = account
            .credentials
            .iter_mut()
            .find(|credential| credential.token_hash == token_hash)
            .ok_or_else(|| "session credential disappeared".to_string())?;
        if !credential.device_id.is_empty()
            && credential.device_id != encode_hex(&device_id.0)
        {
            return Err("bearer token is bound to another device".to_string());
        }
        credential.device_id = encode_hex(&device_id.0);
        self.save_state(&accounts)?;
        self.accounts = accounts;
        Ok(())
    }

    pub fn redeem_device(
        &mut self,
        user_id: UserId,
        statement: AccountKeyStatement,
        token_hash: String,
        device_id: DeviceId,
        password_epoch: u32,
    ) -> Result<(u64, LedgerHash), String> {
        let mut accounts = self.accounts.clone();
        if accounts.iter().any(|account| {
            account
                .credentials
                .iter()
                .any(|credential| credential.token_hash == token_hash)
        }) {
            return Err("device bearer token is already registered".to_string());
        }
        let linked_device = match &statement.body.action {
            AccountKeyAction::AddDevice { device } if device.device_id == device_id => {
                device.clone()
            }
            _ => {
                return Err(
                    "device-link redemption must append the requested AddDevice statement"
                        .to_string(),
                );
            }
        };
        let account = account_mut_or_insert(&mut accounts, user_id);
        let mut statements = Self::decode_statements(account)?;
        if statements.is_empty() {
            return Err("device-link account has no device ledger".to_string());
        }
        statements.push(statement);
        let validated = ValidatedAccountLedger::validate(
            &self.server_public_key,
            user_id,
            &statements,
        )?;
        let device = validated
            .device_keys
            .iter()
            .find(|device| device.keys.device_id == device_id)
            .ok_or_else(|| "device-link statement did not add the requested device".to_string())?;
        if device.status != DeviceKeyStatus::Active {
            return Err("device-link statement did not add an active device".to_string());
        }
        if device.keys != linked_device {
            return Err("device-link statement did not add the requested device".to_string());
        }
        account.statements = statements
            .iter()
            .map(|statement| encode_hex(&jsony::to_binary(statement)))
            .collect();
        account.credentials.push(DeviceCredentialEntry {
            token_hash,
            device_id: encode_hex(&device_id.0),
            password_epoch,
        });
        self.save_state(&accounts)?;
        self.accounts = accounts;
        Ok((validated.roster_epoch, validated.head))
    }

    pub fn active_device_signing_key(
        &self,
        user_id: UserId,
        device_id: DeviceId,
        key_epoch: u64,
        expected_head: LedgerHash,
    ) -> Result<Option<[u8; 32]>, String> {
        let Some(ledger) = self.validated(user_id)? else {
            return Ok(None);
        };
        if ledger.head != expected_head {
            return Ok(None);
        }
        let Some(device) = ledger.device_key(device_id, key_epoch) else {
            return Ok(None);
        };
        if device.status != DeviceKeyStatus::Active {
            return Ok(None);
        }
        let key = device
            .keys
            .signing_public_key
            .as_slice()
            .try_into()
            .map_err(|_| "validated device signing key has the wrong length".to_string())?;
        Ok(Some(key))
    }

    pub fn put_recovery_bundle(
        &mut self,
        user_id: UserId,
        expected_head: LedgerHash,
        bundle: &[u8],
    ) -> Result<(), String> {
        let ledger = self
            .validated(user_id)?
            .ok_or_else(|| "account has no device ledger".to_string())?;
        if ledger.head != expected_head {
            return Err("recovery bundle ledger head is stale".to_string());
        }
        let digest = ring::digest::digest(&ring::digest::SHA256, bundle);
        if ledger.recovery_bundle_hash.as_slice() != digest.as_ref() {
            return Err("recovery bundle does not match the signed ledger hash".to_string());
        }
        let mut accounts = self.accounts.clone();
        let entry = accounts
            .iter_mut()
            .find(|entry| entry.user_id == user_id.0)
            .ok_or_else(|| "account device ledger disappeared".to_string())?;
        entry.recovery_bundle = encode_hex(bundle);
        self.save_state(&accounts)?;
        self.accounts = accounts;
        Ok(())
    }

    pub fn recovery_bundle(&self, user_id: UserId) -> Result<Option<Vec<u8>>, String> {
        let Some(entry) = self.entry(user_id) else {
            return Ok(None);
        };
        if entry.recovery_bundle.is_empty() {
            return Ok(None);
        }
        decode_hex(&entry.recovery_bundle)
            .map(Some)
            .map_err(|_| "stored recovery bundle is invalid hex".to_string())
    }

    fn entry(&self, user_id: UserId) -> Option<&AccountEntry> {
        self.accounts
            .iter()
            .find(|entry| entry.user_id == user_id.0)
    }

    fn decode_statements(entry: &AccountEntry) -> Result<Vec<AccountKeyStatement>, String> {
        entry
            .statements
            .iter()
            .map(|encoded| {
                let bytes = decode_hex(encoded)
                    .map_err(|_| "stored account key statement is invalid hex".to_string())?;
                jsony::from_binary(&bytes)
                    .map_err(|error| format!("stored account key statement is invalid: {error}"))
            })
            .collect()
    }

    fn validate(&self, source: &str) -> Result<(), String> {
        let mut users = HashSet::new();
        let mut credential_hashes = HashSet::new();
        for entry in &self.accounts {
            if !users.insert(entry.user_id) {
                return Err(format!("{source}: duplicate account for user {}", entry.user_id));
            }
            let statements = Self::decode_statements(entry)
                .map_err(|error| format!("{source}: user {}: {error}", entry.user_id))?;
            if !statements.is_empty() {
                ValidatedAccountLedger::validate(
                    &self.server_public_key,
                    UserId(entry.user_id),
                    &statements,
                )
                .map_err(|error| format!("{source}: user {}: {error}", entry.user_id))?;
            }
            if !entry.recovery_bundle.is_empty() {
                decode_hex(&entry.recovery_bundle).map_err(|_| {
                    format!("{source}: user {} recovery bundle is invalid hex", entry.user_id)
                })?;
            }
            for credential in &entry.credentials {
                if credential.token_hash.trim().is_empty() {
                    return Err(format!(
                        "{source}: user {} credential has an empty token hash",
                        entry.user_id
                    ));
                }
                if !credential_hashes.insert(credential.token_hash.as_str()) {
                    return Err(format!(
                        "{source}: duplicate device bearer credential"
                    ));
                }
                if !credential.device_id.is_empty()
                    && decode_device_id(&credential.device_id).is_none()
                {
                    return Err(format!(
                        "{source}: user {} credential has an invalid device id",
                        entry.user_id
                    ));
                }
            }
        }
        Ok(())
    }

    fn save_state(&self, accounts: &[AccountEntry]) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
        }
        atomic_write_toml(path, &Self::snapshot(accounts))
    }

    fn snapshot(accounts: &[AccountEntry]) -> String {
        let mut out = String::from(
            "# Chatt signed account/device directory. Managed by the server; do not edit.\n",
        );
        for account in accounts {
            out.push_str("\n[[accounts]]\n");
            out.push_str(&format!("user-id = {}\n", account.user_id));
            out.push_str("statements = [\n");
            for statement in &account.statements {
                out.push_str(&format!("  \"{statement}\",\n"));
            }
            out.push_str("]\n");
            if !account.recovery_bundle.is_empty() {
                out.push_str(&format!(
                    "recovery-bundle = \"{}\"\n",
                    account.recovery_bundle
                ));
            }
            for credential in &account.credentials {
                out.push_str("[[accounts.credentials]]\n");
                out.push_str(&format!("token-hash = \"{}\"\n", credential.token_hash));
                if !credential.device_id.is_empty() {
                    out.push_str(&format!("device-id = \"{}\"\n", credential.device_id));
                }
                if credential.password_epoch != 0 {
                    out.push_str(&format!(
                        "password-epoch = {}\n",
                        credential.password_epoch
                    ));
                }
            }
        }
        out
    }
}

fn decode_device_id(encoded: &str) -> Option<DeviceId> {
    (!encoded.is_empty())
        .then(|| decode_hex(encoded).ok())
        .flatten()
        .and_then(|bytes| <[u8; 16]>::try_from(bytes).ok())
        .map(DeviceId)
}

fn account_mut_or_insert(accounts: &mut Vec<AccountEntry>, user_id: UserId) -> &mut AccountEntry {
    if let Some(index) = accounts
        .iter()
        .position(|account| account.user_id == user_id.0)
    {
        return &mut accounts[index];
    }
    accounts.push(AccountEntry {
        user_id: user_id.0,
        statements: Vec::new(),
        recovery_bundle: String::new(),
        credentials: Vec::new(),
    });
    accounts.last_mut().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_credentials_round_trip_and_bind() {
        let dir = std::env::temp_dir().join(format!(
            "chatt-device-directory-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&dir);
        let server_key = vec![7; 32];
        let user_id = UserId(42);
        let token_hash = crate::config::hash_secret("device-token");
        let device_id = DeviceId([3; 16]);

        let mut directory =
            DeviceDirectory::open(Some(dir.clone()), server_key.clone()).unwrap();
        directory
            .add_credential(user_id, token_hash, None, 0)
            .unwrap();
        let stored_hash = directory
            .authenticate_credential("device-token")
            .unwrap()
            .3;
        directory
            .bind_credential(user_id, &stored_hash, device_id)
            .unwrap();

        let reloaded = DeviceDirectory::open(Some(dir.clone()), server_key).unwrap();
        let (stored_user, stored_device, _, _) =
            reloaded.authenticate_credential("device-token").unwrap();
        assert_eq!(stored_user, user_id);
        assert_eq!(stored_device, Some(device_id));
        assert!(reloaded.authenticate_credential("wrong-token").is_none());
        let _ = fs::remove_dir_all(dir);
    }
}
