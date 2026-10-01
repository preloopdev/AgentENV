use anyhow::Result;
use aws_config::{meta::region::RegionProviderChain, BehaviorVersion};
use aws_sdk_s3::config::Credentials;
use aws_sdk_s3::Client as S3Client;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::minio::MinIO;

pub const MINIO_USER: &str = "minioadmin";
pub const MINIO_PASS: &str = "minioadmin";
pub const REGION: &str = "us-east-1";
pub const BUCKET: &str = "test-bucket";

pub struct MinioFixture {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub client: S3Client,
    _container: ContainerAsync<MinIO>,
}

impl MinioFixture {
    pub async fn start() -> Result<Self> {
        let container = MinIO::default().start().await?;
        Self::from_container(container).await
    }

    /// Start MinIO bound to a fixed host port.
    ///
    /// Used to stop and restart the primary on the same endpoint in failover
    /// tests. The caller must ensure `port` is free.
    pub async fn start_on_port(port: u16) -> Result<Self> {
        let container = MinIO::default()
            .with_mapped_port(port, 9000.into())
            .start()
            .await?;
        Self::from_container(container).await
    }

    async fn from_container(container: ContainerAsync<MinIO>) -> Result<Self> {
        let port = container.get_host_port_ipv4(9000).await?;
        let endpoint = format!("http://127.0.0.1:{port}");
        let client = build_s3_client(&endpoint).await;
        client.create_bucket().bucket(BUCKET).send().await?;

        Ok(Self {
            endpoint,
            bucket: BUCKET.to_string(),
            region: REGION.to_string(),
            client,
            _container: container,
        })
    }

    /// Stop the container without removing it, keeping its mapped host port.
    pub async fn stop(&self) -> Result<()> {
        self._container.stop().await?;
        Ok(())
    }

    /// Restart a previously stopped container at the same endpoint.
    pub async fn restart(&self) -> Result<()> {
        self._container.start().await?;
        Ok(())
    }

    /// Delete an object, ignoring absence.
    pub async fn delete_object(&self, key: &str) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await?;
        Ok(())
    }

    pub async fn object_exists(&self, key: &str) -> Result<bool> {
        let result = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await;
        Ok(result.is_ok())
    }

    /// Write an object with default content type.
    pub async fn put_object(&self, key: &str, body: Vec<u8>) -> Result<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(body.into())
            .send()
            .await?;
        Ok(())
    }

    /// Read an object's full body.
    pub async fn get_object(&self, key: &str) -> Result<Vec<u8>> {
        let output = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await?;
        let bytes = output.body.collect().await?;
        Ok(bytes.into_bytes().to_vec())
    }

    pub fn object_url(&self, key: &str) -> String {
        format!(
            "s3://{}/{}?endpoint={}&region={}",
            self.bucket, key, self.endpoint, self.region
        )
    }
}

async fn build_s3_client(endpoint: &str) -> S3Client {
    let region_provider = RegionProviderChain::default_provider().or_else(REGION);
    let creds = Credentials::new(MINIO_USER, MINIO_PASS, None, None, "agentenv-tests");
    let shared_config = aws_config::defaults(BehaviorVersion::latest())
        .region(region_provider)
        .endpoint_url(endpoint)
        .credentials_provider(creds)
        .load()
        .await;
    S3Client::new(&shared_config)
}
