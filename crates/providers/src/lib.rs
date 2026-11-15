//! Provider integrations (Bitbucket Cloud first).

pub mod bb_client;
pub mod bitbucket;
pub mod github;
pub mod gitlab;
mod html_text;
pub mod http;
pub mod provider;

pub use bb_client::{BbClient, CERT_UNTRUSTED};
pub use provider::{Provider, ProviderError};
