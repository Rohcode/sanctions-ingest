//! Cloudflare R2 archive (S3 API, via aws-sdk-s3). Content-addressed: the object
//! key is `sanctions/dfat/{sha256}.xlsx`, so the bytes we screened against are
//! recoverable and hash-verifiable. The ingest credential is a
//! WRITE-ONLY, prefix-scoped token with NO overwrite/lock/bucket-config rights —
//! enforced by the token policy and verified by an overwrite-denial check.

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;

pub struct R2 {
    client: Client,
    bucket: String,
}

impl R2 {
    pub fn new(endpoint: &str, access_key: &str, secret: &str, bucket: &str) -> Self {
        let creds = Credentials::new(access_key, secret, None, None, "r2-ingest");
        let conf = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .endpoint_url(endpoint)
            .region(Region::new("auto"))
            .credentials_provider(creds)
            .force_path_style(true)
            .build();
        Self {
            client: Client::from_conf(conf),
            bucket: bucket.to_string(),
        }
    }

    /// Archive the bytes at `key` if not already present. Content-addressing means
    /// an existing object has identical bytes, so we never need to overwrite — and
    /// the write-only token is denied overwrite anyway. Returns true if uploaded.
    pub async fn archive_if_absent(
        &self,
        key: &str,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> Result<bool, String> {
        // HEAD may be denied for a write-only token; treat "denied/absent" as
        // "not known present" and attempt the PUT (idempotent for identical bytes).
        if self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .is_ok()
        {
            return Ok(false); // already archived
        }

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(bytes))
            .content_type(content_type)
            .send()
            .await
            .map_err(|e| format!("R2 put_object failed: {e}"))?;
        Ok(true)
    }
}
