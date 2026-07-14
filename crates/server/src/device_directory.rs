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
        AccountKeyStatement, DeviceKeyStatus, ValidatedAccountLedger,
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
}

#[derive(Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
struct DirectoryFile {
    #[toml(default)]
    accounts: Vec<AccountEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryAppend {
    Unchanged,
    Advanced {
        roster_epoch: u64,
        head: LedgerHash,
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
        match existing {
            Some(entry) => entry.statements = encoded,
            None => accounts.push(AccountEntry {
                user_id: user_id.0,
                statements: encoded,
                recovery_bundle: String::new(),
            }),
        }
        self.save_state(&accounts)?;
        self.accounts = accounts;
        Ok(DirectoryAppend::Advanced {
            roster_epoch: validated.roster_epoch,
            head: validated.head,
        })
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
        for entry in &self.accounts {
            if !users.insert(entry.user_id) {
                return Err(format!("{source}: duplicate account for user {}", entry.user_id));
            }
            let statements = Self::decode_statements(entry)
                .map_err(|error| format!("{source}: user {}: {error}", entry.user_id))?;
            if statements.is_empty() {
                return Err(format!(
                    "{source}: account for user {} has no genesis",
                    entry.user_id
                ));
            }
            ValidatedAccountLedger::validate(
                &self.server_public_key,
                UserId(entry.user_id),
                &statements,
            )
            .map_err(|error| format!("{source}: user {}: {error}", entry.user_id))?;
            if !entry.recovery_bundle.is_empty() {
                decode_hex(&entry.recovery_bundle).map_err(|_| {
                    format!("{source}: user {} recovery bundle is invalid hex", entry.user_id)
                })?;
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
        }
        out
    }
}

