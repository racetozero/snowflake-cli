//! `SnowflakeSQLFacade` (`_plugins/nativeapp/sf_sql_facade.py`) and its error
//! translation (`sf_facade_exceptions.py`).

use snow_api::identifiers::{
    Fqn, identifier_to_show_like_pattern, same_identifiers, to_identifier, to_string_literal,
    unquote_identifier,
};
use snow_api::output::{Pv, Row};
use snow_api::{Cli, Result, SnowError};
use snow_connector::{DbError, ErrorKind};

pub type Rows = Vec<Vec<Option<String>>>;

pub const SPECIAL_COMMENT: &str = "GENERATED_BY_SNOWFLAKECLI";
pub const ALLOWED_SPECIAL_COMMENTS: [&str; 2] =
    ["GENERATED_BY_SNOWFLAKECLI", "GENERATED_BY_SNOWCLI"];
pub const DEFAULT_CHANNEL: &str = "DEFAULT";

pub mod errno {
    pub const NO_WAREHOUSE_SELECTED_IN_SESSION: i64 = 606;
    pub const SQL_COMPILATION_ERROR: i64 = 1003;
    pub const DOES_NOT_EXIST_OR_NOT_AUTHORIZED: i64 = 2003;
    pub const DOES_NOT_EXIST_OR_CANNOT_BE_PERFORMED: i64 = 2043;
    pub const INSUFFICIENT_PRIVILEGES: i64 = 3001;
    pub const APPLICATION_PACKAGE_MAX_VERSIONS_HIT: i64 = 93037;
    pub const VERSION_REFERENCED_BY_RELEASE_DIRECTIVE: i64 = 93088;
    pub const APPLICATION_PACKAGE_PATCH_ALREADY_EXISTS: i64 = 93168;
    pub const CANNOT_ADD_PATCH_WITH_NON_INCREASING_PATCH_NUMBER: i64 = 93167;
    pub const APPLICATION_REQUIRES_TELEMETRY_SHARING: i64 = 93321;
    pub const CANNOT_DISABLE_MANDATORY_TELEMETRY: i64 = 93329;
    pub const CANNOT_DISABLE_RELEASE_CHANNELS: i64 = 512001;
    pub const VERSION_DOES_NOT_EXIST: i64 = 93031;
    pub const CANNOT_CREATE_VERSION_WITH_NON_ZERO_PATCH: i64 = 93170;
    pub const MAX_UNBOUND_VERSIONS_REACHED: i64 = 512023;
    pub const CANNOT_DEREGISTER_VERSION_ASSOCIATED_WITH_CHANNEL: i64 = 512021;
    pub const CANNOT_SET_DEBUG_MODE_WITH_MANIFEST_VERSION: i64 = 93362;
    pub const APPLICATION_OWNS_EXTERNAL_OBJECTS: i64 = 93128;
    pub const NO_REFERENCE_SET_FOR_DEFINITION: i64 = 505019;
    pub const APPLICATION_NO_LONGER_AVAILABLE: i64 = 93079;
    pub const RELEASE_DIRECTIVE_DOES_NOT_EXIST: i64 = 93090;
    pub const TARGET_ACCOUNT_USED_BY_OTHER_RELEASE_DIRECTIVE: i64 = 93091;
    pub const ACCOUNT_DOES_NOT_EXIST: i64 = 1999;
    pub const ACCOUNT_HAS_TOO_MANY_QUALIFIERS: i64 = 906;

    /// `UPGRADE_RESTRICTION_CODES`.
    pub const UPGRADE_RESTRICTION: [i64; 5] = [93044, 93045, 93046, 93055, 93079];
    /// `CREATE_OR_UPGRADE_APPLICATION_EXPECTED_USER_ERROR_CODES`.
    pub const CREATE_OR_UPGRADE_USER_ERRORS: [i64; 24] = [
        93082, 93303, 93083, 93009, 93301, 397012, 2037, 3013, 93003, 397007, 93036, 93148, 397013,
        93302, 93118, 505019, 93300, 93011, 93054, 93084, 93197, 93012, 93003, 93148,
    ];
}

/// `handle_unclassified_error(err, context)`.
pub fn unclassified(cli: &Cli, e: &DbError, context: &str) -> SnowError {
    let m = format!("{context} {}", e.msg_with(cli.connector_info));
    match e.kind {
        ErrorKind::Programming => SnowError::unexpected(format!("Invalid SQL error occurred. {m}"))
            .named("InvalidSQLError"),
        ErrorKind::Database | ErrorKind::Operational => {
            SnowError::unexpected(format!("Unknown SQL error occurred. {m}"))
                .named("UnknownSQLError")
        }
        _ => SnowError::unexpected(format!("Unknown error occurred. {m}"))
            .named("UnknownConnectorError"),
    }
}

pub fn user_input(m: impl Into<String>) -> SnowError {
    SnowError::click(m.into()).named("UserInputError")
}

/// `InsufficientPrivilegesError(message, role=, database=, schema=)`.
pub fn insufficient(
    message: &str,
    role: Option<&str>,
    database: Option<&str>,
    schema: Option<&str>,
) -> SnowError {
    let mut m = message.to_string();
    if let Some(s) = schema.filter(|s| !s.is_empty()) {
        m.push_str(&format!(" in schema: {s}"));
    }
    if let Some(d) = database.filter(|s| !s.is_empty()) {
        m.push_str(&format!(" in database: {d}"));
    }
    if let Some(r) = role.filter(|s| !s.is_empty()) {
        m.push_str(&format!(" using role: {r}"));
    }
    SnowError::click(m).named("InsufficientPrivilegesError")
}

fn is_prog(e: &DbError, errno: i64) -> bool {
    e.kind == ErrorKind::Programming && e.errno == errno
}

/// `_use_object_optional(object_type, name)`: switch for the duration of `f`.
pub fn use_optional<T>(
    cli: &mut Cli,
    kind: &str,
    name: Option<&str>,
    f: impl FnOnce(&mut Cli) -> Result<T>,
) -> Result<T> {
    let Some(name) = name else { return f(cli) };
    let name = to_identifier(name);
    let row = match cli.try_query(&format!("select current_{kind}()"))? {
        Ok(c) => c,
        Err(e) => {
            return Err(unclassified(
                cli,
                &e,
                &format!("Failed to select current {kind}."),
            ));
        }
    };
    let prev = Cli::first_cell_opt(&row)
        .filter(|s| !s.is_empty())
        .map(|s| to_identifier(&s));
    if prev.as_deref().is_some_and(|p| same_identifiers(p, &name)) {
        return f(cli);
    }
    use_object(cli, kind, &name)?;
    let r = f(cli);
    if let Some(p) = prev {
        use_object(cli, kind, &p)?;
    }
    r
}

fn use_object(cli: &mut Cli, kind: &str, name: &str) -> Result<()> {
    match cli.try_query(&format!("use {kind} {name}"))? {
        Ok(_) => Ok(()),
        Err(e) if is_prog(&e, errno::DOES_NOT_EXIST_OR_CANNOT_BE_PERFORMED) => Err(user_input(format!(
            "Could not use {kind} {name}. Object does not exist, or operation cannot be performed."
        ))
        .named("CouldNotUseObjectError")),
        Err(e) => Err(unclassified(cli, &e, &format!("Failed to use {kind} {name}."))),
    }
}

pub fn use_role<T>(
    cli: &mut Cli,
    role: Option<&str>,
    f: impl FnOnce(&mut Cli) -> Result<T>,
) -> Result<T> {
    use_optional(cli, "role", role, f)
}

/// `_strip_empty_lines` + `dedent` of an indented multi-line statement.
pub fn strip_empty_lines(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let (last, rest) = lines.split_last().expect("non-empty");
    let mut out: Vec<&str> = rest
        .iter()
        .copied()
        .filter(|l| !l.trim().is_empty())
        .collect();
    out.push(last);
    out.join("\n")
}

fn statement(lines: &[&str]) -> String {
    // Builds the dedented text of an f-string whose lines are `lines` (already
    // without their common indent), ending in a newline like the triple-quoted source.
    let mut s = lines.join("\n");
    s.push('\n');
    strip_empty_lines(&s)
}

/// `execute_user_script(queries, script_name, role, warehouse, database)`.
pub fn execute_user_script(
    cli: &mut Cli,
    queries: &str,
    script_name: &str,
    role: Option<&str>,
    warehouse: Option<&str>,
    database: Option<&str>,
) -> Result<()> {
    use_optional(cli, "role", role, |cli| {
        use_optional(cli, "warehouse", warehouse, |cli| {
            use_optional(cli, "database", database, |cli| {
                let text = snow_rich::render::dedent(queries);
                for st in snow_api::splitter::split_statements(&text, true) {
                    match cli.try_query(&st.text)? {
                        Ok(_) => {}
                        Err(e) if e.kind == ErrorKind::Programming => {
                            let m = if e.errno == errno::NO_WAREHOUSE_SELECTED_IN_SESSION {
                                format!(
                                    "{}. Please provide a warehouse in your project definition file, config.toml file, or via command line",
                                    e.msg_with(cli.connector_info)
                                )
                            } else {
                                e.msg_with(cli.connector_info)
                            };
                            return Err(SnowError::click(format!(
                                "Failed to run script {script_name}. {m}"
                            ))
                            .named("UserScriptError"));
                        }
                        Err(e) => {
                            return Err(unclassified(
                                cli,
                                &e,
                                &format!("Failed to run script {script_name}."),
                            ));
                        }
                    }
                }
                Ok(())
            })
        })
    })
}

/// `create_version_in_package`.
pub fn create_version_in_package(
    cli: &mut Cli,
    package: &str,
    stage_path: &str,
    version: &str,
    label: Option<&str>,
    role: Option<&str>,
) -> Result<()> {
    let version = to_identifier(version);
    let package = to_identifier(package);
    let channels = super::release_channels(cli, &package, role)?;
    let label_clause = label
        .map(|l| format!("label={}", to_string_literal(l)))
        .unwrap_or_default();
    let using = format!(
        "using {}",
        crate::stage_execute::quote_stage_name(stage_path)
    );
    let action = if channels.is_empty() {
        "add"
    } else {
        "register"
    };
    let q = statement(&[
        &format!("alter application package {package}"),
        &format!("    {action} version {version}"),
        &format!("    {using}"),
        &format!("    {label_clause}"),
    ]);
    use_role(cli, role, |cli| match cli.try_query(&q)? {
        Ok(_) => Ok(()),
        Err(e) => Err(match e.errno {
            errno::MAX_UNBOUND_VERSIONS_REACHED if e.kind == ErrorKind::Programming => {
                user_input(format!(
                    "Maximum unbound versions reached for application package {package}. Please drop other unbound versions first, or add them to a release channel. Use `snow app version list` to view all versions."
                ))
            }
            errno::APPLICATION_PACKAGE_MAX_VERSIONS_HIT if e.kind == ErrorKind::Programming => {
                user_input(format!(
                    "Maximum versions reached for application package {package}. Please drop the other versions first."
                ))
            }
            errno::CANNOT_CREATE_VERSION_WITH_NON_ZERO_PATCH
                if e.kind == ErrorKind::Programming =>
            {
                user_input(
                    "Cannot create a new version with a non-zero patch in the manifest file.",
                )
            }
            _ => unclassified(
                cli,
                &e,
                &format!("Failed to {action} version {version} to application package {package}."),
            ),
        }),
    })
}

/// `drop_version_from_package`.
pub fn drop_version_from_package(
    cli: &mut Cli,
    package: &str,
    version: &str,
    role: Option<&str>,
) -> Result<()> {
    let version = to_identifier(version);
    let package = to_identifier(package);
    let channels = super::release_channels(cli, &package, role)?;
    let action = if channels.is_empty() {
        "drop"
    } else {
        "deregister"
    };
    let q = format!("alter application package {package} {action} version {version}");
    use_role(cli, role, |cli| match cli.try_query(&q)? {
        Ok(_) => Ok(()),
        Err(e) => Err(match e.errno {
            errno::VERSION_REFERENCED_BY_RELEASE_DIRECTIVE if e.kind == ErrorKind::Programming => {
                user_input(format!(
                    "Cannot drop version {version} from application package {package} because it is in use by one or more release directives."
                ))
            }
            errno::CANNOT_DEREGISTER_VERSION_ASSOCIATED_WITH_CHANNEL
                if e.kind == ErrorKind::Programming =>
            {
                user_input(format!(
                    "Cannot drop version {version} from application package {package} because it is associated with a release channel."
                ))
            }
            errno::VERSION_DOES_NOT_EXIST if e.kind == ErrorKind::Programming => user_input(
                format!("Version {version} does not exist in application package {package}."),
            ),
            _ => unclassified(
                cli,
                &e,
                &format!(
                    "Failed to {action} version {version} from application package {package}."
                ),
            ),
        }),
    })
}

/// `add_patch_to_package_version`: returns the new patch number.
pub fn add_patch_to_package_version(
    cli: &mut Cli,
    package: &str,
    stage_path: &str,
    version: &str,
    patch: Option<i64>,
    label: Option<&str>,
    role: Option<&str>,
) -> Result<i64> {
    let version = to_identifier(version);
    let label_clause = label
        .map(|l| format!("\nlabel={}", to_string_literal(l)))
        .unwrap_or_default();
    let patch_q = patch.map(|p| format!(" {p}")).unwrap_or_default();
    let using = crate::stage_execute::quote_stage_name(stage_path);
    // `dedent` of the source f-string: a `label=` line at column 0 defeats the dedent.
    let q = if label.is_some() {
        format!(
            "{:17}alter application package {package}\n{:21}add patch{patch_q} for version {version}\n{:21}using {using}{label_clause}\n",
            "", "", ""
        )
    } else {
        format!(
            "alter application package {package}\n    add patch{patch_q} for version {version}\n    using {using}\n"
        )
    };
    use_role(cli, role, |cli| {
        let c = match cli.try_query(&q)? {
            Ok(c) => c,
            Err(e) => {
                return Err(match e.errno {
                    errno::APPLICATION_PACKAGE_PATCH_ALREADY_EXISTS
                        if e.kind == ErrorKind::Programming =>
                    {
                        let extra = if patch.is_none() {
                            " Check the manifest file for any hard-coded patch value."
                        } else {
                            ""
                        };
                        user_input(format!(
                            "Patch{patch_q} already exists for version {version} in application package {package}.{extra}"
                        ))
                    }
                    errno::CANNOT_ADD_PATCH_WITH_NON_INCREASING_PATCH_NUMBER
                        if e.kind == ErrorKind::Programming =>
                    {
                        user_input(format!(
                            "Cannot add a patch with a non-increasing patch number to version {version} in application package {package}."
                        ))
                    }
                    _ => unclassified(
                        cli,
                        &e,
                        &format!(
                            "Failed to create patch{patch_q} for version {version} in application package {package}."
                        ),
                    ),
                });
            }
        };
        let rows = cli.rows(&c);
        let Some(row) = rows.first() else {
            return Err(SnowError::unexpected(
                "Received unexpected result from query. Expected to receive the new patch but the result is empty",
            )
            .named("UnexpectedResultError"));
        };
        Ok(match row.get("patch") {
            Some(Pv::Int(i)) => *i as i64,
            Some(other) => other.py_str().parse().unwrap_or(0),
            None => 0,
        })
    })
}

/// `get_event_definitions(app_name, role)`.
pub fn get_event_definitions(cli: &mut Cli, app: &str, role: Option<&str>) -> Result<Vec<Row>> {
    let ident = to_identifier(app);
    use_role(cli, role, |cli| {
        match cli.try_query(&format!(
            "show telemetry event definitions in application {ident}"
        ))? {
            Ok(c) => Ok(cli.rows(&c)),
            Err(e) => Err(unclassified(
                cli,
                &e,
                &format!("Failed to get event definitions for application {ident}."),
            )),
        }
    })
}

/// `get_app_properties(app_name, role)`: `desc application` as property → value.
pub fn get_app_properties(
    cli: &mut Cli,
    app: &str,
    role: Option<&str>,
) -> Result<indexmap::IndexMap<String, String>> {
    let ident = to_identifier(app);
    use_role(cli, role, |cli| {
        match cli.try_query(&format!("desc application {ident}"))? {
            Ok(c) => Ok(cli
                .rows(&c)
                .into_iter()
                .map(|r| {
                    (
                        r.get("property").map(Pv::py_str).unwrap_or_default(),
                        r.get("value").map(Pv::py_str).unwrap_or_default(),
                    )
                })
                .collect()),
            Err(e) => Err(unclassified(
                cli,
                &e,
                &format!("Failed to describe application {ident}."),
            )),
        }
    })
}

/// `share_telemetry_events(app_name, event_names, role)`.
pub fn share_telemetry_events(
    cli: &mut Cli,
    app: &str,
    events: &[String],
    role: Option<&str>,
) -> Result<()> {
    let ident = to_identifier(app);
    let names: Vec<String> = events.iter().map(|e| to_string_literal(e)).collect();
    let q = format!(
        "alter application {ident} set shared telemetry events ({})",
        names.join(", ")
    );
    use_role(cli, role, |cli| match cli.try_query(&q)? {
        Ok(_) => Ok(()),
        Err(e) => Err(unclassified(
            cli,
            &e,
            &format!("Failed to share telemetry events for application {ident}."),
        )),
    })
}

/// `create_schema(name, database)` (the caller's role).
pub fn create_schema(cli: &mut Cli, name: &str, database: Option<&str>) -> Result<()> {
    let fqn = Fqn::from_string(name)?;
    let ident = to_identifier(&fqn.name);
    let prefix = fqn.prefix();
    let database = if prefix.is_empty() {
        database.map(str::to_string)
    } else {
        Some(prefix)
    };
    use_optional(cli, "database", database.as_deref(), |cli| {
        match cli.try_query(&format!("create schema if not exists {ident}"))? {
            Ok(_) => Ok(()),
            Err(e) if is_prog(&e, errno::INSUFFICIENT_PRIVILEGES) => Err(insufficient(
                &format!("Insufficient privileges to create schema {name}"),
                None,
                database.as_deref(),
                None,
            )),
            Err(e) => Err(unclassified(
                cli,
                &e,
                &format!("Failed to create schema {name}."),
            )),
        }
    })
}

/// `stage_exists(name, database, schema)` (the caller's role).
pub fn stage_exists(cli: &mut Cli, name: &str) -> Result<bool> {
    let fqn = Fqn::from_string(name)?;
    let ident = to_identifier(&fqn.name);
    let pattern = identifier_to_show_like_pattern(&ident);
    let clause = match (&fqn.schema, &fqn.database) {
        (Some(s), Some(d)) => format!(" in schema {d}.{s}"),
        (Some(s), None) => format!(" in schema {s}"),
        (None, Some(d)) => format!(" in database {d}"),
        _ => String::new(),
    };
    match cli.try_query(&format!("show stages like {pattern}{clause}"))? {
        Ok(c) => Ok(!c.rows.is_empty()),
        Err(e) if is_prog(&e, errno::DOES_NOT_EXIST_OR_CANNOT_BE_PERFORMED) => Ok(false),
        Err(e) if is_prog(&e, errno::INSUFFICIENT_PRIVILEGES) => Err(insufficient(
            &format!("Insufficient privileges to check if stage {name} exists"),
            None,
            fqn.database.as_deref(),
            fqn.schema.as_deref(),
        )),
        Err(e) => Err(unclassified(
            cli,
            &e,
            &format!("Failed to check if stage {name} exists."),
        )),
    }
}

/// `create_stage(name, ...)` (not temporary, the caller's role) with the default
/// encryption and directory table.
pub fn create_stage(cli: &mut Cli, name: &str) -> Result<()> {
    let fqn = Fqn::from_string(name)?;
    let ident = to_identifier(&fqn.name);
    let q = format!(
        "create stage if not exists {ident} encryption = (type = 'SNOWFLAKE_SSE') directory = (enable = True)"
    );
    use_optional(cli, "database", fqn.database.as_deref(), |cli| {
        use_optional(cli, "schema", fqn.schema.as_deref(), |cli| {
            match cli.try_query(&q)? {
                Ok(_) => Ok(()),
                Err(e) if is_prog(&e, errno::INSUFFICIENT_PRIVILEGES) => Err(insufficient(
                    &format!("Insufficient privileges to create stage {name}"),
                    None,
                    fqn.database.as_deref(),
                    fqn.schema.as_deref(),
                )),
                Err(e) => Err(unclassified(
                    cli,
                    &e,
                    &format!("Failed to create stage {name}."),
                )),
            }
        })
    })
}

/// `show_release_directives(package, release_channel, role)`.
pub fn show_release_directives(
    cli: &mut Cli,
    package: &str,
    channel: Option<&str>,
    role: Option<&str>,
) -> Result<Vec<Row>> {
    let ident = to_identifier(package);
    let mut q = format!("show release directives in application package {ident}");
    if let Some(c) = channel.filter(|c| !c.is_empty()) {
        q.push_str(&format!(" for release channel {}", to_identifier(c)));
    }
    use_role(cli, role, |cli| match cli.try_query(&q)? {
        Ok(c) => Ok(cli.rows(&c)),
        Err(e) if is_prog(&e, errno::INSUFFICIENT_PRIVILEGES) => Err(insufficient(
            &format!(
                "Insufficient privileges to show release directives for application package {package}"
            ),
            role,
            None,
            None,
        )),
        Err(e) if is_prog(&e, errno::DOES_NOT_EXIST_OR_NOT_AUTHORIZED) => Err(user_input(format!(
            "Application package {package} does not exist or you are not authorized to access it."
        ))),
        Err(e) => Err(unclassified(
            cli,
            &e,
            &format!("Failed to show release directives for application package {package}."),
        )),
    })
}

/// `get_existing_app_info(name, role)`: the `show applications like` row named `name`.
pub fn get_existing_app_info(cli: &mut Cli, name: &str, role: Option<&str>) -> Result<Option<Row>> {
    use_role(cli, role, |cli| {
        let q = format!(
            "show applications like {}",
            identifier_to_show_like_pattern(name)
        );
        match cli.try_query(q.trim())? {
            Ok(c) => {
                let want = unquote_identifier(name);
                Ok(cli
                    .rows(&c)
                    .into_iter()
                    .find(|r| r.get("name").map(Pv::py_str).as_deref() == Some(&want)))
            }
            Err(e) => Err(unclassified(
                cli,
                &e,
                &format!("Unable to fetch information on application {name}."),
            )),
        }
    })
}

/// `create_application_package(package, distribution, enable_release_channels, role)`.
pub fn create_application_package(
    cli: &mut Cli,
    package: &str,
    distribution: &str,
    enable_release_channels: Option<bool>,
    role: Option<&str>,
) -> Result<()> {
    let package = to_identifier(package);
    let rc = enable_release_channels
        .map(|b| format!("enable_release_channels = {b}"))
        .unwrap_or_default();
    let q = statement(&[
        &format!("create application package {package}"),
        &format!("    comment = {SPECIAL_COMMENT}"),
        &format!("    distribution = {distribution}"),
        &format!("    {rc}"),
    ]);
    use_role(cli, role, |cli| match cli.try_query(&q)? {
        Ok(_) => Ok(()),
        Err(e) if is_prog(&e, errno::INSUFFICIENT_PRIVILEGES) => Err(insufficient(
            &format!("Insufficient privileges to create application package {package}"),
            role,
            None,
            None,
        )),
        Err(e) => Err(unclassified(
            cli,
            &e,
            &format!("Failed to create application package {package}."),
        )),
    })
}

/// `alter_application_package_properties(package, enable_release_channels, role)`.
pub fn alter_application_package_properties(
    cli: &mut Cli,
    package: &str,
    enable_release_channels: Option<bool>,
    role: Option<&str>,
) -> Result<()> {
    let package = to_identifier(package);
    let Some(b) = enable_release_channels else {
        return Ok(());
    };
    let q = format!("alter application package {package}\n    set enable_release_channels = {b}\n");
    use_role(cli, role, |cli| match cli.try_query(&q)? {
        Ok(_) => Ok(()),
        Err(e) if is_prog(&e, errno::INSUFFICIENT_PRIVILEGES) => Err(insufficient(
            &format!(
                "Insufficient privileges to update enable_release_channels for application package {package}"
            ),
            role,
            None,
            None,
        )),
        Err(e) if is_prog(&e, errno::CANNOT_DISABLE_RELEASE_CHANNELS) => Err(user_input(format!(
            "Cannot disable release channels for application package {package} after it is enabled. Try recreating the application package."
        ))),
        Err(e) => Err(unclassified(
            cli,
            &e,
            &format!("Failed to update enable_release_channels for application package {package}."),
        )),
    })
}

/// `show_versions(package, role)`.
pub fn show_versions(cli: &mut Cli, package: &str, role: Option<&str>) -> Result<Vec<Row>> {
    let package = to_identifier(package);
    use_role(cli, role, |cli| {
        match cli.try_query(&format!("show versions in application package {package}"))? {
            Ok(c) => Ok(cli.rows(&c)),
            Err(e) if is_prog(&e, errno::DOES_NOT_EXIST_OR_NOT_AUTHORIZED) => {
                Err(user_input(format!(
                    "Application package {package} does not exist or you are not authorized to access it."
                )))
            }
            Err(e) => Err(unclassified(
                cli,
                &e,
                &format!("Failed to show versions for application package {package}."),
            )),
        }
    })
}

/// `SameAccountInstallMethod`.
#[derive(Clone, Debug)]
pub enum InstallMethod {
    /// From the stage; only an application the CLI created can be upgraded.
    UnversionedDev,
    VersionedDev {
        version: String,
        patch: Option<i64>,
    },
    ReleaseDirective,
}

impl InstallMethod {
    pub fn is_dev_mode(&self) -> bool {
        !matches!(self, InstallMethod::ReleaseDirective)
    }
    pub fn using_clause(&self, stage_fqn: &str) -> String {
        match self {
            InstallMethod::ReleaseDirective => String::new(),
            InstallMethod::VersionedDev { version, patch } => {
                let patch = patch
                    .filter(|p| *p != 0)
                    .map(|p| format!("patch {p}"))
                    .unwrap_or_default();
                format!("using version {} {patch}", to_identifier(version))
            }
            InstallMethod::UnversionedDev => format!(
                "using {}",
                crate::stage_execute::quote_stage_name(stage_fqn)
            ),
        }
    }
    pub fn ensure_app_usable(&self, app: &str, row: Option<&Row>) -> Result<()> {
        if matches!(self, InstallMethod::UnversionedDev) {
            let comment = row
                .and_then(|r| r.get("comment"))
                .map(Pv::py_str)
                .unwrap_or_default();
            if !ALLOWED_SPECIAL_COMMENTS.contains(&comment.as_str()) {
                return Err(SnowError::click(format!(
                    "An application object \"{app}\" not created by Snowflake CLI already exists in the account."
                ))
                .named("ApplicationCreatedExternallyError"));
            }
        }
        Ok(())
    }
}

fn bool_py(b: bool) -> &'static str {
    if b { "True" } else { "False" }
}

/// `upgrade_application(...)`: the `alter application ... upgrade` rows.
#[allow(clippy::too_many_arguments)]
pub fn upgrade_application(
    cli: &mut Cli,
    name: &str,
    install: &InstallMethod,
    stage_path: &str,
    role: Option<&str>,
    warehouse: Option<&str>,
    debug_mode: Option<bool>,
    share_events: Option<bool>,
    release_channel: Option<&str>,
) -> Result<Rows> {
    let name = to_identifier(name);
    let channel = to_identifier(release_channel.unwrap_or(DEFAULT_CHANNEL));
    let row = get_existing_app_info(cli, &name, role)?;
    install.ensure_app_usable(&name, row.as_ref())?;
    let mut props: Option<indexmap::IndexMap<String, String>> = None;
    use_role(cli, role, |cli| {
        use_optional(cli, "warehouse", warehouse, |cli| {
            let using = install.using_clause(stage_path);
            let p = get_app_properties(cli, &name, role)?;
            let current = p
                .get("release_channel_name")
                .filter(|s| !s.is_empty() && *s != "None")
                .cloned()
                .unwrap_or_else(|| DEFAULT_CHANNEL.into());
            props = Some(p);
            if unquote_identifier(&channel) != current {
                return Err(user_input(format!(
                    "Application {name} is currently on release channel {current}. Cannot upgrade to release channel {channel}."
                ))
                .named("UpgradeApplicationRestrictionError"));
            }
            let upgrade_err = |cli: &Cli, e: DbError| -> SnowError {
                if e.kind == ErrorKind::Programming {
                    if errno::UPGRADE_RESTRICTION.contains(&e.errno) {
                        return user_input(e.msg_with(cli.connector_info))
                            .named("UpgradeApplicationRestrictionError");
                    }
                    if errno::CREATE_OR_UPGRADE_USER_ERRORS.contains(&e.errno) {
                        return user_input(format!(
                            "Failed to upgrade application {name} with the following error message:\n{}",
                            e.msg_with(cli.connector_info)
                        ));
                    }
                }
                unclassified(cli, &e, &format!("Failed to upgrade application {name}."))
            };
            let cursor =
                match cli.try_query(&format!("alter application {name} upgrade {using}"))? {
                    Ok(c) => c,
                    Err(e) => return Err(upgrade_err(cli, e)),
                };
            if install.is_dev_mode()
                && let Some(d) = debug_mode
                && let Err(e) = cli.try_query(&format!(
                    "alter application {name} set debug_mode = {}",
                    bool_py(d)
                ))?
            {
                return Err(upgrade_err(cli, e));
            }
            if let Some(share) = share_events {
                let current = props
                    .as_ref()
                    .and_then(|p| p.get("authorize_telemetry_event_sharing").cloned())
                    .unwrap_or_else(|| "false".into())
                    .to_lowercase()
                    == "true";
                if current != share {
                    crate::logs::info(
                        "snowflake.cli._plugins.nativeapp.sf_sql_facade",
                        &format!(
                            "Setting telemetry sharing authorization to {}",
                            bool_py(share)
                        ),
                    );
                    let q = format!(
                        "alter application {name} set AUTHORIZE_TELEMETRY_EVENT_SHARING = {}",
                        bool_py(share).to_uppercase()
                    );
                    if let Err(e) = cli.try_query(&q)? {
                        if is_prog(&e, errno::CANNOT_DISABLE_MANDATORY_TELEMETRY) {
                            return Err(user_input(
                                "Could not disable telemetry event sharing for the application because it contains mandatory events. Please set 'share_mandatory_events' to true in the application telemetry section of the project definition file.",
                            ));
                        }
                        return Err(unclassified(
                            cli,
                            &e,
                            &format!(
                                "Failed to set AUTHORIZE_TELEMETRY_EVENT_SHARING when upgrading application {name}."
                            ),
                        ));
                    }
                }
            }
            Ok(cursor.rows)
        })
    })
}

/// `create_application(...)`: the create rows and warnings.
#[allow(clippy::too_many_arguments)]
pub fn create_application(
    cli: &mut Cli,
    name: &str,
    package: &str,
    install: &InstallMethod,
    stage_path: &str,
    role: Option<&str>,
    warehouse: Option<&str>,
    debug_mode: Option<bool>,
    share_events: Option<bool>,
    release_channel: Option<&str>,
) -> Result<(Rows, Vec<String>)> {
    let package = to_identifier(package);
    let name = to_identifier(name);
    let channel = release_channel.filter(|c| !c.is_empty()).map(to_identifier);
    let initial_debug = install.is_dev_mode() && debug_mode.unwrap_or(true);
    let telemetry = match share_events {
        Some(s) => {
            crate::logs::info(
                "snowflake.cli._plugins.nativeapp.sf_sql_facade",
                &format!(
                    "Setting AUTHORIZE_TELEMETRY_EVENT_SHARING to {}",
                    bool_py(s)
                ),
            );
            format!(
                "AUTHORIZE_TELEMETRY_EVENT_SHARING = {}",
                bool_py(s).to_uppercase()
            )
        }
        None => String::new(),
    };
    let using = install.using_clause(stage_path);
    let channel_clause = channel
        .map(|c| format!("using release channel {c}"))
        .unwrap_or_default();
    let q = statement(&[
        &format!("create application {name}"),
        &format!("    from application package {package}"),
        &format!("    {using}"),
        &format!("    {channel_clause}"),
        &format!("    {telemetry}"),
        &format!("    comment = {SPECIAL_COMMENT}"),
    ]);
    use_role(cli, role, |cli| {
        use_optional(cli, "warehouse", warehouse, |cli| {
            let cursor = match cli.try_query(&q)? {
                Ok(c) => c,
                Err(e) => {
                    if is_prog(&e, errno::APPLICATION_REQUIRES_TELEMETRY_SHARING) {
                        return Err(user_input(
                            "The application package requires event sharing to be authorized. Please set 'share_mandatory_events' to true in the application telemetry section of the project definition file.",
                        ));
                    }
                    if e.kind == ErrorKind::Programming
                        && errno::CREATE_OR_UPGRADE_USER_ERRORS.contains(&e.errno)
                    {
                        return Err(user_input(format!(
                            "Failed to create application {name} with the following error message:\n{}",
                            e.msg_with(cli.connector_info)
                        )));
                    }
                    return Err(unclassified(
                        cli,
                        &e,
                        &format!("Failed to create application {name}."),
                    ));
                }
            };
            let mut warnings = Vec::new();
            if initial_debug {
                let q = statement(&[
                    &format!("alter application {name}"),
                    "set debug_mode = True",
                ]);
                if let Err(e) = cli.try_query(&q)? {
                    if is_prog(&e, errno::CANNOT_SET_DEBUG_MODE_WITH_MANIFEST_VERSION) {
                        warnings.push(
                            "Did not apply debug mode to application because the manifest version is set to 2 or higher. Please use session debugging instead.".to_string(),
                        );
                    } else {
                        warnings.push(format!(
                            "Failed to set debug mode for application {name}. {}",
                            e.msg_with(cli.connector_info)
                        ));
                    }
                }
            }
            Ok((cursor.rows, warnings))
        })
    })
}
