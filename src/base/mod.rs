//! The base layer, the bottom of the order: config and its keys, paths, identity, provider and
//! model names, pricing, the shared HTTP client and TLS, build facts, error logging.

pub mod build_info;
pub mod config;
pub mod config_keys;
pub mod config_set;
pub mod config_show;
pub mod http_client;
pub mod identity;
pub mod log_err;
pub mod model;
pub mod paths;
pub mod pricing;
pub mod provider;
pub mod tls;
