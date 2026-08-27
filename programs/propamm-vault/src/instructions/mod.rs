//! Program instructions. One instruction — one module: `Accounts`, arguments and
//! the handler sit side by side, because they change together too.

pub mod authority;
pub mod initialize_vault;
pub mod treasury;

pub use authority::*;
pub use initialize_vault::*;
pub use treasury::*;
