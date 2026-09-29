//! Kiro managed authentication state.

use crate::proxy::providers::kiro_auth::KiroOAuthManager;
use std::sync::Arc;

/// Kiro manager already owns fine-grained locks for accounts, refreshes and
/// pending device codes, so no outer RwLock is required.
pub struct KiroOAuthState(pub Arc<KiroOAuthManager>);
