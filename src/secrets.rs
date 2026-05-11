//! Secrets resolution for blackbox.
//!
//! Provides a secure way to resolve secrets from multiple sources:
//! 1. systemd LoadCredential (= $CREDENTIALS_DIRECTORY/<name>)
//! 2. File secrets ($XDG_DATA_HOME/blackbox/secrets/<name>)
//! 3. Environment variables (BLACKBOX_SECRET_<UPPER_SNAKE_NAME>)

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use regex::Regex;

/// A secret value that has been resolved.
/// The inner string is not exposed except through the `expose` method.
#[derive(Debug)]
pub struct SecretValue(String);

impl SecretValue {
    /// Create a new secret value. This is internal; use `resolve` to get secrets.
    fn new(value: String) -> Self {
        SecretValue(value)
    }

    /// Expose the secret value. This should only be called when the secret
    /// is actually needed (e.g., for passing to child processes).
    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// Sources for secret resolution
#[derive(Debug, Clone)]
pub struct SecretSources {
    /// Directory for systemd LoadCredential files ($CREDENTIALS_DIRECTORY)
    pub credentials_dir: Option<PathBuf>,
    /// Directory for file secrets
    pub secrets_dir: PathBuf,
    /// Prefix for environment variables
    pub env_prefix: String,
}

impl Default for SecretSources {
    fn default() -> Self {
        let data_dir = dirs::data_dir().unwrap_or_else(|| {
            dirs::home_dir().map(|h| h.join(".local").join("share")).unwrap_or_else(|| PathBuf::from("/tmp"))
        });
        let secrets_dir = data_dir.join("blackbox").join("secrets");
        
        SecretSources {
            credentials_dir: std::env::var("CREDENTIALS_DIRECTORY")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from),
            secrets_dir,
            env_prefix: "BLACKBOX_SECRET_".to_string(),
        }
    }
}

/// Validate a secret name.
/// Must match [A-Za-z0-9_.-]+ and must not contain path separators.
fn validate_secret_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Secret name cannot be empty");
    }
    
    // Check for path separators
    if name.contains('/') || name.contains('\\') {
        bail!("Secret name cannot contain path separators");
    }
    
    // Check character class
    let re = Regex::new(r"^[A-Za-z0-9_.-]+$").unwrap();
    if !re.is_match(name) {
        bail!("Secret name must match [A-Za-z0-9_.-]+");
    }
    
    Ok(())
}

/// Convert a secret name to an environment variable name.
/// test-secret -> TEST_SECRET
/// slacks-app-token -> SLACKS_APP_TOKEN
fn secret_name_to_env_name(name: &str) -> String {
    name.to_uppercase().replace('-', "_")
}

/// Check file permissions on Unix systems.
/// Returns true if permissions are secure (file is 0600, dir is 0700).
#[cfg(unix)]
fn check_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    
    let metadata = fs::metadata(path)
        .with_context(|| format!("Cannot stat file: {}", path.display()))?;
    
    let permissions = metadata.permissions();
    let mode = permissions.mode() & 0o777;
    
    // For files: must be 0600 (owner read/write only)
    // For directories: must be 0700 (owner only)
    if path.is_file() {
        if mode != 0o600 {
            bail!(
                "Secret file {} has insecure permissions: {:o} (expected 0600)",
                path.display(),
                mode
            );
        }
    } else if path.is_dir() {
        if mode != 0o700 {
            bail!(
                "Secret directory {} has insecure permissions: {:o} (expected 0700)",
                path.display(),
                mode
            );
        }
    }
    
    Ok(())
}

#[cfg(not(unix))]
fn check_file_permissions(_path: &Path) -> Result<()> {
    // On non-Unix systems, we can't easily check permissions
    // Just pass for now (Phase 1)
    Ok(())
}

/// Read a secret from a file, trimming one trailing newline.
fn read_secret_file(path: &Path) -> Result<String> {
    check_file_permissions(path)?;
    
    let mut file = File::open(path)
        .with_context(|| format!("Cannot open secret file: {}", path.display()))?;
    
    let mut content = String::new();
    file.read_to_string(&mut content)
        .with_context(|| format!("Cannot read secret file: {}", path.display()))?;
    
    // Trim one trailing newline only
    let trimmed = if content.ends_with('\n') {
        content[..content.len() - 1].to_string()
    } else {
        content
    };
    
    Ok(trimmed)
}

/// Resolve a secret by name using the configured sources.
pub fn resolve(name: &str) -> Result<SecretValue> {
    resolve_with_sources(name, SecretSources::default())
}

/// Resolve a secret by name using custom sources.
pub fn resolve_with_sources(name: &str, sources: SecretSources) -> Result<SecretValue> {
    validate_secret_name(name)?;
    
    // 1. Try systemd LoadCredential
    if let Some(credentials_dir) = &sources.credentials_dir {
        let credential_path = credentials_dir.join(name);
        if credential_path.exists() {
            let value = read_secret_file(&credential_path)?;
            return Ok(SecretValue::new(value));
        }
    }
    
    // 2. Try file secret
    let secret_path = sources.secrets_dir.join(name);
    if secret_path.exists() {
        // Check directory permissions
        check_file_permissions(&sources.secrets_dir)?;
        let value = read_secret_file(&secret_path)?;
        return Ok(SecretValue::new(value));
    }
    
    // 3. Try environment variable
    let env_name = format!("{}{}", sources.env_prefix, secret_name_to_env_name(name));
    if let Ok(value) = std::env::var(&env_name) {
        if !value.is_empty() {
            return Ok(SecretValue::new(value));
        }
    }
    
    // Not found in any source
    bail!("Secret '{}' not found in any source", name);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use tempfile::tempdir;
    
    #[test]
    fn loadcredential_wins_over_file_and_env() {
        let _guard = crate::util::test_env_lock();
        
        // Save original env vars
        let orig_creds = env::var("CREDENTIALS_DIRECTORY").ok();
        let orig_env = env::var("BLACKBOX_SECRET_TEST_SECRET").ok();
        env::remove_var("CREDENTIALS_DIRECTORY");
        env::remove_var("BLACKBOX_SECRET_TEST_SECRET");
        
        let dir = tempdir().unwrap();
        let home = dir.path();
        
        // Set up a temp credentials directory
        let creds_dir = home.join("credentials");
        #[cfg(unix)] {
            fs::create_dir_all(&creds_dir).unwrap();
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&creds_dir).unwrap().permissions();
            perms.set_mode(0o700);
            fs::set_permissions(&creds_dir, perms).unwrap();
        }
        #[cfg(not(unix))]
        {
            fs::create_dir_all(&creds_dir).unwrap();
        }
        env::set_var("CREDENTIALS_DIRECTORY", &creds_dir.to_string_lossy().into_owned());
        
        // Create file secret that would conflict
        let secrets_dir = home.join("secrets");
        #[cfg(unix)] {
            fs::create_dir_all(&secrets_dir).unwrap();
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&secrets_dir).unwrap().permissions();
            perms.set_mode(0o700);
            fs::set_permissions(&secrets_dir, perms).unwrap();
        }
        #[cfg(not(unix))]
        {
            fs::create_dir_all(&secrets_dir).unwrap();
        }
        let secret_file = secrets_dir.join("test-secret");
        fs::write(&secret_file, "from-file").unwrap();
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&secret_file).unwrap().permissions();
            perms.set_mode(0o600);
            fs::set_permissions(&secret_file, perms).unwrap();
        }
        
        // Set env var that would conflict
        env::set_var("BLACKBOX_SECRET_TEST_SECRET", "from-env");
        
        // Create LoadCredential file
        let loadcred_file = creds_dir.join("test-secret");
        fs::write(&loadcred_file, "from-loadcredential").unwrap();
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&loadcred_file).unwrap().permissions();
            perms.set_mode(0o600);
            fs::set_permissions(&loadcred_file, perms).unwrap();
        }
        
        let sources = SecretSources {
            credentials_dir: Some(creds_dir.clone()),
            secrets_dir,
            env_prefix: "BLACKBOX_SECRET_".to_string(),
        };
        
        let secret = resolve_with_sources("test-secret", sources).unwrap();
        assert_eq!(secret.expose(), "from-loadcredential");
        
        // Restore original env vars
        if let Some(v) = orig_creds { env::set_var("CREDENTIALS_DIRECTORY", v); } else { env::remove_var("CREDENTIALS_DIRECTORY"); }
        if let Some(v) = orig_env { env::set_var("BLACKBOX_SECRET_TEST_SECRET", v); } else { env::remove_var("BLACKBOX_SECRET_TEST_SECRET"); }
    }
    
    #[test]
    fn file_secret_wins_over_env() {
        let _guard = crate::util::test_env_lock();
        
        // Save original env vars
        let orig_creds = env::var("CREDENTIALS_DIRECTORY").ok();
        let orig_env = env::var("BLACKBOX_SECRET_TEST_SECRET").ok();
        env::remove_var("CREDENTIALS_DIRECTORY");
        env::remove_var("BLACKBOX_SECRET_TEST_SECRET");
        
        let dir = tempdir().unwrap();
        let home = dir.path();
        
        // Create file secret with secure permissions
        let secrets_dir = home.join("secrets");
        #[cfg(unix)] {
            fs::create_dir_all(&secrets_dir).unwrap();
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&secrets_dir).unwrap().permissions();
            perms.set_mode(0o700);
            fs::set_permissions(&secrets_dir, perms).unwrap();
        }
        #[cfg(not(unix))]
        {
            fs::create_dir_all(&secrets_dir).unwrap();
        }
        let secret_path = secrets_dir.join("test-secret");
        fs::write(&secret_path, "from-file").unwrap();
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&secret_path).unwrap().permissions();
            perms.set_mode(0o600);
            fs::set_permissions(&secret_path, perms).unwrap();
        }
        
        // Set env var that would conflict
        env::set_var("BLACKBOX_SECRET_TEST_SECRET", "from-env");
        
        let sources = SecretSources {
            credentials_dir: None,
            secrets_dir,
            env_prefix: "BLACKBOX_SECRET_".to_string(),
        };
        
        let secret = resolve_with_sources("test-secret", sources).unwrap();
        assert_eq!(secret.expose(), "from-file");
        
        // Restore original env vars
        if let Some(v) = orig_creds { env::set_var("CREDENTIALS_DIRECTORY", v); } else { env::remove_var("CREDENTIALS_DIRECTORY"); }
        if let Some(v) = orig_env { env::set_var("BLACKBOX_SECRET_TEST_SECRET", v); } else { env::remove_var("BLACKBOX_SECRET_TEST_SECRET"); }
    }
    
    #[test]
    fn env_secret_fallback() {
        let _guard = crate::util::test_env_lock();
        
        // Save original env vars
        let orig_creds = env::var("CREDENTIALS_DIRECTORY").ok();
        let orig_env = env::var("BLACKBOX_SECRET_TEST_SECRET").ok();
        env::remove_var("CREDENTIALS_DIRECTORY");
        env::remove_var("BLACKBOX_SECRET_TEST_SECRET");
        
        let dir = tempdir().unwrap();
        let home = dir.path();
        
        // No file secret
        let secrets_dir = home.join("secrets");
        fs::create_dir_all(&secrets_dir).unwrap();
        
        // Set env var
        env::set_var("BLACKBOX_SECRET_TEST_SECRET", "from-env");
        
        let sources = SecretSources {
            credentials_dir: None,
            secrets_dir,
            env_prefix: "BLACKBOX_SECRET_".to_string(),
        };
        
        let secret = resolve_with_sources("test-secret", sources).unwrap();
        assert_eq!(secret.expose(), "from-env");
        
        // Restore original env vars
        if let Some(v) = orig_creds { env::set_var("CREDENTIALS_DIRECTORY", v); } else { env::remove_var("CREDENTIALS_DIRECTORY"); }
        if let Some(v) = orig_env { env::set_var("BLACKBOX_SECRET_TEST_SECRET", v); } else { env::remove_var("BLACKBOX_SECRET_TEST_SECRET"); }
    }
    
    #[test]
    fn rejects_secret_name_with_slash() {
        let _guard = crate::util::test_env_lock();
        
        // Save original env vars
        let orig_creds = env::var("CREDENTIALS_DIRECTORY").ok();
        env::remove_var("CREDENTIALS_DIRECTORY");
        
        let result = resolve("test/secret");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("path separators"));
        
        // Restore original env vars
        if let Some(v) = orig_creds { env::set_var("CREDENTIALS_DIRECTORY", v); } else { env::remove_var("CREDENTIALS_DIRECTORY"); }
    }
    
    #[test]
    fn rejects_world_readable_secret_file() {
        let _guard = crate::util::test_env_lock();
        
        // Save original env vars
        let orig_creds = env::var("CREDENTIALS_DIRECTORY").ok();
        env::remove_var("CREDENTIALS_DIRECTORY");
        
        #[cfg(unix)] {
            let dir = tempdir().unwrap();
            let secrets_dir = dir.path().join("secrets");
            fs::create_dir_all(&secrets_dir).unwrap();
            
            let secret_path = secrets_dir.join("test-secret");
            fs::write(&secret_path, "secret-value").unwrap();
            
            // Make it world-readable
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&secret_path).unwrap().permissions();
            perms.set_mode(0o644);
            fs::set_permissions(&secret_path, perms).unwrap();
            
            let sources = SecretSources {
                credentials_dir: None,
                secrets_dir,
                env_prefix: "BLACKBOX_SECRET_".to_string(),
            };
            
            let result = resolve_with_sources("test-secret", sources);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("insecure permissions"));
        }
        
        // Restore original env vars
        if let Some(v) = orig_creds { env::set_var("CREDENTIALS_DIRECTORY", v); } else { env::remove_var("CREDENTIALS_DIRECTORY"); }
    }
    
    #[test]
    fn rejects_world_searchable_secret_dir() {
        let _guard = crate::util::test_env_lock();
        
        // Save original env vars
        let orig_creds = env::var("CREDENTIALS_DIRECTORY").ok();
        env::remove_var("CREDENTIALS_DIRECTORY");
        
        #[cfg(unix)] {
            let dir = tempdir().unwrap();
            let secrets_dir = dir.path().join("secrets");
            fs::create_dir_all(&secrets_dir).unwrap();
            
            let secret_path = secrets_dir.join("test-secret");
            fs::write(&secret_path, "secret-value").unwrap();
            
            // Make dir world-searchable
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&secrets_dir).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&secrets_dir, perms).unwrap();
            
            let sources = SecretSources {
                credentials_dir: None,
                secrets_dir,
                env_prefix: "BLACKBOX_SECRET_".to_string(),
            };
            
            let result = resolve_with_sources("test-secret", sources);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("insecure permissions"));
        }
        
        // Restore original env vars
        if let Some(v) = orig_creds { env::set_var("CREDENTIALS_DIRECTORY", v); } else { env::remove_var("CREDENTIALS_DIRECTORY"); }
    }
    
    #[test]
    fn missing_secret_errors_with_name_not_value() {
        let _guard = crate::util::test_env_lock();
        
        // Save original env vars
        let orig_creds = env::var("CREDENTIALS_DIRECTORY").ok();
        let orig_env = env::var("BLACKBOX_SECRET_NONEXISTENT").ok();
        env::remove_var("CREDENTIALS_DIRECTORY");
        env::remove_var("BLACKBOX_SECRET_NONEXISTENT");
        
        let dir = tempdir().unwrap();
        let secrets_dir = dir.path().join("secrets");
        #[cfg(unix)] {
            fs::create_dir_all(&secrets_dir).unwrap();
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&secrets_dir).unwrap().permissions();
            perms.set_mode(0o700);
            fs::set_permissions(&secrets_dir, perms).unwrap();
        }
        #[cfg(not(unix))]
        {
            fs::create_dir_all(&secrets_dir).unwrap();
        }
        
        let sources = SecretSources {
            credentials_dir: None,
            secrets_dir,
            env_prefix: "BLACKBOX_SECRET_".to_string(),
        };
        
        let result = resolve_with_sources("nonexistent", sources);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
        assert!(!err.to_string().contains("from-env") && !err.to_string().contains("from-file"));
        
        // Restore original env vars
        if let Some(v) = orig_creds { env::set_var("CREDENTIALS_DIRECTORY", v); } else { env::remove_var("CREDENTIALS_DIRECTORY"); }
        if let Some(v) = orig_env { env::set_var("BLACKBOX_SECRET_NONEXISTENT", v); } else { env::remove_var("BLACKBOX_SECRET_NONEXISTENT"); }
    }
}
