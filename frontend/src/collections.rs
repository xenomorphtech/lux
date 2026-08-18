//! Hash collections used across the frontend. Always hashbrown, on std and
//! no_std alike, so both builds share one behavior. Nothing hash-ordered ever
//! reaches a content hash (the hashing pass sorts everything), but one code
//! path is still one fewer way to diverge.
pub use hashbrown::{HashMap, HashSet};
