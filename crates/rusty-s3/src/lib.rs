//! S3 object storage for message bodies (docs/design-s3-per-tenant.md, step 2).
//!
//! * [`sigv4`]: AWS Signature Version 4, checked against the published test vector.
//! * [`http`]: a small HTTP/1.1 client over TCP or TLS: no redirects, no proxies, bounded headers and bodies, a deadline on every request, and a [`Connector`] seam through which
//!   the address policy for tenant-chosen endpoints is applied (step 3).
//! * [`client`]: the four calls S3 needs here (put, get, head and delete, list), with the failures sorted into the fixed categories of [`S3Fail`].
//! * [`store`]: [`S3BlobStore`], the `BlobStore` over one bucket and prefix.
#![forbid(unsafe_code)]

pub mod client;
pub mod error;
pub mod http;
pub mod sigv4;
pub mod store;
mod time;
mod xml;

pub use client::{Credentials, ListPage, ObjectMeta, S3Client, S3Config};
pub use error::S3Fail;
pub use http::{Connector, DirectConnector};
pub use store::S3BlobStore;
