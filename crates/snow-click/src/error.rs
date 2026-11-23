//! Click exceptions and Typer's Rich error panel.

use crate::spec::{Param, Ty};

#[derive(Debug, Clone)]
pub enum ClickError {
    /// `click.UsageError` and subclasses (exit 2). `depth` is the level
    /// whose usage line is printed.
    Usage {
        message: String,
        depth: Option<usize>,
        no_such_option: bool,
        /// False for errors Click raises without a context (`BadOptionUsage`
        /// from the parser): no usage line is printed.
        attach: bool,
        /// The Python exception class (`UsageError`, `BadParameter`, ...).
        class: &'static str,
    },
    /// `click.ClickException` (exit code given, usually 1): an Error panel
    /// without a usage line.
    Exception { message: String, code: i32 },
    /// `click.exceptions.Exit` / `ctx.exit(code)`.
    Exit(i32),
    /// `click.Abort` (Ctrl-C at a prompt).
    Abort,
}

impl ClickError {
    pub fn usage(message: String) -> ClickError {
        ClickError::Usage {
            message,
            depth: None,
            no_such_option: false,
            attach: true,
            class: "UsageError",
        }
    }
    /// A usage error with a specific Python class name.
    pub fn usage_class(message: String, class: &'static str) -> ClickError {
        match ClickError::usage(message) {
            ClickError::Usage {
                message,
                depth,
                no_such_option,
                attach,
                ..
            } => ClickError::Usage {
                message,
                depth,
                no_such_option,
                attach,
                class,
            },
            other => other,
        }
    }
    /// A usage error raised without a Click context.
    pub fn bad_option_usage(message: String) -> ClickError {
        ClickError::Usage {
            message,
            depth: None,
            no_such_option: false,
            attach: false,
            class: "BadOptionUsage",
        }
    }
    pub fn exception(message: impl Into<String>) -> ClickError {
        ClickError::Exception {
            message: message.into(),
            code: 1,
        }
    }
    pub fn is_no_such_option(&self) -> bool {
        matches!(
            self,
            ClickError::Usage {
                no_such_option: true,
                ..
            }
        )
    }
    pub fn at(self, d: usize) -> ClickError {
        match self {
            ClickError::Usage {
                message,
                depth: None,
                no_such_option,
                attach: true,
                class,
            } => ClickError::Usage {
                message,
                depth: Some(d),
                no_such_option,
                attach: true,
                class,
            },
            other => other,
        }
    }
    pub fn no_such_option(opt: &str, mut possibilities: Vec<String>) -> ClickError {
        let mut message = format!("No such option: {opt}");
        if possibilities.len() == 1 {
            message.push_str(&format!(" Did you mean {}?", possibilities[0]));
        } else if !possibilities.is_empty() {
            possibilities.sort();
            message.push_str(&format!(
                " (Possible options: {})",
                possibilities.join(", ")
            ));
        }
        ClickError::Usage {
            message,
            depth: None,
            no_such_option: true,
            attach: true,
            class: "NoSuchOption",
        }
    }
    pub fn bad_parameter(param: &Param, message: String) -> ClickError {
        ClickError::usage_class(
            format!("Invalid value for {}: {}", error_hint(param), message),
            "BadParameter",
        )
    }
    pub fn missing(param: &Param) -> ClickError {
        let kind = if param.is_argument {
            "argument"
        } else {
            "option"
        };
        let mut msg = String::new();
        if let Ty::Choice { choices, .. } = &param.ty {
            msg = format!(" Choose from:\n\t{}", choices.join(",\n\t"));
        }
        ClickError::usage_class(
            format!("Missing {kind} {}.{msg}", error_hint(param)),
            "MissingParameter",
        )
    }
    pub fn class_name(&self) -> &'static str {
        match self {
            ClickError::Usage { class, .. } => class,
            ClickError::Exception { .. } => "ClickException",
            ClickError::Exit(_) => "Exit",
            ClickError::Abort => "Abort",
        }
    }
    pub fn exit_code(&self) -> i32 {
        match self {
            ClickError::Usage { .. } => 2,
            ClickError::Exception { code, .. } => *code,
            ClickError::Exit(c) => *c,
            ClickError::Abort => 1,
        }
    }
    pub fn message(&self) -> String {
        match self {
            ClickError::Usage { message, .. } | ClickError::Exception { message, .. } => {
                message.clone()
            }
            ClickError::Exit(_) => String::new(),
            ClickError::Abort => "Aborted.".into(),
        }
    }
}

/// `Parameter.get_error_hint`.
pub fn error_hint(param: &Param) -> String {
    if param.is_argument {
        format!("'{}'", param.usage_metavar)
    } else {
        param
            .opts
            .iter()
            .map(|o| format!("'{o}'"))
            .collect::<Vec<_>>()
            .join(" / ")
    }
}
