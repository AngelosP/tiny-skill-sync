//! A token that lives only in memory, is wiped when dropped, and never prints itself.

use std::fmt;
use std::sync::atomic::{Ordering, compiler_fence};

pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // SAFETY: zero bytes are valid UTF-8, so the string stays valid while it is wiped.
        for b in unsafe { self.0.as_bytes_mut() } {
            // Volatile so the compiler cannot drop the writes as dead stores.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
        compiler_fence(Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_hides_the_value() {
        let s = Secret::new("ghp_abc");
        assert_eq!(format!("{s:?}"), "Secret(***)");
        assert_eq!(s.expose(), "ghp_abc");
    }
}
