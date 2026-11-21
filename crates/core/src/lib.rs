//! Colab Playground core: everything except the GUI.
//!
//! All process spawning goes through [`host::Host`]. Errors are
//! machine-readable ([`error::Error`]); no user-facing prose is produced here.

pub mod accounts;
pub mod backend;
pub mod cli;
pub mod clock;
pub mod defaults;
pub mod driver_lock;
pub mod error;
pub mod guard;
pub mod hf;
pub mod host;
pub mod hostfs;
pub mod http;
pub mod mock;
pub mod paths;
pub mod plugins;
pub mod provision;
pub mod quant;
pub mod real;
pub mod self_update;
pub mod sessions;
pub mod tunnel;
pub mod update;
pub mod usage;
pub mod vm;
pub mod watchdog;

pub use clock::{CancelToken, Clock, ManualClock, SystemClock};
pub use defaults::Defaults;
pub use error::{Error, ErrorKind, NextStep, Result};
pub use host::{Host, Toolchain};
