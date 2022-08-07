/*
 * Author: Ruben Fiszel
 * Copyright: Windmill Labs, Inc 2022
 * This file and its contents are licensed under the AGPLv3 License.
 * Please see the included NOTICE for copyright information and
 * LICENSE-AGPL for a copy of the license.
 */

use itertools::Itertools;
use std::{
    collections::HashMap,
    process::{ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use crate::{
    db::DB,
    error::Error,
    jobs::{
        add_completed_job, add_completed_job_error, get_queued_job, postprocess_queued_job, pull,
        JobKind, QueuedJob,
    },
    parser::{self, Typ},
    scripts::{ScriptHash, ScriptLang},
    users::{create_token_for_owner, get_email_from_username},
    variables,
    worker_flow::{
        handle_flow, update_flow_status_after_job_completion, update_flow_status_in_progress,
    },
};

use serde_json::{json, Map, Value};

use tokio::{
    fs::{DirBuilder, File},
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{mpsc, Mutex},
    time::Instant,
};

use async_recursion::async_recursion;

const TMP_DIR: &str = "/tmp/windmill";
const PIP_CACHE_DIR: &str = "/tmp/windmill/cache/pip";
const DENO_CACHE_DIR: &str = "/tmp/windmill/cache/deno";
const NUM_SECS_ENV_CHECK: u64 = 15;

const INCLUDE_DEPS_SH_CONTENT: &str = include_str!("../../nsjail/download_deps.sh");
const NSJAIL_CONFIG_DOWNLOAD_CONTENT: &str = include_str!("../../nsjail/download.config.proto");
const NSJAIL_CONFIG_RUN_PYTHON3_CONTENT: &str =
    include_str!("../../nsjail/run.python3.config.proto");
const NSJAIL_CONFIG_RUN_DENO_CONTENT: &str = include_str!("../../nsjail/run.deno.config.proto");
const MAX_LOG_SIZE: u32 = 50000;
pub struct Metrics {
    pub jobs_failed: prometheus::IntCounter,
}

pub async fn run_worker(
    db: &DB,
    timeout: i32,
    worker_instance: &str,
    worker_name: String,
    i_worker: u64,
    num_workers: u64,
    _mutex: Arc<Mutex<i32>>,
    ip: &str,
    sleep_queue: u64,
    base_url: &str,
    disable_nuser: bool,
    disable_nsjail: bool,
    tx: tokio::sync::broadcast::Sender<()>,
) {
    let worker_dir = format!("{TMP_DIR}/{worker_name}");
    tracing::debug!(worker_dir = %worker_dir, worker_name = %worker_name, "Creating worker dir");

    for x in [&worker_dir, PIP_CACHE_DIR, DENO_CACHE_DIR] {
        DirBuilder::new()
            .recursive(true)
            .create(x)
            .await
            .expect("could not create initial worker dir");
    }

    let _ = write_file(&worker_dir, "download_deps.sh", INCLUDE_DEPS_SH_CONTENT).await;

    let mut last_ping = Instant::now() - Duration::from_secs(NUM_SECS_ENV_CHECK + 1);

    insert_initial_ping(worker_instance, &worker_name, ip, db).await;

    prometheus::register_int_gauge!(prometheus::Opts::new(
        "start_time_seconds",
        "Start time of worker as seconds since unix epoch",
    )
    .const_label("name", &worker_name))
    .expect("register prometheus metric")
    .set(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0),
    );

    let job_duration_seconds = prometheus::register_histogram_vec!(
        prometheus::HistogramOpts::new(
            "job_duration_seconds",
            "Duration between receiving a job and completing it",
        )
        .const_label("name", &worker_name),
        &["workspace_id", "language"],
    )
    .expect("register prometheus metric");

    let jobs_failed = prometheus::register_int_counter_vec!(
        prometheus::Opts::new("jobs_failed", "Number of failed jobs",)
            .const_label("name", &worker_name),
        &["workspace_id", "language"],
    )
    .expect("register prometheus metric");

    let mut jobs_executed = 0;
    let mut rx = tx.subscribe();
    loop {
        if last_ping.elapsed().as_secs() > NUM_SECS_ENV_CHECK {
            sqlx::query!(
                "UPDATE worker_ping SET ping_at = $1, jobs_executed = $2 WHERE worker = $3",
                chrono::Utc::now(),
                jobs_executed,
                &worker_name
            )
            .execute(db)
            .await
            .expect("update worker ping");

            last_ping = Instant::now();
        }

        match pull(db).await {
            Ok(Some(job)) => {
                let label_values = [
                    &job.workspace_id,
                    job.language.as_ref().map(|l| l.as_str()).unwrap_or(""),
                ];

                let _timer = job_duration_seconds
                    .with_label_values(label_values.as_slice())
                    .start_timer();

                jobs_executed += 1;

                let metrics =
                    Metrics { jobs_failed: jobs_failed.with_label_values(label_values.as_slice()) };

                tracing::info!(worker = %worker_name, id = %job.id, "Fetched job");

                if let Some(err) = handle_queued_job(
                    job.clone(),
                    db,
                    timeout,
                    &worker_name,
                    &worker_dir,
                    base_url,
                    disable_nuser,
                    disable_nsjail,
                    &metrics,
                )
                .await
                .err()
                {
                    let m = add_completed_job_error(
                        db,
                        &job,
                        "Unexpected error during job execution:\n".to_string(),
                        &err,
                        &metrics,
                    )
                    .await
                    .map(|(_, m)| m)
                    .unwrap_or_else(|_| Map::new());

                    let _ = postprocess_queued_job(
                        job.is_flow_step,
                        job.schedule_path.clone(),
                        job.script_path.clone(),
                        &job.workspace_id,
                        job.id,
                        db,
                    )
                    .await;

                    if let Some(parent_job_id) = job.parent_job {
                        let updated_flow = update_flow_status_after_job_completion(
                            db,
                            &job,
                            false,
                            serde_json::Value::Object(m),
                            &metrics,
                        )
                        .await;
                        if let Err(err) = updated_flow {
                            if let Ok(mut tx) = db.begin().await {
                                if let Ok(Some(parent_job)) =
                                    get_queued_job(parent_job_id, &job.workspace_id, &mut tx).await
                                {
                                    let _ = add_completed_job_error(
                                        db,
                                        &parent_job,
                                        format!("Unexpected error during flow job error handling:\n{err}")
                                            ,
                                        err,
                                        &metrics,
                                    )
                                    .await;

                                    let _ = postprocess_queued_job(
                                        parent_job.is_flow_step,
                                        parent_job.schedule_path.clone(),
                                        parent_job.script_path.clone(),
                                        &job.workspace_id,
                                        parent_job.id,
                                        db,
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                    tracing::error!(job_id = %job.id, "Error handling job: {err}");
                };
            }
            Ok(None) => (),
            Err(err) => {
                tracing::error!(worker = %worker_name, "run_worker: pulling jobs: {}", err);
            }
        };

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(sleep_queue * num_workers))    => (),
            _ = rx.recv() => {
                 println!("received killpill for worker {}", i_worker);
                 break;
            }
        }
    }
}

async fn insert_initial_ping(worker_instance: &str, worker_name: &str, ip: &str, db: &DB) {
    sqlx::query!(
        "INSERT INTO worker_ping (worker_instance, worker, ip) VALUES ($1, $2, $3)",
        worker_instance,
        worker_name,
        ip
    )
    .execute(db)
    .await
    .expect("insert worker_ping initial value");
}

async fn handle_queued_job(
    job: QueuedJob,
    db: &sqlx::Pool<sqlx::Postgres>,
    timeout: i32,
    worker_name: &str,
    worker_dir: &str,
    base_url: &str,
    disable_nuser: bool,
    disable_nsjail: bool,
    metrics: &Metrics,
) -> crate::error::Result<()> {
    let job_id = job.id;
    let w_id = &job.workspace_id.clone();

    match job.job_kind {
        JobKind::FlowPreview | JobKind::Flow => {
            handle_flow(
                &job,
                db,
                job.args.clone().unwrap_or_else(|| serde_json::Value::Null),
            )
            .await?;
        }
        _ => {
            let mut logs = "".to_string();
            let mut last_line = "{}".to_string();

            if job.is_flow_step {
                update_flow_status_in_progress(
                    db,
                    &job.workspace_id,
                    job.parent_job
                        .ok_or_else(|| Error::InternalErr(format!("expected parent job")))?,
                    job.id,
                )
                .await?;
            }

            let execution = handle_job(
                &job,
                db,
                timeout,
                worker_name,
                worker_dir,
                &mut logs,
                &mut last_line,
                base_url,
                disable_nuser,
                disable_nsjail,
            )
            .await;

            match execution {
                Ok(r) => {
                    add_completed_job(db, &job, true, false, r.clone(), logs).await?;
                    if job.is_flow_step {
                        update_flow_status_after_job_completion(db, &job, true, r, metrics).await?;
                    }
                }
                Err(e) => {
                    let (_, output_map) =
                        add_completed_job_error(db, &job, logs, e, &metrics).await?;
                    if job.is_flow_step {
                        update_flow_status_after_job_completion(
                            db,
                            &job,
                            false,
                            serde_json::Value::Object(output_map),
                            metrics,
                        )
                        .await?;
                    }
                }
            };

            let _ = postprocess_queued_job(
                job.is_flow_step,
                job.schedule_path,
                job.script_path,
                &w_id,
                job_id,
                db,
            )
            .await;
        }
    }
    Ok(())
}

async fn write_file(dir: &str, path: &str, content: &str) -> Result<File, Error> {
    let path = format!("{}/{}", dir, path);
    let mut file = File::create(&path).await?;
    file.write_all(content.as_bytes()).await?;
    Ok(file)
}

#[async_recursion]
async fn transform_json_value(token: &str, workspace: &str, base_url: &str, v: Value) -> Value {
    match v {
        Value::String(y) if y.starts_with("$var:") => {
            let path = y.strip_prefix("$var:").unwrap();
            let v = crate::client::get_variable(workspace, path, token, base_url)
                .await
                .unwrap_or_else(|_| format!("error fetching variable {path}"));
            Value::String(v)
        }
        Value::String(y) if y.starts_with("$res:") => {
            let path = y.strip_prefix("$res:").unwrap();
            if path.split("/").count() < 2 {
                return Value::String(format!("resource path: {path} is ill-defined"));
            }
            let v = crate::client::get_resource(workspace, path, token, base_url)
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| Value::String(format!("error fetching resource {path}")));
            transform_json_value(token, workspace, base_url, v).await
        }
        Value::Object(mut m) => {
            for (a, b) in m.clone().into_iter() {
                m.insert(a, transform_json_value(token, workspace, base_url, b).await);
            }
            Value::Object(m)
        }
        a @ _ => a,
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_job(
    job: &QueuedJob,
    db: &DB,
    timeout: i32,
    worker_name: &str,
    worker_dir: &str,
    logs: &mut String,
    last_line: &mut String,
    base_url: &str,
    disable_nuser: bool,
    disable_nsjail: bool,
) -> Result<serde_json::Value, Error> {
    tracing::info!(
        worker = %worker_name,
        job_id = %job.id,
        workspace_id = %job.workspace_id,
        "handling job"
    );

    logs.push_str(&format!("job {} on worker {}\n", &job.id, &worker_name));
    let job_dir = format!("{worker_dir}/{}", job.id);
    DirBuilder::new()
        .recursive(true)
        .create(&format!("{job_dir}/dependencies"))
        .await
        .expect("could not create initial job dir");

    let mut status: Result<ExitStatus, Error> =
        Err(Error::InternalErr("job not started".to_string()));

    if matches!(job.job_kind, JobKind::Dependencies) {
        handle_dependency_job(job, logs, &job_dir, &mut status, db, last_line, timeout).await?;
    } else {
        handle_nondep_job(
            job,
            db,
            &job_dir,
            worker_dir,
            disable_nuser,
            disable_nsjail,
            logs,
            &mut status,
            last_line,
            timeout,
            base_url,
        )
        .await?;
    }
    tokio::fs::remove_dir_all(job_dir).await?;

    if status.is_ok() && status.as_ref().unwrap().success() {
        let result = serde_json::from_str::<serde_json::Value>(last_line).map_err(|e| {
            Error::ExecutionErr(format!(
                "result {} is not parsable.\n err: {}",
                last_line,
                e.to_string()
            ))
        })?;
        Ok(result)
    } else {
        let err = match status {
            Ok(_) => {
                let s = format!(
                    "Error during execution of the script\nlast 5 logs lines:\n{}",
                    logs.lines()
                        .skip(logs.lines().count().max(5) - 5)
                        .join("\n")
                );
                logs.push_str("\n\n--- ERROR ---\n");
                s
            }
            Err(err) => format!("error before termination: {err}"),
        };
        Err(Error::ExecutionErr(err))
    }
}

async fn handle_nondep_job(
    job: &QueuedJob,
    db: &sqlx::Pool<sqlx::Postgres>,
    job_dir: &String,
    worker_dir: &str,
    disable_nuser: bool,
    disable_nsjail: bool,
    logs: &mut String,
    status: &mut Result<ExitStatus, Error>,
    last_line: &mut String,
    timeout: i32,
    base_url: &str,
) -> Result<(), Error> {
    let (inner_content, requirements_o, language) = if matches!(job.job_kind, JobKind::Preview)
        || matches!(job.job_kind, JobKind::Script_Hub)
    {
        let code = (job.raw_code.as_ref().unwrap_or(&"no raw code".to_owned())).to_owned();
        let reqs = if job
            .language
            .as_ref()
            .map(|x| matches!(x, ScriptLang::Python3))
            .unwrap_or(false)
        {
            Some(parser::parse_python_imports(&code)?.join("\n"))
        } else {
            None
        };
        (code, reqs, job.language.to_owned())
    } else {
        sqlx::query_as::<_, (String, Option<String>, Option<ScriptLang>)>(
            "SELECT content, lock, language FROM script WHERE hash = $1 AND (workspace_id = $2 OR \
             workspace_id = 'starter')",
        )
        .bind(&job.script_hash.unwrap_or(ScriptHash(0)).0)
        .bind(&job.workspace_id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| Error::InternalErr(format!("expected content and lock")))?
    };
    let worker_name = worker_dir.split("/").last().unwrap_or("unknown");
    match language {
        None => {
            return Err(Error::ExecutionErr(
                "Require language to be not null".to_string(),
            ))?;
        }
        Some(ScriptLang::Python3) => {
            let requirements =
                requirements_o.ok_or_else(|| Error::InternalErr(format!("lockfile missing")))?;

            if !disable_nsjail {
                let _ = write_file(
                    job_dir,
                    "download.config.proto",
                    &NSJAIL_CONFIG_DOWNLOAD_CONTENT
                        .replace("{JOB_DIR}", job_dir)
                        .replace("{WORKER_DIR}", &worker_dir)
                        .replace("{CACHE_DIR}", PIP_CACHE_DIR)
                        .replace("{CLONE_NEWUSER}", &(!disable_nuser).to_string()),
                )
                .await?;
            }
            let _ = write_file(job_dir, "requirements.txt", &requirements).await?;

            tracing::info!(
                worker_name = %worker_name,
                job_id = %job.id,
                workspace_id = %job.workspace_id,
                "started setup python dependencies"
            );
            let child = if !disable_nsjail {
                Command::new("nsjail")
                    .current_dir(job_dir)
                    .args(vec!["--config", "download.config.proto"])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?
            } else {
                Command::new("/usr/local/bin/python3")
                    .current_dir(job_dir)
                    .args(vec![
                        "-m",
                        "pip",
                        "install",
                        "--no-color",
                        "--isolated",
                        "--no-warn-conflicts",
                        "--disable-pip-version-check",
                        "-t",
                        "./dependencies",
                        "-r",
                        "./requirements.txt",
                    ])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?
            };

            logs.push_str("\n--- PIP DEPENDENCIES INSTALL ---\n");
            *status = handle_child(job, db, logs, last_line, timeout, child).await;
            tracing::info!(
                worker_name = %worker_name,
                job_id = %job.id,
                workspace_id = %job.workspace_id,
                is_ok = status.is_ok(),
                "finished setup python dependencies"
            );
            if status.is_ok() {
                logs.push_str("\n\n--- PYTHON CODE EXECUTION ---\n");

                set_logs(logs, job.id, db).await;

                let _ = write_file(job_dir, "inner.py", &inner_content).await?;

                let sig = crate::parser::parse_python_signature(&inner_content)?;
                let transforms = sig
                    .args
                    .into_iter()
                    .map(|x| match x.typ {
                        Typ::Bytes => {
                            format!(
                                "if \"{}\" in kwargs and kwargs[\"{}\"] is not None:\n    \
                                     kwargs[\"{}\"] = base64.b64decode(kwargs[\"{}\"])\n",
                                x.name, x.name, x.name, x.name
                            )
                        }
                        Typ::Datetime => {
                            format!(
                                "if \"{}\" in kwargs and kwargs[\"{}\"] is not None:\n    \
                                     kwargs[\"{}\"] = datetime.strptime(kwargs[\"{}\"], \
                                     '%Y-%m-%dT%H:%M')\n",
                                x.name, x.name, x.name, x.name
                            )
                        }
                        _ => "".to_string(),
                    })
                    .collect::<Vec<String>>()
                    .join("");

                let tx = db.begin().await?;

                let token = create_token_for_owner(
                    &db,
                    &job.workspace_id,
                    &job.permissioned_as,
                    crate::users::NewToken {
                        label: Some("ephemeral-script".to_string()),
                        expiration: Some(
                            chrono::Utc::now() + chrono::Duration::seconds((timeout * 2).into()),
                        ),
                    },
                    &job.created_by,
                )
                .await?;

                let args = if let Some(args) = &job.args {
                    Some(
                        transform_json_value(&token, &job.workspace_id, base_url, args.clone())
                            .await,
                    )
                } else {
                    None
                };
                let ser_args =
                    serde_json::to_string(&args).map_err(|e| Error::ExecutionErr(e.to_string()))?;
                write_file(job_dir, "args.json", &ser_args).await?;

                let wrapper_content: String = format!(
                    r#"
import json
import base64
from datetime import datetime

inner_script = __import__("inner")

with open("args.json") as f:
    kwargs = json.load(f, strict=False)
for k, v in list(kwargs.items()):
    if v == '<function call>':
        del kwargs[k]
{transforms}
res = inner_script.main(**kwargs)
res_json = json.dumps(res, separators=(',', ':'), default=str).replace('\n', '')
print()
print("result:")
print(res_json)
"#,
                );
                write_file(job_dir, "main.py", &wrapper_content).await?;

                tx.commit().await?;
                let mut reserved_variables = get_reserved_variables(job, token, db).await?;
                if !disable_nuser {
                    let _ = write_file(
                        job_dir,
                        "run.config.proto",
                        &NSJAIL_CONFIG_RUN_PYTHON3_CONTENT
                            .replace("{JOB_DIR}", job_dir)
                            .replace("{CLONE_NEWUSER}", &(!disable_nuser).to_string()),
                    )
                    .await?;
                } else {
                    reserved_variables
                        .insert("PYTHONPATH".to_string(), format!("{job_dir}/dependencies"));
                }

                tracing::info!(
                    worker_name = %worker_name,
                    job_id = %job.id,
                    workspace_id = %job.workspace_id,
                    "started python code execution"
                );
                let child = if !disable_nuser {
                    Command::new("nsjail")
                        .current_dir(job_dir)
                        .env_clear()
                        .envs(reserved_variables)
                        .args(vec![
                            "--config",
                            "run.config.proto",
                            "--",
                            "/usr/local/bin/python3",
                            "-u",
                            "/tmp/main.py",
                        ])
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()?
                } else {
                    Command::new("/usr/local/bin/python3")
                        .current_dir(job_dir)
                        .env_clear()
                        .envs(reserved_variables)
                        .args(vec!["-u", "main.py"])
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()?
                };
                *status = handle_child(job, db, logs, last_line, timeout, child).await;
                tracing::info!(
                    worker_name = %worker_name,
                    job_id = %job.id,
                    workspace_id = %job.workspace_id,
                    is_ok = status.is_ok(),
                    "finished python code execution"
                );
            }
        }
        Some(ScriptLang::Deno) => {
            logs.push_str("\n\n--- DENO CODE EXECUTION ---\n");

            set_logs(logs, job.id, db).await;

            let _ = write_file(job_dir, "inner.ts", &inner_content).await?;

            let sig = crate::parser::parse_deno_signature(&inner_content)?;
            //             let transforms = sig.args.clone().into_iter().map(|x| match x.typ {
            //     Typ::Bytes => format!("if \"{}\" in kwargs and kwargs[\"{}\"] is not None:\n    kwargs[\"{}\"] = base64.b64decode(kwargs[\"{}\"])\n", x.name, x.name, x.name, x.name),
            //     Typ::Datetime => format!("if \"{}\" in kwargs and kwargs[\"{}\"] is not None:\n    kwargs[\"{}\"] = datetime.strptime(kwargs[\"{}\"], '%Y-%m-%dT%H:%M')\n", x.name, x.name, x.name, x.name),
            //     _ => "".to_string()
            // }).collect::<Vec<String>>().join("");

            let tx = db.begin().await?;

            let token = create_token_for_owner(
                &db,
                &job.workspace_id,
                &job.permissioned_as,
                crate::users::NewToken {
                    label: Some("ephemeral-script".to_string()),
                    expiration: Some(
                        chrono::Utc::now() + chrono::Duration::seconds((timeout * 2).into()),
                    ),
                },
                &job.created_by,
            )
            .await?;

            let args = if let Some(args) = &job.args {
                Some(transform_json_value(&token, &job.workspace_id, base_url, args.clone()).await)
            } else {
                None
            };
            let ser_args =
                serde_json::to_string(&args).map_err(|e| Error::ExecutionErr(e.to_string()))?;
            write_file(job_dir, "args.json", &ser_args).await?;

            let spread = sig.args.into_iter().map(|x| x.name).join(",");
            let wrapper_content: String = format!(
                r#"
import {{ main }} from "./inner.ts";

const args = await Deno.readTextFile("args.json")
    .then(JSON.parse)
    .then(({{ {spread} }}) => [ {spread} ])

async function run() {{
    let res: any = await main(...args);
    const res_json = JSON.stringify(res ?? null, (key, value) => typeof value === 'undefined' ? null : value);
    console.log();
    console.log("result:");
    console.log(res_json);
    Deno.exit(0);
}}
run();
"#,
            );
            write_file(job_dir, "main.ts", &wrapper_content).await?;

            tx.commit().await?;

            let mut reserved_variables = get_reserved_variables(job, token, db).await?;
            reserved_variables.insert("RUST_LOG".to_string(), "info".to_string());

            if !disable_nuser {
                let _ = write_file(
                    job_dir,
                    "run.config.proto",
                    &NSJAIL_CONFIG_RUN_DENO_CONTENT
                        .replace("{JOB_DIR}", job_dir)
                        .replace("{CACHE_DIR}", DENO_CACHE_DIR)
                        .replace("{CLONE_NEWUSER}", &(!disable_nuser).to_string()),
                )
                .await?;
            }

            tracing::info!(
                worker_name = %worker_name,
                job_id = %job.id,
                workspace_id = %job.workspace_id,
                "started deno code execution"
            );
            let child = if !disable_nsjail {
                Command::new("nsjail")
                    .current_dir(job_dir)
                    .env_clear()
                    .envs(reserved_variables)
                    .args(vec![
                        "--config",
                        "run.config.proto",
                        "--",
                        "/usr/bin/deno",
                        "run",
                        "--unstable",
                        "--v8-flags=--max-heap-size=2048",
                        "-A",
                        "/tmp/main.ts",
                    ])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?
            } else {
                Command::new("/usr/bin/deno")
                    .current_dir(job_dir)
                    .env_clear()
                    .envs(reserved_variables)
                    .args(vec![
                        "run",
                        "--unstable",
                        "--v8-flags=--max-heap-size=2048",
                        "-A",
                        "main.ts",
                    ])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?
            };
            *status = handle_child(job, db, logs, last_line, timeout, child).await;
            tracing::info!(
                worker_name = %worker_name,
                job_id = %job.id,
                workspace_id = %job.workspace_id,
                is_ok = status.is_ok(),
                "finished deno code execution"
            );
        }
    }
    Ok(())
}

async fn handle_dependency_job(
    job: &QueuedJob,
    logs: &mut String,
    job_dir: &String,
    status: &mut Result<ExitStatus, Error>,
    db: &sqlx::Pool<sqlx::Postgres>,
    last_line: &mut String,
    timeout: i32,
) -> Result<(), Error> {
    let requirements = job
        .raw_code
        .as_ref()
        .ok_or_else(|| Error::ExecutionErr("missing requirements".to_string()))?;
    logs.push_str(&format!("content of requirements:\n{}\n", &requirements));
    let file = "requirements.in";
    write_file(job_dir, file, &requirements).await?;
    let child = Command::new("pip-compile")
        .current_dir(job_dir)
        .args(vec!["-q", "--no-header", file])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    *status = handle_child(job, db, logs, last_line, timeout, child).await;
    Ok(if status.is_ok() && status.as_ref().unwrap().success() {
        let path_lock = format!("{}/requirements.txt", job_dir);
        let mut file = File::open(path_lock).await?;

        let mut content = "".to_string();
        file.read_to_string(&mut content).await?;
        content = content
            .lines()
            .filter(|x| !x.trim_start().starts_with('#'))
            .map(|x| x.to_string())
            .collect::<Vec<String>>()
            .join("\n");
        let as_json = json!(content);

        *last_line =
            format!(r#"{{ "success": "Successful lock file generation", "lock": {as_json} }}"#);

        sqlx::query!(
            "UPDATE script SET lock = $1 WHERE hash = $2 AND workspace_id = $3",
            &content,
            &job.script_hash.unwrap_or(ScriptHash(0)).0,
            &job.workspace_id
        )
        .execute(db)
        .await?;
    } else {
        sqlx::query!(
            "UPDATE script SET lock_error_logs = $1 WHERE hash = $2 AND workspace_id = $3",
            &logs.clone(),
            &job.script_hash.unwrap_or(ScriptHash(0)).0,
            &job.workspace_id
        )
        .execute(db)
        .await?;
    })
}

async fn get_reserved_variables(
    job: &QueuedJob,
    token: String,
    db: &sqlx::Pool<sqlx::Postgres>,
) -> Result<HashMap<String, String>, Error> {
    let flow_path = if let Some(uuid) = job.parent_job {
        sqlx::query_scalar!("SELECT script_path FROM queue WHERE id = $1", uuid)
            .fetch_optional(db)
            .await?
            .flatten()
    } else {
        None
    };
    let variables = variables::get_reserved_variables(
        &job.workspace_id,
        &token,
        &get_email_from_username(&job.created_by, db)
            .await?
            .unwrap_or_else(|| "nosuitable@email.xyz".to_string()),
        &job.created_by,
        &job.id.to_string(),
        &job.permissioned_as,
        job.script_path.clone(),
        flow_path,
        job.schedule_path.clone(),
    );
    Ok(variables
        .into_iter()
        .map(|rv| (rv.name, rv.value))
        .collect())
}

async fn handle_child(
    job: &QueuedJob,
    db: &DB,
    logs: &mut String,
    last_line: &mut String,
    timeout: i32,
    mut child: Child,
) -> crate::error::Result<ExitStatus> {
    let stderr = child
        .stderr
        .take()
        .expect("child did not have a handle to stdout");

    let stdout = child
        .stdout
        .take()
        .expect("child did not have a handle to stdout");

    let mut reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();

    let done = Arc::new(AtomicBool::new(false));

    let done2 = done.clone();
    let done3 = done.clone();
    let done4 = done.clone();
    // Ensure the child process is spawned in the runtime so it can
    // make progress on its own while we await for any output.
    let handle = tokio::spawn(async move {
        let inner_done = done2.clone();
        let r: Result<ExitStatus, anyhow::Error> = tokio::select! {
            r = child.wait() => {
                inner_done.store(true, Ordering::Relaxed);
                Ok(r?)
            }
            _ = async move {
                while !done2.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            } => {
                child.kill().await?;
                return Err(Error::ExecutionErr("execution interrupted".to_string()).into())
            }
        };
        r
    });

    let (tx, mut rx) = mpsc::channel::<String>(100);
    let id = job.id;

    tokio::spawn(async move {
        while !done4.load(Ordering::Relaxed) {
            let send = tokio::select! {
                Ok(Some(out)) = reader.next_line() => {
                    if out.len() > MAX_LOG_SIZE as usize {
                        tracing::info!("Line is too big");
                        let _ = tx.send(format!("Line is too big")).await;
                        done4.store(true, Ordering::Relaxed);
                        break;
                    } else {
                        tx.send(out).await
                    }
                },
                Ok(Some(err)) = stderr_reader.next_line() => {
                    if err.len() > MAX_LOG_SIZE as usize {
                        tracing::info!("Line is too big");
                        let _ = tx.send(format!("Line is too big")).await;
                        done4.store(true, Ordering::Relaxed);
                        break;
                    } else {
                        tx.send(err).await
                    }
                },
                else => {
                    break
                },
            };
            if send.err().is_some() {
                tracing::error!("error sending log line");
            };
        }
    });

    let db2 = db.clone();

    tokio::spawn(async move {
        while !&done3.load(Ordering::Relaxed) {
            let q = sqlx::query!(
                "UPDATE queue SET last_ping = $1 WHERE id = $2",
                chrono::Utc::now(),
                id
            )
            .execute(&db2)
            .await;

            if q.is_err() {
                tracing::error!("error setting last ping for id {}", id);
            }

            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    let mut start = logs.chars().count();
    let mut last_update = chrono::Utc::now().timestamp_millis();

    while !done.load(Ordering::Relaxed) {
        let diff = 500 - (chrono::Utc::now().timestamp_millis() - last_update);
        let sleeping_future = if diff > 0 as i64 {
            tokio::time::sleep(Duration::from_millis(diff as u64))
        } else {
            //TODO make it just resolve immediately
            tokio::time::sleep(Duration::from_millis(0))
        };
        tokio::select! {
            _ = sleeping_future => {
                let end = logs.chars().count();

                let to_send = logs.chars().skip(start).collect::<String>();

                if start != end {
                    concat_logs(&to_send, id, db).await;
                    start = end;
                }

                let canceled = sqlx::query_scalar!("SELECT canceled FROM queue WHERE id = $1", id)
                    .fetch_one(db)
                    .await
                    .map_err(|e| tracing::error!("error getting canceled for id {}: {e}", id))
                    .unwrap_or(false);

                if canceled {
                    tracing::info!("killed after cancel: {}", job.id);
                    done.store(true, Ordering::Relaxed);
                }

                let has_timeout = job
                    .started_at
                    .map(|sa| (chrono::Utc::now() - sa).num_seconds() > timeout as i64)
                    .unwrap_or(false);

                if has_timeout {
                    let q = sqlx::query(&format!(
                        "UPDATE queue SET canceled = true, canceled_by = 'timeout', \
                            canceled_reason = 'duration > {}' WHERE id = $1",
                        timeout
                    ))
                    .bind(id)
                    .execute(db)
                    .await;

                    if q.is_err() {
                        tracing::error!("error setting canceled for id {}", id);
                    }
                }
                last_update = chrono::Utc::now().timestamp_millis();
            },
            nl = rx.recv() => {

                if let Some(nl) = nl {
                    if logs.chars().count() > MAX_LOG_SIZE as usize{
                        tracing::info!("Too many logs lines: {}", job.id);
                        logs.push_str("Too many logs lines. Limit is 50000 chars. Killing job.");
                        done.store(true, Ordering::Relaxed);
                    }

                    logs.push('\n');
                    logs.push_str(&nl);

                    *last_line = nl;
                } else {
                    let to_send = logs.chars().skip(start).collect::<String>();
                    concat_logs(&to_send, id, db).await;
                    break;
                }
            },
        }
    }

    let status = handle
        .await
        .map_err(|e| Error::ExecutionErr(e.to_string()))??;
    Ok(status)
}

async fn set_logs(logs: &str, id: uuid::Uuid, db: &DB) {
    if sqlx::query!(
        "UPDATE queue SET logs = $1 WHERE id = $2",
        logs.to_owned(),
        id
    )
    .execute(db)
    .await
    .is_err()
    {
        tracing::error!("error updating logs for id {}", id)
    };
}

async fn concat_logs(logs: &str, id: uuid::Uuid, db: &DB) {
    if sqlx::query!(
        "UPDATE queue SET logs = concat(logs, $1::text) WHERE id = $2",
        logs.to_owned(),
        id
    )
    .execute(db)
    .await
    .is_err()
    {
        tracing::error!("error updating logs for id {}", id)
    };
}

pub async fn restart_zombie_jobs_periodically(
    db: &DB,
    timeout: i32,
    mut rx: tokio::sync::broadcast::Receiver<()>,
) {
    loop {
        let restarted = sqlx::query!(
            "UPDATE queue SET running = false WHERE last_ping < $1 and running = true RETURNING id, workspace_id",
            chrono::Utc::now() - chrono::Duration::seconds(timeout as i64 * 5)
        )
        .fetch_all(db)
        .await
        .ok()
        .unwrap_or_else(|| vec![]);

        for r in restarted {
            tracing::info!("restarted zombie job {} {}", r.id, r.workspace_id);
        }

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(60))    => (),
            _ = rx.recv() => {
                    println!("received killpill for monitor job");
                    break;
            }
        }
    }
}
