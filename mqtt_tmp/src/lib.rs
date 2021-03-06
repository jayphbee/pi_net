//! mqtt 实现
//!
extern crate mqtt3;
extern crate net;
extern crate rand;
extern crate fnv;
extern crate magnetic;
extern crate gray;
extern crate time;
extern crate atom;
extern crate rustc_serialize;
extern crate compress;

pub mod client;
pub mod data;
pub mod server;
pub mod util;
pub mod handler;
pub mod session;

// pub use client::ClientNode;
// pub use data::{Client, Server};
pub use mqtt3::{LastWill, QoS};
// pub use server::{ServerNode, ClientStub};

use std::time::SystemTime;
fn now_millis() -> isize {
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Err(e) => -1,
        Ok(n) => n.as_millis() as isize,
    }
}
