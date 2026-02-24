//! Lexical environments: linked frames of name→value bindings.

use super::value::{LispError, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// A shared, mutable environment frame.
pub type Env = Rc<RefCell<EnvFrame>>;

pub struct EnvFrame {
    bindings: HashMap<String, Value>,
    parent: Option<Env>,
}

impl EnvFrame {
    /// Create a new top-level (no parent) environment.
    pub fn new() -> Env {
        Rc::new(RefCell::new(EnvFrame {
            bindings: HashMap::new(),
            parent: None,
        }))
    }

    /// Create a child frame whose parent is `parent`.
    pub fn child(parent: &Env) -> Env {
        Rc::new(RefCell::new(EnvFrame {
            bindings: HashMap::new(),
            parent: Some(Rc::clone(parent)),
        }))
    }

    /// Look up `name`, walking up the parent chain.
    pub fn lookup(&self, name: &str) -> Result<Value, LispError> {
        if let Some(v) = self.bindings.get(name) {
            Ok(v.clone())
        } else if let Some(ref parent) = self.parent {
            parent.borrow().lookup(name)
        } else {
            Err(LispError::new(format!("unbound variable: {}", name)))
        }
    }

    /// Bind `name` in *this* frame (shadowing any outer binding).
    pub fn define(&mut self, name: &str, val: Value) {
        self.bindings.insert(name.to_string(), val);
    }

    /// Mutate an existing binding, searching up the parent chain.
    ///
    /// Returns an error if the name is not bound anywhere.
    pub fn set(&mut self, name: &str, val: Value) -> Result<(), LispError> {
        if self.bindings.contains_key(name) {
            self.bindings.insert(name.to_string(), val);
            Ok(())
        } else if let Some(ref parent) = self.parent {
            parent.borrow_mut().set(name, val)
        } else {
            Err(LispError::new(format!("set!: unbound variable: {}", name)))
        }
    }
}
