pub mod client;
pub mod connector;
pub mod dns;

pub use client::{HttpClients, build_clients};
pub use dns::{HickoryDnsResolver, build_single_resolver, parse_resolver_addr};

/// hyper's 16 KiB h2 default resets the stream on real pages (a `link`
/// preload header alone can run 27 KiB); Chrome accepts 256 KiB.
pub const H2_MAX_HEADER_LIST_SIZE: u32 = 256 << 10;
/// Over h1 the same hints arrive one `link` line each; hyper's default is 100.
pub const H1_MAX_HEADERS: usize = 1024;
