//! 单机归属启动：密钥必须稳定且受保护，不随机生成或降级托管 authority。
use std::{io::Read, path::Path, sync::Arc};

use aisix_core::{config::StandaloneNativeObjectsConfig, snapshot::SnapshotHandle, AisixSnapshot};
use aisix_proxy::native_api::standalone_store::StandaloneNativeStore;

pub async fn open(
    config: &StandaloneNativeObjectsConfig,
    snapshot: SnapshotHandle<AisixSnapshot>,
) -> anyhow::Result<Arc<StandaloneNativeStore>> {
    #[cfg(unix)]
    let mut file = {
        use rustix::fs::{open, Mode, OFlags};
        use std::os::unix::fs::PermissionsExt;
        let descriptor = open(
            Path::new(&config.origin_secret_file),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| {
            anyhow::anyhow!(
                "native_objects.origin_secret_file cannot be read as a protected regular file"
            )
        })?;
        let file = std::fs::File::from(descriptor);
        let metadata = file.metadata().map_err(|_| {
            anyhow::anyhow!("native_objects.origin_secret_file cannot be inspected")
        })?;
        if !metadata.is_file()
            || metadata.len() > 1024
            || metadata.permissions().mode() & 0o077 != 0
        {
            anyhow::bail!("native_objects.origin_secret_file must be a protected regular file containing a stable random secret");
        }
        file.take(1025)
    };
    #[cfg(not(unix))]
    {
        anyhow::bail!("standalone native object storage requires Unix file protections");
    }
    #[cfg(unix)]
    {
        let mut value = String::new();
        file.read_to_string(&mut value)
            .map_err(|_| anyhow::anyhow!("native_objects.origin_secret_file cannot be read"))?;
        if value.len() > 1024 || super::decoded_secret_len(value.trim()) < 32 {
            anyhow::bail!("native_objects.origin_secret_file must contain at least 32 random bytes written as hex or base64");
        }
        let key = aisix_core::native_objects::platform_id_key_from_secret(value.trim());
        StandaloneNativeStore::open(Path::new(&config.sqlite_path),key,snapshot).await.map(Arc::new)
            .map_err(|_|anyhow::anyhow!("standalone native object authority could not be opened; check its protected local database, schema version and stable origin secret"))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use aisix_proxy::native_api::registry::NativeObjectRegistry;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn fixture() -> (tempfile::TempDir, StandaloneNativeObjectsConfig) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let secret = directory.path().join("origin-secret");
        std::fs::write(&secret, "a3".repeat(32)).unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        let config = StandaloneNativeObjectsConfig {
            sqlite_path: directory
                .path()
                .join("native.sqlite3")
                .to_string_lossy()
                .into_owned(),
            origin_secret_file: secret.to_string_lossy().into_owned(),
        };
        (directory, config)
    }

    #[tokio::test]
    async fn restarting_uses_the_protected_stable_secret() {
        let (_directory, config) = fixture();
        let snapshot = SnapshotHandle::new(AisixSnapshot::new());
        let first = open(&config, snapshot.clone()).await.unwrap();
        let key = first.id_key();
        first.close().await;
        let second = open(&config, snapshot).await.unwrap();
        assert_eq!(key, second.id_key());
        assert_eq!(
            key,
            Some(aisix_core::native_objects::platform_id_key_from_secret(
                &"a3".repeat(32)
            ))
        );
    }

    #[tokio::test]
    async fn missing_weak_public_or_symlinked_secrets_never_boot_or_print_the_secret() {
        let (directory, config) = fixture();
        let snapshot = SnapshotHandle::new(AisixSnapshot::new());
        std::fs::set_permissions(
            &config.origin_secret_file,
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let error = open(&config, snapshot.clone())
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(!error.contains(&"a3".repeat(32)));
        std::fs::set_permissions(
            &config.origin_secret_file,
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        std::fs::write(&config.origin_secret_file, "short-secret").unwrap();
        assert!(open(&config, snapshot.clone()).await.is_err());
        let mut missing = config.clone();
        missing.origin_secret_file = directory
            .path()
            .join("missing")
            .to_string_lossy()
            .into_owned();
        assert!(open(&missing, snapshot.clone()).await.is_err());
        let link = directory.path().join("symlink-secret");
        symlink(&config.origin_secret_file, &link).unwrap();
        missing.origin_secret_file = link.to_string_lossy().into_owned();
        assert!(open(&missing, snapshot).await.is_err());
    }
}
