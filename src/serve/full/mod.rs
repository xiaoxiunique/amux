//! Extra endpoints for the full agent-port host build (`amux serve --features
//! full`): macOS control-center (apps + screenshots), token-usage stats, and
//! APNs push. The whole module is gated by the `full` feature via the
//! `#[cfg(feature = "full")] pub(crate) mod full;` declaration in the parent.
pub(crate) mod control_center;
pub(crate) mod push;
pub(crate) mod usage;
