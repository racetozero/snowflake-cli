//! Error types mirroring Snowflake CLI's exception hierarchy.

use snow_click::ClickError;

/// Exit codes of the `BaseCliError` family when `--enhanced-exit-codes` is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// `CliError` / plain `ClickException`.
    Generic,
    /// `CliArgumentError` (2).
    Argument,
    /// `CliConnectionError` (3).
    Connection,
    /// `CliSqlError` (5).
    Sql,
    /// A `ClickException` that is not a `BaseCliError` (always 1).
    Click,
    /// A `BaseCliError` whose `exit_code` was reset to 0 (rendered as usual).
    ExitZero,
    /// A plain `ClickException` whose `exit_code` was reset to 0.
    ExitZeroClick,
}

#[derive(Debug, Clone)]
pub enum SnowError {
    /// Click usage errors and `Exit`; rendered by `snow_click::help::render_error`.
    Click(ClickError),
    /// A `ClickException` subclass with a message shown in the Error panel.
    Cli {
        message: String,
        class: ErrorClass,
        /// Lines written after the panel (ConfigFileTooWidePermissionsError).
        after: Vec<String>,
        /// The Python exception class name (recorded in last_cli_error.json).
        name: &'static str,
    },
    /// Any other exception: message and Python class name.
    Unexpected(String, &'static str),
}

pub type Result<T, E = SnowError> = std::result::Result<T, E>;

impl SnowError {
    fn with(message: impl Into<String>, class: ErrorClass, name: &'static str) -> SnowError {
        SnowError::Cli {
            message: message.into(),
            class,
            after: Vec::new(),
            name,
        }
    }
    pub fn cli(message: impl Into<String>) -> SnowError {
        SnowError::with(message, ErrorClass::Generic, "CliError")
    }
    /// `click.ClickException(message)`.
    pub fn click(message: impl Into<String>) -> SnowError {
        SnowError::with(message, ErrorClass::Click, "ClickException")
    }
    pub fn argument(message: impl Into<String>) -> SnowError {
        SnowError::with(message, ErrorClass::Argument, "CliArgumentError")
    }
    pub fn connection(message: impl Into<String>) -> SnowError {
        SnowError::with(message, ErrorClass::Connection, "CliConnectionError")
    }
    pub fn sql(message: impl Into<String>) -> SnowError {
        SnowError::with(message, ErrorClass::Sql, "CliSqlError")
    }
    /// `MissingConfigurationError` for a directory without `snowflake.yml`.
    pub fn missing_project_definition() -> SnowError {
        SnowError::cli(
            "Cannot find project definition (snowflake.yml). Please provide a path to the project or run this command in a valid project directory.",
        )
        .named("MissingConfigurationError")
    }
    /// `click.UsageError(message)` raised by command code (exit 2, usage line).
    pub fn usage(message: impl Into<String>) -> SnowError {
        SnowError::Click(ClickError::usage(message.into()))
    }
    /// `IncompatibleParametersError(options)`.
    pub fn incompatible(options: &[&str]) -> SnowError {
        SnowError::Click(ClickError::usage_class(
            parameters_message(
                options,
                "are incompatible and cannot be used simultaneously.",
            ),
            "IncompatibleParametersError",
        ))
    }
    /// `UnmetParametersError(options)`.
    pub fn unmet(options: &[&str]) -> SnowError {
        SnowError::Click(ClickError::usage_class(
            parameters_message(options, "must be used simultaneously."),
            "UnmetParametersError",
        ))
    }
    /// Set the Python exception class name.
    pub fn named(self, n: &'static str) -> SnowError {
        match self {
            SnowError::Cli {
                message,
                class,
                after,
                ..
            } => SnowError::Cli {
                message,
                class,
                after,
                name: n,
            },
            SnowError::Unexpected(m, _) => SnowError::Unexpected(m, n),
            other => other,
        }
    }
    pub fn class_name(&self) -> &'static str {
        match self {
            SnowError::Cli { name, .. } => name,
            SnowError::Click(snow_click::ClickError::Usage { message, .. })
                if message.starts_with("Invalid value") =>
            {
                "BadParameter"
            }
            SnowError::Click(snow_click::ClickError::Usage { message, .. })
                if message.ends_with("are incompatible and cannot be used simultaneously.") =>
            {
                "IncompatibleParametersError"
            }
            SnowError::Click(snow_click::ClickError::Usage { message, .. })
                if message.ends_with("must be used simultaneously.") =>
            {
                "UnmetParametersError"
            }
            SnowError::Click(snow_click::ClickError::Usage { message, .. })
                if message.starts_with("Missing ") =>
            {
                "MissingParameter"
            }
            SnowError::Click(c) => c.class_name(),
            SnowError::Unexpected(_, name) => name,
        }
    }
    /// An unexpected exception of the given Python class.
    pub fn raised(name: &'static str, message: impl Into<String>) -> SnowError {
        SnowError::Unexpected(message.into(), name)
    }
    pub fn unexpected(message: impl Into<String>) -> SnowError {
        SnowError::Unexpected(message.into(), "Exception")
    }
    pub fn message(&self) -> String {
        match self {
            SnowError::Click(c) => c.message(),
            SnowError::Cli { message, .. } => message.clone(),
            SnowError::Unexpected(m, _) => m.clone(),
        }
    }
    pub fn exit_code(&self, enhanced: bool) -> i32 {
        match self {
            SnowError::Click(c) => c.exit_code(),
            SnowError::Cli { class, .. } => match class {
                ErrorClass::ExitZero | ErrorClass::ExitZeroClick => 0,
                _ if !enhanced => 1,
                ErrorClass::Generic | ErrorClass::Click => 1,
                ErrorClass::Argument => 2,
                ErrorClass::Connection => 3,
                ErrorClass::Sql => 5,
            },
            SnowError::Unexpected(..) => 1,
        }
    }
}

/// `Parameters 'a', 'b' and 'c' <tail>`.
fn parameters_message(options: &[&str], tail: &str) -> String {
    let quoted: Vec<String> = options.iter().map(|o| format!("'{o}'")).collect();
    let (last, rest) = quoted.split_last().expect("at least one option");
    format!("Parameters {} and {last} {tail}", rest.join(", "))
}

impl From<ClickError> for SnowError {
    fn from(e: ClickError) -> Self {
        match e {
            ClickError::Exception { message, .. } => SnowError::click(message),
            other => SnowError::Click(other),
        }
    }
}

impl From<std::io::Error> for SnowError {
    fn from(e: std::io::Error) -> Self {
        SnowError::Unexpected(python_os_error(&e, None), "Exception")
    }
}

/// Python's `OSError.strerror`.
pub fn strerror(e: &std::io::Error) -> String {
    let full = python_os_error(e, None);
    full.split_once("] ")
        .map(|(_, t)| t.to_string())
        .unwrap_or(full)
}

/// Python's `str(OSError)` (`[Errno 2] No such file or directory: 'x'`).
pub fn python_os_error(e: &std::io::Error, path: Option<&str>) -> String {
    let (errno, text) = match e.kind() {
        std::io::ErrorKind::NotFound => (2, "No such file or directory"),
        std::io::ErrorKind::PermissionDenied => (13, "Permission denied"),
        std::io::ErrorKind::AlreadyExists => (17, "File exists"),
        std::io::ErrorKind::IsADirectory => (21, "Is a directory"),
        std::io::ErrorKind::NotADirectory => (20, "Not a directory"),
        std::io::ErrorKind::DirectoryNotEmpty => (66, "Directory not empty"),
        _ => (e.raw_os_error().unwrap_or(0), ""),
    };
    let text = if text.is_empty() {
        e.to_string()
    } else {
        text.to_string()
    };
    match path {
        Some(p) => format!("[Errno {errno}] {text}: {}", snow_click::py_repr(p)),
        None => format!("[Errno {errno}] {text}"),
    }
}
