//! gentle-partitioner: Rust host that partitions the terminal and runs
//! gentle-shell, unchanged, inside its own PTY pane.
pub mod app;
pub mod apps_client;
pub mod apps_panel;
pub mod arch_client;
pub mod editor;
pub mod explain_client;
pub mod gitgraph;
pub mod highlight;
pub mod icons;
pub mod keys;
pub mod layout;
pub mod pane;
pub mod predict;
pub mod theme;
pub mod predict_server;
pub mod sanitize;
pub mod tree;
pub mod viewer;
