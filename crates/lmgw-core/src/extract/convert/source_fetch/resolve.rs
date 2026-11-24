//! Name resolution for web sources: a trait, so the fetcher resolves
//! through the system (`getaddrinfo`, with `/etc/hosts`) in the gateway and
//! through a fixed table in tests, which never touch the machine's DNS.
//! The fetcher resolves once per hop and connects to exactly the addresses
//! it was answered and checked.

use std::net::IpAddr;

use futures::future::BoxFuture;

/// Turns a host name into its addresses.
pub trait Resolve: Send + Sync {
    fn lookup(&self, host: &str, port: u16) -> BoxFuture<'static, std::io::Result<Vec<IpAddr>>>;
}

/// The system's resolver.
pub struct System;

impl Resolve for System {
    fn lookup(&self, host: &str, port: u16) -> BoxFuture<'static, std::io::Result<Vec<IpAddr>>> {
        let host = host.to_string();
        Box::pin(async move {
            Ok(tokio::net::lookup_host((host.as_str(), port))
                .await?
                .map(|s| s.ip())
                .collect())
        })
    }
}

/// A fixed table (tests): a name not in it does not resolve.
#[cfg(test)]
pub struct Table(pub Vec<(&'static str, Vec<IpAddr>)>);

#[cfg(test)]
impl Resolve for Table {
    fn lookup(&self, host: &str, _port: u16) -> BoxFuture<'static, std::io::Result<Vec<IpAddr>>> {
        let found = self
            .0
            .iter()
            .find(|(h, _)| *h == host)
            .map(|(_, a)| a.clone());
        Box::pin(async move {
            found.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no such name"))
        })
    }
}
