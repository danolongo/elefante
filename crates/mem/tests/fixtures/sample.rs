//! Provider-neutral agent loop. It owns history across turns.
//! A second sentence that must not appear.

use std::{collections::HashMap, sync::Arc};
use crate::sse::{SseDecoder, frame::Frame as F};
use super::util::*;
use self::inner::helper;

mod sse;
pub mod openrouter;

/// The stable standing instructions. Safety lives in code.
pub const SYSTEM_PROMPT: &str = "You are Elefante. sk-ant-api03-ABCDEFGH12345678";

pub static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A model identifier.
pub type ModelId = String;

/// One loop invocation.
#[derive(Clone, Debug)]
pub struct Session {
    messages: Vec<Message>,
}

pub struct Unit;
pub struct Pair(pub u32, pub u32);

/// Why a turn ended.
pub enum StopReason {
    EndTurn,
    ToolUse,
}

impl Session {
    /// Appends one user turn and retains its history.
    ///
    /// Longer discussion that is not the first sentence.
    #[allow(clippy::too_many_arguments)]
    pub async fn prompt<F>(&mut self, provider: &dyn Provider, text: impl Into<String>) -> Result<(), E>
    where
        F: FnMut(),
    {
        let local = 1;
        fn nested_helper() {}
    }

    fn private(&self) {}
}

impl<T: Clone> Provider for Wrapper<T> {
    fn stream(&self) {}
}

/// Decides whether a call may run.
pub trait Gate {
    /// Required method.
    fn check(&self) -> bool;
    fn default_method(&self) {}
}

pub mod inner {
    pub fn helper() {}
    pub struct Deep;
    impl Deep {
        pub fn new() -> Self { Deep }
    }
}

macro_rules! bump {
    () => {};
}

fn free_function(a: u32) -> u32 { a }

/// Separated from the item by a blank line, so it is not a doc.

fn after_blank() {}
