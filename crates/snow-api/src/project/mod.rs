//! Project definition files (`snowflake.yml`): loading, validation, templates.

pub mod model;
pub mod validators;
pub mod value;
pub mod yaml;

pub use value::{Model, PyV};
pub mod conversion;
pub mod definition;
pub mod render;

pub use definition::Project;
