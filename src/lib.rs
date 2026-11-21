//! Control QUBO air purifiers and smart sockets, local or cloud.
//!
//! The library has the protocol, the MQTT client, the local broker and the cloud client.
//! The `qubo` binary adds the command line and the MCP server on top.
//!
//! # Examples
//!
//! Build the MQTT command that switches a purifier on:
//!
//! ```
//! use qubo_bridge::models::{Device, DeviceKind};
//! use qubo_bridge::protocol::power;
//!
//! let device = Device {
//!     kind: DeviceKind::Purifier,
//!     mac: "0A:0B:0C:0D:0E:0F".into(),
//!     device_uuid: "dev-1".into(),
//!     entity_uuid: "ent-1".into(),
//!     unit_uuid: "unit-1".into(),
//!     user_uuid: "user-1".into(),
//!     online: true,
//!     name: String::new(),
//! };
//! let command = power(&device, true);
//! assert_eq!(command.topic, "/control/unit-1/dev-1/lcSwitchControl");
//! ```
//!
//! # Modules
//!
//! - [`protocol`]: topics, command payloads and report parsing. No I/O.
//! - [`client`]: MQTT client that tracks the state of every device.
//! - [`broker`], [`certs`], [`serve`]: the local path.
//! - [`cloud`], [`connect`]: the cloud path and the choice between both paths.
//! - [`doctor`], [`service`]: diagnostics and start at boot.
//! - [`cli`], [`mcp_server`]: the command line and the MCP server.

pub mod broker;
pub mod certs;
pub mod cli;
pub mod client;
pub mod cloud;
pub mod connect;
pub mod doctor;
pub mod error;
pub mod mcp_server;
pub mod models;
pub mod protocol;
pub mod serve;
pub mod service;
