//! Command dispatch: command path -> handler.

use snow_api::{Cli, Output, Result, SnowError};

pub fn dispatch(path: &str, cli: &mut Cli) -> Result<Output> {
    use crate::*;
    match path {
        "snow connection list" => connection::list(cli),
        "snow connection add" => connection::add(cli),
        "snow connection remove" => connection::remove(cli),
        "snow connection set-default" => connection::set_default(cli),
        "snow connection test" => connection::test(cli),
        "snow connection generate-jwt" => connection::generate_jwt(cli),
        "snow connection generate-workload-identity-token" => {
            connection::generate_workload_identity_token(cli)
        }
        "snow object list" => object::list(cli),
        "snow object drop" => object::drop(cli),
        "snow object describe" => object::describe(cli),
        "snow object create" => object::create(cli),
        "snow dbt list" => dbt::list(cli),
        "snow dbt describe" => dbt::describe(cli),
        "snow dbt drop" => dbt::drop(cli),
        p if p.starts_with("snow dbt execute ") => dbt::execute(cli),
        "snow stage list" => object::list_for(cli, "stage"),
        "snow stage describe" => object::describe_for(cli, "stage", "name"),
        "snow stage drop" => object::drop_for(cli, "stage", "name"),
        "snow git list" => object::list_for(cli, "git-repository"),
        "snow git describe" => object::describe_for(cli, "git-repository", "name"),
        "snow git drop" => object::drop_for(cli, "git-repository", "name"),
        "snow streamlit list" => object::list_for(cli, "streamlit"),
        "snow streamlit describe" => object::describe_for(cli, "streamlit", "name"),
        "snow streamlit drop" => object::drop_for(cli, "streamlit", "name"),
        "snow spcs compute-pool list" => object::list_for(cli, "compute-pool"),
        "snow spcs compute-pool describe" => object::describe_for(cli, "compute-pool", "name"),
        "snow spcs compute-pool drop" => object::drop_for(cli, "compute-pool", "name"),
        "snow spcs image-repository list" => object::list_for(cli, "image-repository"),
        "snow spcs image-repository drop" => object::drop_for(cli, "image-repository", "name"),
        "snow spcs service list" => object::list_for(cli, "service"),
        "snow spcs service describe" => object::describe_for(cli, "service", "name"),
        "snow dcm list" => object::list_for(cli, "dcm"),
        "snow spcs compute-pool create" => spcs::compute_pool::create(cli),
        "snow spcs compute-pool stop-all" => spcs::compute_pool::stop_all(cli),
        "snow spcs compute-pool suspend" => spcs::compute_pool::suspend(cli),
        "snow spcs compute-pool resume" => spcs::compute_pool::resume(cli),
        "snow spcs compute-pool set" => spcs::compute_pool::set(cli),
        "snow spcs compute-pool unset" => spcs::compute_pool::unset(cli),
        "snow spcs compute-pool status" => spcs::compute_pool::status(cli),
        "snow spcs image-registry token" => spcs::image_registry::token_cmd(cli),
        "snow spcs image-registry url" => spcs::image_registry::url(cli),
        "snow spcs image-registry login" => spcs::image_registry::login(cli),
        "snow spcs image-repository create" => spcs::image_repository::create(cli),
        "snow spcs image-repository list-images" => spcs::image_repository::list_images(cli),
        "snow spcs image-repository list-tags" => spcs::image_repository::list_tags(cli),
        "snow spcs image-repository url" => spcs::image_repository::url(cli),
        "snow spcs service create" => spcs::services::create(cli),
        "snow spcs service drop" => spcs::services::drop(cli),
        "snow spcs service execute-job" => spcs::services::execute_job(cli),
        "snow spcs service status" => spcs::services::status(cli),
        "snow spcs service list-endpoints" => spcs::services::list_endpoints(cli),
        "snow spcs service list-instances" => spcs::services::list_instances(cli),
        "snow spcs service list-containers" => spcs::services::list_containers(cli),
        "snow spcs service list-roles" => spcs::services::list_roles(cli),
        "snow spcs service suspend" => spcs::services::suspend(cli),
        "snow spcs service resume" => spcs::services::resume(cli),
        "snow spcs service upgrade" => spcs::services::upgrade(cli),
        "snow spcs service set" => spcs::services::set(cli),
        "snow spcs service unset" => spcs::services::unset(cli),
        "snow stage list-files" => stage::list_files(cli),
        "snow stage create" => stage::create(cli),
        "snow stage remove" => stage::remove(cli),
        "snow stage copy" => stage::copy(cli),
        "snow git list-branches" => git::list_branches(cli),
        "snow git list-tags" => git::list_tags(cli),
        "snow git fetch" => git::fetch(cli),
        "snow git list-files" => git::list_files(cli),
        "snow notebook execute" => notebook::execute(cli),
        "snow notebook get-url" => notebook::get_url(cli),
        "snow notebook open" => notebook::open(cli),
        "snow notebook create" => notebook::create(cli),
        "snow sql" => sql::execute(cli),
        "snow cortex complete" => cortex::complete(cli),
        "snow cortex extract-answer" => cortex::extract_answer(cli),
        "snow cortex sentiment" => cortex::sentiment(cli),
        "snow cortex summarize" => cortex::summarize(cli),
        "snow cortex translate" => cortex::translate(cli),
        "snow snowpark list" => {
            let t = cli.str("object_type").unwrap_or_default();
            object::list_for(cli, &t)
        }
        "snow snowpark drop" => {
            let t = cli.str("object_type").unwrap_or_default();
            object::drop_for(cli, &t, "identifier")
        }
        "snow snowpark describe" => {
            let t = cli.str("object_type").unwrap_or_default();
            object::describe_for(cli, &t, "identifier")
        }
        "snow snowpark execute" => {
            let t = cli.str("object_type").unwrap_or_default();
            let id = cli.str("execution_identifier").unwrap_or_default();
            let q = if t == "function" {
                format!("select {id}")
            } else {
                format!("call {id}")
            };
            let c = cli.execute_query(&q)?;
            Ok(cli.single_query_result(&c))
        }
        "snow helpers check-snowsql-env-vars" => helpers::check_snowsql_env_vars(cli),
        "snow helpers check-version" => helpers::check_version(cli),
        "snow helpers generate-project-schema" => helpers::generate_project_schema(cli),
        "snow logs" => logs_cmd::logs(cli),
        "snow helpers detect-encoding" => helpers::detect_encoding(cli),
        "snow helpers show-config-sources" => helpers::show_config_sources(cli),
        "snow helpers clean-installer-path" => helpers::clean_installer_path(cli),
        "snow helpers import-snowsql-connections" => {
            snowsql_import::import_snowsql_connections(cli)
        }
        "snow plugin list" => helpers::plugin_list(cli),
        "snow plugin enable" | "snow plugin disable" => helpers::plugin_toggle(cli),
        "snow feedback" => feedback::feedback(cli),
        "snow auth list" => auth_cmd::list(cli),
        "snow auth oidc read-token" => auth_cmd::oidc_read_token(cli),
        // Upstream gates search on Python < 3.12; the reference (and the spec) is 3.12.
        "snow cortex search" => Err(SnowError::click(
            "Cortex Search uses Snowflake Python API that currently does not support your Python version",
        )),
        "snow ws dump" => ws::dump(cli),
        "snow bundle create" => bundle::create(cli),
        "snow bundle list" => bundle::list(cli),
        "snow bundle delete" => bundle::delete(cli),
        "snow bundle alter" => bundle::alter(cli),
        "snow bundle execute" => bundle::execute(cli),
        "snow bundle status" => bundle::status(cli),
        "snow bundle cancel" => bundle::cancel(cli),
        "snow bundle history" => bundle::history(cli),
        "snow streamlit execute" => streamlit::execute(cli),
        "snow streamlit get-url" => streamlit::get_url(cli),
        "snow streamlit status" => streamlit::status(cli),
        "snow streamlit share" => streamlit::share(cli),
        "snow stage execute" => {
            let p = cli.str("stage_path").unwrap_or_default();
            stage_execute::execute(cli, &p, false)
        }
        "snow git execute" => {
            let p = cli.str("repository_path").unwrap_or_default();
            stage_execute::execute(cli, &p, true)
        }
        "snow git copy" => git::copy(cli),
        "snow git setup" => git::setup(cli),
        "snow stage diff" => stage_diff::stage_diff(cli),
        "snow spcs service logs" => spcs::services::logs(cli),
        "snow spcs service events" => spcs::services::events(cli),
        "snow spcs service metrics" => spcs::services::metrics(cli),
        "snow spcs service remote-build-status" => spcs::services::remote_build_status(cli),
        "snow spcs service remote-build-history" => spcs::services::remote_build_history(cli),
        "snow dbt deploy" => dbt::deploy(cli),
        "snow dbt copy" => stage::copy(cli),
        "snow notebook deploy" => notebook::deploy(cli),
        "snow helpers v1-to-v2" => helpers::v1_to_v2(cli),
        "snow streamlit deploy" => streamlit::deploy(cli),
        "snow ws bundle" => ws::bundle(cli),
        "snow ws deploy" => ws::deploy(cli),
        "snow ws drop" => ws::drop(cli),
        "snow ws validate" => ws::simple(cli, "action_validate"),
        "snow ws version list" => ws::simple(cli, "action_version_list"),
        "snow ws version create" => ws::simple(cli, "action_version_create"),
        "snow ws version drop" => ws::simple(cli, "action_version_drop"),
        "snow spcs compute-pool deploy" => spcs::deploy::compute_pool(cli),
        "snow spcs image-repository deploy" => spcs::deploy::image_repository(cli),
        "snow spcs service deploy" => spcs::deploy::service(cli),
        "snow spcs service build-image" => spcs::build::build_image(cli),
        "snow spcs service remote-build" => spcs::build::remote_build(cli),
        "snow snowpark build" => snowpark::build(cli),
        "snow snowpark deploy" => snowpark::deploy(cli),
        "snow snowpark package lookup" => snowpark_package::lookup(cli),
        "snow snowpark package create" => snowpark_pip::create(cli),
        "snow snowpark package upload" => snowpark_package::upload(cli),
        "snow feature init"
        | "snow feature sync"
        | "snow feature apply"
        | "snow feature plan"
        | "snow feature list"
        | "snow feature describe"
        | "snow feature ingest"
        | "snow feature query"
        | "snow feature online-service status"
        | "snow feature online-service create"
        | "snow feature online-service drop" => feature::preflight(cli),
        "snow dcm refresh" => dcm_ops::refresh(cli),
        "snow dcm plan" => dcm_ops::plan(cli),
        "snow dcm deploy" => dcm_ops::deploy(cli),
        "snow dcm purge" => dcm_ops::purge(cli),
        "snow dcm raw-analyze" => dcm_ops::raw_analyze(cli),
        "snow dcm test" => dcm_ops::test(cli),
        "snow dcm preview" => dcm_ops::preview(cli),
        "snow streamlit logs" => streamlit_logs::logs(cli),
        "snow auth login" => auth_cmd::login(cli),
        "snow app bundle" => nativeapp::commands::bundle(cli),
        "snow app setup" => nativeapp::commands::setup(cli),
        "snow app run" => nativeapp::commands::run(cli),
        "snow app open" => nativeapp::commands::open(cli),
        "snow app teardown" => nativeapp::commands::teardown(cli),
        "snow app events" => nativeapp::commands::events(cli),
        "snow app publish" => nativeapp::commands::publish(cli),
        "snow app diff" => nativeapp::commands::diff(cli),
        "snow app deploy" => nativeapp::commands::deploy(cli),
        "snow app validate" => nativeapp::commands::validate(cli),
        "snow app version create" => nativeapp::commands::version_create(cli),
        "snow app version drop" => nativeapp::commands::version_drop(cli),
        "snow app version list" => nativeapp::version_list(cli),
        "snow app release-directive list" => nativeapp::release_directive_list(cli),
        "snow app release-channel list" => nativeapp::release_channel_list(cli),
        "snow app release-channel add-accounts" => nativeapp::release_channel_accounts(cli, "add"),
        "snow app release-channel remove-accounts" => {
            nativeapp::release_channel_accounts(cli, "remove")
        }
        "snow app release-channel set-accounts" => nativeapp::release_channel_accounts(cli, "set"),
        "snow app release-channel add-version" => nativeapp::release_channel_version(cli, true),
        "snow app release-channel remove-version" => nativeapp::release_channel_version(cli, false),
        "snow app release-directive set" => nativeapp::release_directive_set(cli),
        "snow app release-directive unset" => nativeapp::release_directive_unset(cli),
        "snow app release-directive add-accounts" => {
            nativeapp::release_directive_accounts(cli, true)
        }
        "snow app release-directive remove-accounts" => {
            nativeapp::release_directive_accounts(cli, false)
        }
        "snow dcm create" => dcm::create(cli),
        "snow dcm drop" => dcm::drop(cli),
        "snow dcm describe" => dcm::describe(cli),
        "snow dcm list-deployments" => dcm::list_deployments(cli),
        "snow dcm drop-deployment" => dcm::drop_deployment(cli),
        "snow custom-image validate" => custom_images::validate(cli),
        "snow upgrade" => upgrade::upgrade(cli),
        "snow ai claude" => ai::claude(cli),
        "snow ai opencode" => ai::opencode(cli),
        "snow init" => init::init(cli),
        _ => Err(SnowError::unexpected(format!(
            "command not implemented: {path}"
        ))),
    }
}
