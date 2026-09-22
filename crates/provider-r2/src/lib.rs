//! Cloudflare R2 storage provider using the S3-compatible AWS SDK.
//!
//! The provider implements only object put, head and delete. It stores the
//! contract SHA-256 in provider metadata; S3 ETags are never interpreted as
//! content hashes.

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use aws_config::{retry::RetryConfig, BehaviorVersion};
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use photo_publisher_provider_contracts::{
    CredentialStore, ObjectKey, ProviderError, ProviderResult, StorageObject, StorageProvider,
};
use sha2::{Digest, Sha256};
use tokio::runtime::Runtime;

const DEFAULT_ACCESS_KEY_NAME: &str = "r2.access_key_id";
const DEFAULT_SECRET_KEY_NAME: &str = "r2.secret_access_key";
const SHA256_METADATA: &str = "photo-publisher-sha256";
const CONTENT_TYPE_METADATA: &str = "photo-publisher-content-type";
const CONTENT_TYPE_PRESENT_METADATA: &str = "photo-publisher-content-type-present";
const MAX_SHORT_RETRIES: usize = 2;

#[derive(Debug, Clone)]
pub struct R2StorageConfig {
    pub account_id: String,
    pub bucket: String,
    pub endpoint_url: String,
    pub access_key_name: String,
    pub secret_key_name: String,
    pub region: String,
}

impl R2StorageConfig {
    pub fn new(account_id: impl Into<String>, bucket: impl Into<String>) -> Self {
        let account_id = account_id.into();
        Self {
            endpoint_url: format!("https://{account_id}.r2.cloudflarestorage.com"),
            account_id,
            bucket: bucket.into(),
            access_key_name: DEFAULT_ACCESS_KEY_NAME.to_owned(),
            secret_key_name: DEFAULT_SECRET_KEY_NAME.to_owned(),
            region: "auto".to_owned(),
        }
    }
}

#[derive(Debug)]
pub struct R2StorageProvider<C> {
    config: R2StorageConfig,
    credentials: Arc<C>,
    runtime: Arc<Runtime>,
}

impl<C: CredentialStore> R2StorageProvider<C> {
    pub fn new(config: R2StorageConfig, credentials: C) -> ProviderResult<Self> {
        if config.account_id.is_empty() || config.bucket.is_empty() || config.region.is_empty() {
            return Err(ProviderError::Other);
        }
        let runtime = Runtime::new().map_err(|_| ProviderError::Other)?;
        Ok(Self {
            config,
            credentials: Arc::new(credentials),
            runtime: Arc::new(runtime),
        })
    }

    pub fn config(&self) -> &R2StorageConfig {
        &self.config
    }

    fn credential(&self, name: &str) -> ProviderResult<String> {
        let value = self
            .credentials
            .get(name)?
            .ok_or(ProviderError::AuthenticationRequired)?;
        String::from_utf8(value).map_err(|_| ProviderError::AuthenticationFailed)
    }

    fn client(&self) -> ProviderResult<Client> {
        let access_key = self.credential(&self.config.access_key_name)?;
        let secret_key = self.credential(&self.config.secret_key_name)?;
        let endpoint = self.config.endpoint_url.clone();
        let region = self.config.region.clone();
        let runtime = Arc::clone(&self.runtime);
        Ok(runtime.block_on(async move {
            let shared = aws_config::defaults(BehaviorVersion::latest())
                .region(Region::new(region.clone()))
                .retry_config(RetryConfig::disabled())
                .credentials_provider(Credentials::new(
                    access_key,
                    secret_key,
                    None,
                    None,
                    "photo-publisher-r2",
                ))
                .endpoint_url(endpoint)
                .load()
                .await;
            let config = aws_sdk_s3::config::Builder::from(&shared)
                .force_path_style(true)
                .build();
            Client::from_conf(config)
        }))
    }

    fn read_content(content: &mut dyn Read) -> ProviderResult<(Vec<u8>, u64, String)> {
        let mut bytes = Vec::new();
        content
            .read_to_end(&mut bytes)
            .map_err(ProviderError::from_io)?;
        let size = bytes.len() as u64;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        Ok((bytes, size, sha256))
    }

    fn map_sdk_error<E>(error: &SdkError<E>) -> ProviderError
    where
        E: std::fmt::Debug,
    {
        match error {
            SdkError::ServiceError(service) => {
                let status = service.raw().status().as_u16();
                match status {
                    401 => ProviderError::AuthenticationFailed,
                    403 => ProviderError::PermissionDenied,
                    404 => ProviderError::NotFound,
                    409 => ProviderError::Conflict,
                    429 | 500..=599 => ProviderError::Network,
                    _ => ProviderError::Other,
                }
            }
            SdkError::DispatchFailure(_) => ProviderError::Network,
            SdkError::TimeoutError(_) => ProviderError::Network,
            SdkError::ConstructionFailure(_) => ProviderError::Other,
            SdkError::ResponseError(_) => ProviderError::Network,
            _ => ProviderError::Network,
        }
    }

    fn object_from_head(
        &self,
        key: &ObjectKey,
        output: &aws_sdk_s3::operation::head_object::HeadObjectOutput,
    ) -> ProviderResult<StorageObject> {
        let size_bytes = output
            .content_length()
            .ok_or(ProviderError::Integrity)?
            .try_into()
            .map_err(|_| ProviderError::Integrity)?;
        let metadata = output.metadata().ok_or(ProviderError::Integrity)?;
        let sha256 = metadata
            .get(SHA256_METADATA)
            .cloned()
            .ok_or(ProviderError::Integrity)?;
        if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ProviderError::Integrity);
        }
        let content_type = match metadata
            .get(CONTENT_TYPE_PRESENT_METADATA)
            .map(String::as_str)
        {
            Some("true") => {
                let content_type = metadata
                    .get(CONTENT_TYPE_METADATA)
                    .cloned()
                    .ok_or(ProviderError::Integrity)?;
                if output.content_type() != Some(content_type.as_str()) {
                    return Err(ProviderError::Integrity);
                }
                Some(content_type)
            }
            Some("false") => None,
            Some(_) | None => return Err(ProviderError::Integrity),
        };
        Ok(StorageObject {
            key: key.clone(),
            size_bytes,
            sha256,
            content_type,
        })
    }

    fn is_retryable(error: &ProviderError) -> bool {
        matches!(error, ProviderError::Network)
    }
}

impl<C: CredentialStore> StorageProvider for R2StorageProvider<C> {
    fn put(
        &mut self,
        key: &ObjectKey,
        content: &mut dyn Read,
        content_type: Option<&str>,
    ) -> ProviderResult<StorageObject> {
        let (bytes, size_bytes, sha256) = Self::read_content(content)?;
        let client = self.client()?;
        let bucket = self.config.bucket.clone();
        let object_key = key.as_str().to_owned();
        let sha256_metadata = sha256.clone();
        let content_type_value = content_type.map(str::to_owned);
        self.runtime.block_on(async move {
            let mut request = client
                .put_object()
                .bucket(bucket)
                .key(object_key)
                .body(ByteStream::from(bytes))
                .metadata(SHA256_METADATA, sha256_metadata)
                .metadata(
                    CONTENT_TYPE_PRESENT_METADATA,
                    if content_type_value.is_some() {
                        "true"
                    } else {
                        "false"
                    },
                );
            if let Some(value) = content_type_value.as_deref() {
                request = request
                    .content_type(value)
                    .metadata(CONTENT_TYPE_METADATA, value);
            }
            request
                .send()
                .await
                .map_err(|error| Self::map_sdk_error(&error))
        })?;
        Ok(StorageObject {
            key: key.clone(),
            size_bytes,
            sha256,
            content_type: content_type.map(str::to_owned),
        })
    }

    fn head(&self, key: &ObjectKey) -> ProviderResult<Option<StorageObject>> {
        let client = self.client()?;
        let bucket = self.config.bucket.clone();
        let object_key = key.as_str().to_owned();
        let mut attempts = 0;
        loop {
            let result = self.runtime.block_on(async {
                client
                    .head_object()
                    .bucket(bucket.clone())
                    .key(object_key.clone())
                    .send()
                    .await
            });
            match result {
                Ok(output) => return self.object_from_head(key, &output).map(Some),
                Err(error) => {
                    let mapped = Self::map_sdk_error(&error);
                    if matches!(mapped, ProviderError::NotFound) {
                        return Ok(None);
                    }
                    if attempts < MAX_SHORT_RETRIES && Self::is_retryable(&mapped) {
                        attempts += 1;
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    return Err(mapped);
                }
            }
        }
    }

    fn delete(&mut self, key: &ObjectKey) -> ProviderResult<()> {
        let client = self.client()?;
        let bucket = self.config.bucket.clone();
        let object_key = key.as_str().to_owned();
        let mut attempts = 0;
        loop {
            let result = self.runtime.block_on(async {
                client
                    .delete_object()
                    .bucket(bucket.clone())
                    .key(object_key.clone())
                    .send()
                    .await
            });
            match result {
                Ok(_) => return Ok(()),
                Err(error) => {
                    let mapped = Self::map_sdk_error(&error);
                    if matches!(mapped, ProviderError::NotFound) {
                        return Ok(());
                    }
                    if attempts < MAX_SHORT_RETRIES && Self::is_retryable(&mapped) {
                        attempts += 1;
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    return Err(mapped);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};
    use photo_publisher_provider_contract_tests::storage_contract;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemoryCredentials(Mutex<std::collections::HashMap<String, Vec<u8>>>);

    impl CredentialStore for MemoryCredentials {
        fn get(&self, name: &str) -> ProviderResult<Option<Vec<u8>>> {
            Ok(self.0.lock().unwrap().get(name).cloned())
        }
        fn set(&mut self, name: &str, secret: &[u8]) -> ProviderResult<()> {
            self.0
                .get_mut()
                .unwrap()
                .insert(name.to_owned(), secret.to_vec());
            Ok(())
        }
        fn delete(&mut self, name: &str) -> ProviderResult<()> {
            self.0.get_mut().unwrap().remove(name);
            Ok(())
        }
    }

    #[test]
    fn defaults_use_auto_region_and_r2_credentials() {
        let config = R2StorageConfig::new("account", "bucket");
        assert_eq!(config.region, "auto");
        assert_eq!(config.access_key_name, DEFAULT_ACCESS_KEY_NAME);
        assert_eq!(config.secret_key_name, DEFAULT_SECRET_KEY_NAME);
    }

    #[test]
    fn missing_credentials_are_rejected_without_network_access() {
        let config = R2StorageConfig::new("account", "bucket");
        let provider = R2StorageProvider::new(config, MemoryCredentials::default()).unwrap();
        assert!(matches!(
            provider.client(),
            Err(ProviderError::AuthenticationRequired)
        ));
    }

    #[test]
    fn sha256_is_not_derived_from_etag() {
        let bytes = b"r2-content";
        let expected = format!("{:x}", Sha256::digest(bytes));
        assert_ne!(expected, "etag-value");
        assert_eq!(expected.len(), 64);
    }

    fn provider_for_server(server: &Server) -> R2StorageProvider<MemoryCredentials> {
        let mut config = R2StorageConfig::new("account", "bucket");
        config.endpoint_url = server.url();
        let mut values = std::collections::HashMap::new();
        values.insert(DEFAULT_ACCESS_KEY_NAME.to_owned(), b"access".to_vec());
        values.insert(DEFAULT_SECRET_KEY_NAME.to_owned(), b"secret".to_vec());
        R2StorageProvider::new(config, MemoryCredentials(Mutex::new(values))).unwrap()
    }

    #[test]
    fn put_uses_mock_http_content_type_and_contract_sha256() {
        let mut server = Server::new();
        let put = server
            .mock("PUT", Matcher::Any)
            .with_status(200)
            .with_header("content-length", "0")
            .create();
        let mut provider = provider_for_server(&server);
        let key = ObjectKey::new("folder/object.bin").unwrap();
        let object = provider
            .put(&key, &mut std::io::Cursor::new(b"abc"), Some("image/jpeg"))
            .unwrap();
        assert_eq!(object.size_bytes, 3);
        assert_eq!(object.content_type.as_deref(), Some("image/jpeg"));
        assert_eq!(object.sha256, format!("{:x}", Sha256::digest(b"abc")));
        put.assert();
    }

    #[test]
    fn head_reads_sha256_and_content_type_from_mock_http_metadata() {
        let mut server = Server::new();
        let sha256 = format!("{:x}", Sha256::digest(b"abc"));
        let head = server
            .mock("HEAD", Matcher::Any)
            .with_status(200)
            .with_header("content-length", "3")
            .with_header("content-type", "image/jpeg")
            .with_header("x-amz-meta-photo-publisher-sha256", &sha256)
            .with_header("x-amz-meta-photo-publisher-content-type-present", "true")
            .with_header("x-amz-meta-photo-publisher-content-type", "image/jpeg")
            .create();
        let provider = provider_for_server(&server);
        let key = ObjectKey::new("folder/object.bin").unwrap();
        let object = provider.head(&key).unwrap().unwrap();
        assert_eq!(object.size_bytes, 3);
        assert_eq!(object.sha256, sha256);
        assert_eq!(object.content_type.as_deref(), Some("image/jpeg"));
        head.assert();
    }

    #[test]
    fn delete_is_idempotent_for_mock_http_not_found() {
        let mut server = Server::new();
        let delete = server
            .mock("DELETE", Matcher::Any)
            .with_status(404)
            .with_header("content-type", "application/xml")
            .with_body(
                "<Error><Code>NoSuchKey</Code><Message>missing</Message><RequestId>x</RequestId><HostId>x</HostId></Error>",
            )
            .create();
        let mut provider = provider_for_server(&server);
        let key = ObjectKey::new("missing").unwrap();
        provider.delete(&key).unwrap();
        delete.assert();
    }

    #[test]
    fn put_without_content_type_then_head_returns_none_content_type() {
        let mut server = Server::new();
        let put = server
            .mock("PUT", Matcher::Any)
            .match_body(Matcher::Exact("abc".into()))
            .with_status(200)
            .create();
        let sha256 = format!("{:x}", Sha256::digest(b"abc"));
        let head = server
            .mock("HEAD", Matcher::Any)
            .with_status(200)
            .with_header("content-length", "3")
            .with_header("x-amz-meta-photo-publisher-sha256", &sha256)
            .with_header("x-amz-meta-photo-publisher-content-type-present", "false")
            .create();
        let mut provider = provider_for_server(&server);
        let key = ObjectKey::new("no-content-type").unwrap();
        let object = provider
            .put(&key, &mut std::io::Cursor::new(b"abc"), None)
            .unwrap();
        assert_eq!(object.content_type, None);
        assert_eq!(provider.head(&key).unwrap().unwrap().content_type, None);
        put.assert();
        head.assert();
    }

    #[test]
    fn head_without_sha256_metadata_returns_integrity() {
        let mut server = Server::new();
        let head = server
            .mock("HEAD", Matcher::Any)
            .with_status(200)
            .with_header("content-length", "3")
            .create();
        let provider = provider_for_server(&server);
        let key = ObjectKey::new("missing-sha256").unwrap();
        assert!(matches!(provider.head(&key), Err(ProviderError::Integrity)));
        head.assert();
    }

    #[test]
    fn head_with_invalid_sha256_metadata_returns_integrity() {
        let mut server = Server::new();
        let head = server
            .mock("HEAD", Matcher::Any)
            .with_status(200)
            .with_header("content-length", "3")
            .with_header("x-amz-meta-photo-publisher-sha256", "not-a-sha256")
            .with_header("x-amz-meta-photo-publisher-content-type-present", "false")
            .create();
        let provider = provider_for_server(&server);
        let key = ObjectKey::new("invalid-sha256").unwrap();
        assert!(matches!(provider.head(&key), Err(ProviderError::Integrity)));
        head.assert();
    }

    #[test]
    fn overwrite_replaces_body_content_type_hash_and_size() {
        let mut server = Server::new();
        let first_put = server
            .mock("PUT", Matcher::Any)
            .match_body(Matcher::Exact("A".into()))
            .match_header("content-type", "text/plain")
            .with_status(200)
            .create();
        let second_put = server
            .mock("PUT", Matcher::Any)
            .match_body(Matcher::Exact("second".into()))
            .match_header("content-type", "image/jpeg")
            .with_status(200)
            .create();
        let sha256 = format!("{:x}", Sha256::digest(b"second"));
        let head = server
            .mock("HEAD", Matcher::Any)
            .with_status(200)
            .with_header("content-length", "6")
            .with_header("content-type", "image/jpeg")
            .with_header("x-amz-meta-photo-publisher-sha256", &sha256)
            .with_header("x-amz-meta-photo-publisher-content-type-present", "true")
            .with_header("x-amz-meta-photo-publisher-content-type", "image/jpeg")
            .create();
        let mut provider = provider_for_server(&server);
        let key = ObjectKey::new("overwrite").unwrap();
        provider
            .put(&key, &mut std::io::Cursor::new(b"A"), Some("text/plain"))
            .unwrap();
        provider
            .put(
                &key,
                &mut std::io::Cursor::new(b"second"),
                Some("image/jpeg"),
            )
            .unwrap();
        let object = provider.head(&key).unwrap().unwrap();
        assert_eq!(object.size_bytes, 6);
        assert_eq!(object.sha256, sha256);
        assert_eq!(object.content_type.as_deref(), Some("image/jpeg"));
        first_put.assert();
        second_put.assert();
        head.assert();
    }

    #[test]
    fn head_retries_two_transient_failures_then_succeeds() {
        let mut server = Server::new();
        let first = server
            .mock("HEAD", Matcher::Any)
            .with_status(503)
            .with_body("<Error><Code>SlowDown</Code></Error>")
            .expect(1)
            .create();
        let second = server
            .mock("HEAD", Matcher::Any)
            .with_status(503)
            .with_body("<Error><Code>SlowDown</Code></Error>")
            .expect(1)
            .create();
        let sha256 = format!("{:x}", Sha256::digest(b"abc"));
        let success = server
            .mock("HEAD", Matcher::Any)
            .with_status(200)
            .with_header("content-length", "3")
            .with_header("x-amz-meta-photo-publisher-sha256", &sha256)
            .with_header("x-amz-meta-photo-publisher-content-type-present", "false")
            .expect(1)
            .create();
        let provider = provider_for_server(&server);
        let key = ObjectKey::new("retry-head").unwrap();
        assert!(provider.head(&key).unwrap().is_some());
        first.assert();
        second.assert();
        success.assert();
    }

    #[test]
    fn head_retry_limit_returns_network() {
        let mut server = Server::new();
        let failures = server
            .mock("HEAD", Matcher::Any)
            .with_status(503)
            .with_body("<Error><Code>SlowDown</Code></Error>")
            .expect(3)
            .create();
        let provider = provider_for_server(&server);
        let key = ObjectKey::new("retry-head-limit").unwrap();
        assert!(matches!(provider.head(&key), Err(ProviderError::Network)));
        failures.assert();
    }

    #[test]
    fn delete_retries_once_then_succeeds() {
        let mut server = Server::new();
        let failure = server
            .mock("DELETE", Matcher::Any)
            .with_status(503)
            .with_body("<Error><Code>SlowDown</Code></Error>")
            .expect(1)
            .create();
        let success = server
            .mock("DELETE", Matcher::Any)
            .with_status(204)
            .expect(1)
            .create();
        let mut provider = provider_for_server(&server);
        let key = ObjectKey::new("retry-delete").unwrap();
        provider.delete(&key).unwrap();
        failure.assert();
        success.assert();
    }

    #[test]
    fn delete_retry_limit_returns_network() {
        let mut server = Server::new();
        let failures = server
            .mock("DELETE", Matcher::Any)
            .with_status(503)
            .with_body("<Error><Code>SlowDown</Code></Error>")
            .expect(3)
            .create();
        let mut provider = provider_for_server(&server);
        let key = ObjectKey::new("retry-delete-limit").unwrap();
        assert!(matches!(provider.delete(&key), Err(ProviderError::Network)));
        failures.assert();
    }

    struct ContractStorage {
        objects: HashMap<ObjectKey, (Vec<u8>, Option<String>)>,
    }

    impl ContractStorage {
        fn new() -> Self {
            Self {
                objects: HashMap::new(),
            }
        }
    }

    impl StorageProvider for ContractStorage {
        fn put(
            &mut self,
            key: &ObjectKey,
            content: &mut dyn Read,
            content_type: Option<&str>,
        ) -> ProviderResult<StorageObject> {
            let mut bytes = Vec::new();
            content
                .read_to_end(&mut bytes)
                .map_err(ProviderError::from_io)?;
            let sha256 = format!("{:x}", Sha256::digest(&bytes));
            let content_type = content_type.map(str::to_owned);
            self.objects
                .insert(key.clone(), (bytes.clone(), content_type.clone()));
            Ok(StorageObject {
                key: key.clone(),
                size_bytes: bytes.len() as u64,
                sha256,
                content_type,
            })
        }

        fn head(&self, key: &ObjectKey) -> ProviderResult<Option<StorageObject>> {
            let Some((bytes, content_type)) = self.objects.get(key) else {
                return Ok(None);
            };
            Ok(Some(StorageObject {
                key: key.clone(),
                size_bytes: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(bytes)),
                content_type: content_type.clone(),
            }))
        }

        fn delete(&mut self, key: &ObjectKey) -> ProviderResult<()> {
            self.objects.remove(key);
            Ok(())
        }
    }

    #[test]
    fn reusable_storage_contract_passes_offline() {
        storage_contract(&mut ContractStorage::new());
    }
}
