//! Managed Kiro Builder ID authentication.
//!
//! Implements AWS IAM Identity Center OIDC dynamic client registration +
//! Device Authorization Grant, then keeps the refresh token locally so Kiro
//! Runtime credentials can be refreshed just before proxy requests.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{hash_map::DefaultHasher, HashMap};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};

use super::copilot_auth::GitHubDeviceCodeResponse;
use super::kiro::KIRO_DEFAULT_REGION;

const KIRO_START_URL: &str = "https://view.awsapps.com/start";
const TOKEN_REFRESH_BUFFER_MS: i64 = 5 * 60 * 1000;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const POLLING_SAFETY_MARGIN_SECS: u64 = 3;

const KIRO_SCOPES: &[&str] = &[
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations",
    "codewhisperer:transformations",
    "codewhisperer:taskassist",
];

#[derive(Debug, thiserror::Error)]
pub enum KiroOAuthError {
    #[error("等待用户授权中")]
    AuthorizationPending,
    #[error("用户拒绝授权")]
    AccessDenied,
    #[error("Device Code 已过期")]
    ExpiredToken,
    #[error("Kiro OAuth token 获取失败: {0}")]
    TokenFetchFailed(String),
    #[error("Kiro refresh token 已失效")]
    RefreshTokenInvalid,
    #[error("Kiro 账号需要重新登录: {0}")]
    ReauthRequired(String),
    #[error("Kiro 账号不存在: {0}")]
    AccountNotFound(String),
    #[error("网络错误: {0}")]
    NetworkError(String),
    #[error("解析错误: {0}")]
    ParseError(String),
    #[error("IO 错误: {0}")]
    IoError(String),
}

impl From<reqwest::Error> for KiroOAuthError {
    fn from(value: reqwest::Error) -> Self {
        Self::NetworkError(value.to_string())
    }
}
impl From<std::io::Error> for KiroOAuthError {
    fn from(value: std::io::Error) -> Self {
        Self::IoError(value.to_string())
    }
}
impl From<serde_json::Error> for KiroOAuthError {
    fn from(value: serde_json::Error) -> Self {
        Self::ParseError(value.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KiroAccountData {
    account_id: String,
    login: String,
    refresh_token: String,
    client_id: String,
    client_secret: String,
    region: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile_arn: Option<String>,
    authenticated_at: i64,
    #[serde(default)]
    requires_reauth: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroOAuthAccount {
    pub id: String,
    pub login: String,
    pub avatar_url: Option<String>,
    pub authenticated_at: i64,
    pub github_domain: String,
    pub requires_reauth: bool,
}

impl From<&KiroAccountData> for KiroOAuthAccount {
    fn from(value: &KiroAccountData) -> Self {
        Self {
            id: value.account_id.clone(),
            login: value.login.clone(),
            avatar_url: None,
            authenticated_at: value.authenticated_at,
            github_domain: "kiro.dev".to_string(),
            requires_reauth: value.requires_reauth,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroOAuthStatus {
    pub accounts: Vec<KiroOAuthAccount>,
    pub default_account_id: Option<String>,
    pub authenticated: bool,
}

#[derive(Debug, Clone)]
pub struct KiroRuntimeCredential {
    pub token: String,
    pub profile_arn: Option<String>,
    pub region: String,
}

#[derive(Debug, Clone)]
struct CachedToken {
    access_token: String,
    expires_at_ms: i64,
}
impl CachedToken {
    fn usable(&self) -> bool {
        self.expires_at_ms - chrono::Utc::now().timestamp_millis() > TOKEN_REFRESH_BUFFER_MS
    }
}

#[derive(Debug, Clone)]
struct PendingDeviceCode {
    client_id: String,
    client_secret: String,
    region: String,
    expires_at_ms: i64,
    interval_secs: u64,
    next_poll_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct KiroOAuthStore {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    accounts: HashMap<String, KiroAccountData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegisterClientResponse {
    client_id: String,
    client_secret: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default = "default_expires_in")]
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}
fn default_expires_in() -> u64 { 600 }
fn default_interval() -> u64 { 5 }

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    profile_arn: Option<String>,
}

pub struct KiroOAuthManager {
    accounts: Arc<RwLock<HashMap<String, KiroAccountData>>>,
    default_account_id: Arc<RwLock<Option<String>>>,
    access_tokens: Arc<RwLock<HashMap<String, CachedToken>>>,
    refresh_locks: Arc<RwLock<HashMap<String, Arc<Mutex<()>>>>>,
    pending_device_codes: Arc<RwLock<HashMap<String, PendingDeviceCode>>>,
    mutation_lock: Arc<Mutex<()>>,
    storage_path: PathBuf,
}

impl KiroOAuthManager {
    pub fn new(data_dir: PathBuf) -> Self {
        let manager = Self {
            accounts: Arc::new(RwLock::new(HashMap::new())),
            default_account_id: Arc::new(RwLock::new(None)),
            access_tokens: Arc::new(RwLock::new(HashMap::new())),
            refresh_locks: Arc::new(RwLock::new(HashMap::new())),
            pending_device_codes: Arc::new(RwLock::new(HashMap::new())),
            mutation_lock: Arc::new(Mutex::new(())),
            storage_path: data_dir.join("kiro_oauth_auth.json"),
        };
        if let Err(error) = manager.load_from_disk_sync() {
            log::warn!("[KiroOAuth] 加载认证数据失败: {error}");
        }
        manager
    }

    pub async fn start_device_flow(&self) -> Result<GitHubDeviceCodeResponse, KiroOAuthError> {
        let region = KIRO_DEFAULT_REGION.to_string();
        let base = format!("https://oidc.{region}.amazonaws.com");
        let client = crate::proxy::http_client::get();
        let register = client
            .post(format!("{base}/client/register"))
            .timeout(HTTP_TIMEOUT)
            .json(&json!({
                "clientName": "CC Switch Kiro",
                "clientType": "public",
                "scopes": KIRO_SCOPES,
                "grantTypes": ["urn:ietf:params:oauth:grant-type:device_code", "refresh_token"],
                "issuerUrl": KIRO_START_URL
            }))
            .send().await?;
        let status = register.status();
        let bytes = register.bytes().await?;
        if !status.is_success() {
            return Err(KiroOAuthError::TokenFetchFailed(format!(
                "client/register HTTP {status}: {}", String::from_utf8_lossy(&bytes)
            )));
        }
        let registered: RegisterClientResponse = serde_json::from_slice(&bytes)?;

        let auth = client
            .post(format!("{base}/device_authorization"))
            .timeout(HTTP_TIMEOUT)
            .json(&json!({
                "clientId": registered.client_id,
                "clientSecret": registered.client_secret,
                "startUrl": KIRO_START_URL
            }))
            .send().await?;
        let status = auth.status();
        let bytes = auth.bytes().await?;
        if !status.is_success() {
            return Err(KiroOAuthError::TokenFetchFailed(format!(
                "device_authorization HTTP {status}: {}", String::from_utf8_lossy(&bytes)
            )));
        }
        let device: DeviceAuthorizationResponse = serde_json::from_slice(&bytes)?;
        let now = chrono::Utc::now().timestamp_millis();
        let interval = device.interval.max(1).saturating_add(POLLING_SAFETY_MARGIN_SECS);
        let mut pending_codes = self.pending_device_codes.write().await;
        pending_codes.retain(|_, pending| pending.expires_at_ms > now);
        pending_codes.insert(device.device_code.clone(), PendingDeviceCode {
            client_id: registered.client_id,
            client_secret: registered.client_secret,
            region,
            expires_at_ms: now.saturating_add((device.expires_in as i64).saturating_mul(1000)),
            interval_secs: interval,
            next_poll_at_ms: now,
        });
        Ok(GitHubDeviceCodeResponse {
            device_code: device.device_code,
            user_code: device.user_code,
            verification_uri: device.verification_uri_complete.unwrap_or(device.verification_uri),
            expires_in: device.expires_in,
            interval,
        })
    }

    pub async fn poll_for_token(&self, device_code: &str) -> Result<Option<KiroOAuthAccount>, KiroOAuthError> {
        let now = chrono::Utc::now().timestamp_millis();
        let mut pending_codes = self.pending_device_codes.write().await;
        let pending = pending_codes.get(device_code).cloned()
            .ok_or_else(|| KiroOAuthError::TokenFetchFailed("Device Code 不存在，请重新登录".to_string()))?;
        if pending.expires_at_ms <= now {
            pending_codes.remove(device_code);
            return Err(KiroOAuthError::ExpiredToken);
        }
        if pending.next_poll_at_ms > now {
            return Err(KiroOAuthError::AuthorizationPending);
        }
        if let Some(item) = pending_codes.get_mut(device_code) {
            item.next_poll_at_ms = now.saturating_add((item.interval_secs as i64).saturating_mul(1000));
        }
        drop(pending_codes);
        let endpoint = format!("https://oidc.{}.amazonaws.com/token", pending.region);
        let response = crate::proxy::http_client::get()
            .post(endpoint)
            .timeout(HTTP_TIMEOUT)
            .json(&json!({
                "clientId": pending.client_id,
                "clientSecret": pending.client_secret,
                "grantType": "urn:ietf:params:oauth:grant-type:device_code",
                "deviceCode": device_code
            }))
            .send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let code = oauth_error_code(&value);
            return match code.as_deref() {
                Some("authorization_pending") => Err(KiroOAuthError::AuthorizationPending),
                Some("slow_down") => {
                    if let Some(item) = self.pending_device_codes.write().await.get_mut(device_code) {
                        item.interval_secs = item.interval_secs.saturating_add(5);
                        item.next_poll_at_ms = chrono::Utc::now().timestamp_millis()
                            .saturating_add((item.interval_secs as i64).saturating_mul(1000));
                    }
                    Err(KiroOAuthError::AuthorizationPending)
                }
                Some("access_denied") => {
                    self.pending_device_codes.write().await.remove(device_code);
                    Err(KiroOAuthError::AccessDenied)
                }
                Some("expired_token") => {
                    self.pending_device_codes.write().await.remove(device_code);
                    Err(KiroOAuthError::ExpiredToken)
                }
                _ => Err(KiroOAuthError::TokenFetchFailed(format!(
                    "HTTP {status}: {}", String::from_utf8_lossy(&bytes)
                ))),
            };
        }
        let tokens: TokenResponse = serde_json::from_slice(&bytes)?;
        if tokens.access_token.trim().is_empty() {
            return Err(KiroOAuthError::ParseError("token 响应缺少 accessToken".to_string()));
        }
        let refresh_token = tokens.refresh_token.clone().filter(|value| !value.trim().is_empty())
            .ok_or_else(|| KiroOAuthError::ParseError("token 响应缺少 refreshToken".to_string()))?;
        let (account_id, login) = token_identity(tokens.id_token.as_deref().unwrap_or(&tokens.access_token), &refresh_token);
        let expires_at_ms = expiry_ms(tokens.expires_in);
        let account = KiroAccountData {
            account_id: account_id.clone(),
            login,
            refresh_token,
            client_id: pending.client_id,
            client_secret: pending.client_secret,
            region: pending.region,
            profile_arn: tokens.profile_arn,
            authenticated_at: chrono::Utc::now().timestamp(),
            requires_reauth: false,
        };
        self.commit_account(account.clone(), device_code).await?;
        self.access_tokens.write().await.insert(account_id, CachedToken {
            access_token: tokens.access_token,
            expires_at_ms,
        });
        Ok(Some(KiroOAuthAccount::from(&account)))
    }

    pub async fn get_runtime_credential_for_account(&self, account_id: &str) -> Result<KiroRuntimeCredential, KiroOAuthError> {
        if let Some(cached) = self.access_tokens.read().await.get(account_id).cloned().filter(CachedToken::usable) {
            let account = self.account(account_id).await?;
            return Ok(runtime_credential(&account, cached.access_token));
        }
        let lock = self.refresh_lock(account_id).await;
        let _guard = lock.lock().await;
        if let Some(cached) = self.access_tokens.read().await.get(account_id).cloned().filter(CachedToken::usable) {
            let account = self.account(account_id).await?;
            return Ok(runtime_credential(&account, cached.access_token));
        }
        let account = self.account(account_id).await?;
        if account.requires_reauth {
            return Err(KiroOAuthError::ReauthRequired(account_id.to_string()));
        }
        let tokens = match refresh_token(&account).await {
            Ok(tokens) => tokens,
            Err(KiroOAuthError::RefreshTokenInvalid) => {
                self.mark_reauth(account_id).await?;
                return Err(KiroOAuthError::ReauthRequired(account_id.to_string()));
            }
            Err(error) => return Err(error),
        };
        let mut updated = account;
        if let Some(refresh) = tokens.refresh_token.clone().filter(|value| !value.trim().is_empty()) {
            updated.refresh_token = refresh;
        }
        if let Some(profile) = tokens.profile_arn.clone().filter(|value| !value.trim().is_empty()) {
            updated.profile_arn = Some(profile);
        }
        updated.requires_reauth = false;
        self.replace_account(updated.clone()).await?;
        self.access_tokens.write().await.insert(account_id.to_string(), CachedToken {
            access_token: tokens.access_token.clone(),
            expires_at_ms: expiry_ms(tokens.expires_in),
        });
        Ok(runtime_credential(&updated, tokens.access_token))
    }

    pub async fn get_runtime_credential(&self) -> Result<KiroRuntimeCredential, KiroOAuthError> {
        let id = self.resolve_default_account_id().await.ok_or_else(|| KiroOAuthError::AccountNotFound("无可用 Kiro 账号".to_string()))?;
        self.get_runtime_credential_for_account(&id).await
    }

    pub async fn get_valid_token_for_account(&self, account_id: &str) -> Result<String, KiroOAuthError> {
        Ok(self.get_runtime_credential_for_account(account_id).await?.token)
    }
    pub async fn get_valid_token(&self) -> Result<String, KiroOAuthError> {
        Ok(self.get_runtime_credential().await?.token)
    }
    pub async fn default_account_id(&self) -> Option<String> { self.resolve_default_account_id().await }

    pub async fn get_status(&self) -> KiroOAuthStatus {
        let accounts = self.accounts.read().await.clone();
        let default = self.resolve_default_account_id().await;
        let mut list: Vec<_> = accounts.values().map(KiroOAuthAccount::from).collect();
        list.sort_by(|a,b| b.authenticated_at.cmp(&a.authenticated_at));
        KiroOAuthStatus { authenticated: default.is_some(), default_account_id: default, accounts: list }
    }

    pub async fn remove_account(&self, account_id: &str) -> Result<(), KiroOAuthError> {
        let _guard = self.mutation_lock.lock().await;
        let mut accounts = self.accounts.read().await.clone();
        if accounts.remove(account_id).is_none() { return Err(KiroOAuthError::AccountNotFound(account_id.to_string())); }
        let current_default = self.default_account_id.read().await.clone();
        let next_default = if current_default.as_deref() == Some(account_id) || current_default.as_ref().is_some_and(|id| !accounts.contains_key(id)) {
            fallback_default(&accounts)
        } else { current_default };
        self.persist_store(&accounts, next_default.clone())?;
        *self.accounts.write().await = accounts;
        *self.default_account_id.write().await = next_default;
        self.access_tokens.write().await.remove(account_id);
        self.refresh_locks.write().await.remove(account_id);
        Ok(())
    }

    pub async fn set_default_account(&self, account_id: &str) -> Result<(), KiroOAuthError> {
        let _guard = self.mutation_lock.lock().await;
        let accounts = self.accounts.read().await.clone();
        let account = accounts.get(account_id).ok_or_else(|| KiroOAuthError::AccountNotFound(account_id.to_string()))?;
        if account.requires_reauth { return Err(KiroOAuthError::ReauthRequired(account_id.to_string())); }
        self.persist_store(&accounts, Some(account_id.to_string()))?;
        *self.default_account_id.write().await = Some(account_id.to_string());
        Ok(())
    }

    pub async fn clear_auth(&self) -> Result<(), KiroOAuthError> {
        let _guard = self.mutation_lock.lock().await;
        let accounts = HashMap::new();
        self.persist_store(&accounts, None)?;
        *self.accounts.write().await = accounts;
        *self.default_account_id.write().await = None;
        self.access_tokens.write().await.clear();
        self.refresh_locks.write().await.clear();
        self.pending_device_codes.write().await.clear();
        Ok(())
    }

    async fn account(&self, account_id: &str) -> Result<KiroAccountData, KiroOAuthError> {
        self.accounts.read().await.get(account_id).cloned().ok_or_else(|| KiroOAuthError::AccountNotFound(account_id.to_string()))
    }
    async fn refresh_lock(&self, account_id: &str) -> Arc<Mutex<()>> {
        if let Some(lock) = self.refresh_locks.read().await.get(account_id).cloned() { return lock; }
        let mut locks = self.refresh_locks.write().await;
        locks.entry(account_id.to_string()).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
    }
    async fn resolve_default_account_id(&self) -> Option<String> {
        let accounts = self.accounts.read().await;
        let stored = self.default_account_id.read().await.clone();
        stored.filter(|id| accounts.get(id).is_some_and(|account| !account.requires_reauth))
            .or_else(|| fallback_default(&accounts))
    }
    pub async fn cancel_device_flow(&self, device_code: &str) -> bool {
        let _guard = self.mutation_lock.lock().await;
        self.pending_device_codes.write().await.remove(device_code).is_some()
    }

    async fn commit_account(&self, account: KiroAccountData, device_code: &str) -> Result<(), KiroOAuthError> {
        let _guard = self.mutation_lock.lock().await;
        if !self.pending_device_codes.read().await.contains_key(device_code) {
            return Err(KiroOAuthError::ExpiredToken);
        }
        let mut accounts = self.accounts.read().await.clone();
        let id = account.account_id.clone();
        accounts.insert(id.clone(), account);
        let current = self.default_account_id.read().await.clone();
        let default = current.filter(|value| accounts.get(value).is_some_and(|account| !account.requires_reauth)).or(Some(id));
        self.persist_store(&accounts, default.clone())?;
        *self.accounts.write().await = accounts;
        *self.default_account_id.write().await = default;
        self.pending_device_codes.write().await.remove(device_code);
        Ok(())
    }
    async fn replace_account(&self, account: KiroAccountData) -> Result<(), KiroOAuthError> {
        let _guard = self.mutation_lock.lock().await;
        let mut accounts = self.accounts.read().await.clone();
        if !accounts.contains_key(&account.account_id) {
            return Err(KiroOAuthError::AccountNotFound(account.account_id));
        }
        accounts.insert(account.account_id.clone(), account);
        let default = self.default_account_id.read().await.clone();
        self.persist_store(&accounts, default.clone())?;
        *self.accounts.write().await = accounts;
        Ok(())
    }
    async fn mark_reauth(&self, account_id: &str) -> Result<(), KiroOAuthError> {
        let _guard = self.mutation_lock.lock().await;
        let mut accounts = self.accounts.read().await.clone();
        let account = accounts.get_mut(account_id).ok_or_else(|| KiroOAuthError::AccountNotFound(account_id.to_string()))?;
        account.requires_reauth = true;
        let default = self.default_account_id.read().await.clone().filter(|id| id != account_id).or_else(|| fallback_default(&accounts));
        self.persist_store(&accounts, default.clone())?;
        *self.accounts.write().await = accounts;
        *self.default_account_id.write().await = default;
        self.access_tokens.write().await.remove(account_id);
        Ok(())
    }

    fn load_from_disk_sync(&self) -> Result<(), KiroOAuthError> {
        if !self.storage_path.exists() { return Ok(()); }
        let bytes = fs::read(&self.storage_path)?;
        let store: KiroOAuthStore = serde_json::from_slice(&bytes)?;
        *self.accounts.try_write().map_err(|error| KiroOAuthError::IoError(error.to_string()))? = store.accounts;
        *self.default_account_id.try_write().map_err(|error| KiroOAuthError::IoError(error.to_string()))? = store.default_account_id;
        Ok(())
    }
    fn persist_store(&self, accounts: &HashMap<String, KiroAccountData>, default: Option<String>) -> Result<(), KiroOAuthError> {
        if let Some(parent) = self.storage_path.parent() { fs::create_dir_all(parent)?; }
        let store = KiroOAuthStore { version: 1, accounts: accounts.clone(), default_account_id: default };
        crate::config::write_json_file_private(&self.storage_path, &store)
            .map_err(|error| KiroOAuthError::IoError(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> KiroAccountData {
        KiroAccountData {
            account_id: "test-account".into(), login: "test".into(),
            refresh_token: "test-refresh".into(), client_id: "test-client".into(),
            client_secret: "test-secret".into(), region: KIRO_DEFAULT_REGION.into(),
            profile_arn: None, authenticated_at: 1, requires_reauth: false,
        }
    }

    async fn pending(manager: &KiroOAuthManager) {
        manager.pending_device_codes.write().await.insert("device".into(), PendingDeviceCode {
            client_id: "test-client".into(), client_secret: "test-secret".into(),
            region: KIRO_DEFAULT_REGION.into(), expires_at_ms: i64::MAX,
            interval_secs: 5, next_poll_at_ms: 0,
        });
    }

    #[tokio::test]
    async fn kiro_cancel_prevents_late_login_commit() {
        let dir = tempfile::tempdir().unwrap();
        let manager = KiroOAuthManager::new(dir.path().into());
        pending(&manager).await;
        assert!(manager.cancel_device_flow("device").await);
        assert!(matches!(manager.commit_account(account(), "device").await, Err(KiroOAuthError::ExpiredToken)));
        assert!(manager.get_status().await.accounts.is_empty());
        assert!(!manager.storage_path.exists());
    }

    #[tokio::test]
    async fn kiro_store_reloads_inside_runtime_and_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let manager = KiroOAuthManager::new(dir.path().into());
        pending(&manager).await;
        manager.commit_account(account(), "device").await.unwrap();
        manager.set_default_account("test-account").await.unwrap();
        let reloaded = KiroOAuthManager::new(dir.path().into());
        assert!(reloaded.get_status().await.authenticated);
        assert!(!manager.cancel_device_flow("device").await);
        manager.remove_account("test-account").await.unwrap();
        assert!(matches!(manager.replace_account(account()).await, Err(KiroOAuthError::AccountNotFound(_))));
        let reloaded = KiroOAuthManager::new(dir.path().into());
        assert!(reloaded.get_status().await.accounts.is_empty());
    }

    #[tokio::test]
    async fn kiro_logout_prevents_late_login_commit() {
        let dir = tempfile::tempdir().unwrap();
        let manager = KiroOAuthManager::new(dir.path().into());
        pending(&manager).await;
        manager.clear_auth().await.unwrap();
        assert!(manager.commit_account(account(), "device").await.is_err());
    }
}

fn runtime_credential(account: &KiroAccountData, token: String) -> KiroRuntimeCredential {
    KiroRuntimeCredential { token, profile_arn: account.profile_arn.clone(), region: account.region.clone() }
}
fn fallback_default(accounts: &HashMap<String, KiroAccountData>) -> Option<String> {
    accounts.values().filter(|account| !account.requires_reauth).max_by_key(|account| account.authenticated_at).map(|account| account.account_id.clone())
}
fn expiry_ms(expires_in: Option<i64>) -> i64 {
    chrono::Utc::now().timestamp_millis().saturating_add(expires_in.unwrap_or(3600).max(60).saturating_mul(1000))
}
fn oauth_error_code(value: &Value) -> Option<String> {
    value.get("error").and_then(Value::as_str)
        .or_else(|| value.get("errorCode").and_then(Value::as_str))
        .map(ToString::to_string)
}
fn token_identity(token: &str, refresh_token: &str) -> (String, String) {
    if let Some(payload) = token.split('.').nth(1).and_then(|part| URL_SAFE_NO_PAD.decode(part).ok()).and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()) {
        if let Some(sub) = payload.get("sub").and_then(Value::as_str).filter(|value| !value.is_empty()) {
            let login = payload.get("email").or_else(|| payload.get("preferred_username")).and_then(Value::as_str).unwrap_or(sub).to_string();
            return (sub.to_string(), login);
        }
    }
    let mut hasher = DefaultHasher::new();
    refresh_token.hash(&mut hasher);
    let id = format!("kiro-{:016x}", hasher.finish());
    let short = id.trim_start_matches("kiro-").chars().take(8).collect::<String>();
    (id, format!("Kiro Builder ID ({short})"))
}

async fn refresh_token(account: &KiroAccountData) -> Result<TokenResponse, KiroOAuthError> {
    let endpoint = format!("https://oidc.{}.amazonaws.com/token", account.region);
    let response = crate::proxy::http_client::get()
        .post(endpoint)
        .timeout(HTTP_TIMEOUT)
        .json(&json!({
            "clientId": account.client_id,
            "clientSecret": account.client_secret,
            "refreshToken": account.refresh_token,
            "grantType": "refresh_token"
        }))
        .send().await?;
    let status = response.status();
    let bytes = response.bytes().await?;
    if !status.is_success() {
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let code = oauth_error_code(&value).unwrap_or_default();
        if status.as_u16() == 400 && matches!(code.as_str(), "invalid_grant" | "invalid_request" | "unauthorized_client") {
            return Err(KiroOAuthError::RefreshTokenInvalid);
        }
        return Err(KiroOAuthError::TokenFetchFailed(format!("refresh HTTP {status}: {}", String::from_utf8_lossy(&bytes))));
    }
    let token: TokenResponse = serde_json::from_slice(&bytes)?;
    if token.access_token.trim().is_empty() { return Err(KiroOAuthError::ParseError("refresh 响应缺少 accessToken".to_string())); }
    Ok(token)
}
