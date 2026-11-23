//! Project definition files (`snowflake.yml`): loading, validation, templates.

pub mod model;
pub(crate) mod validators;
pub mod value;
pub mod yaml;

pub use value::{Model, PyV};
pub mod conversion;
pub mod definition;
pub(crate) mod render;

pub use definition::Project;
