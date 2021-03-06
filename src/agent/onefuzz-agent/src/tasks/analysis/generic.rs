// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tasks::{
    config::{CommonConfig, SyncedDir},
    heartbeat::HeartbeatSender,
    utils,
};
use anyhow::Result;
use futures::stream::StreamExt;
use onefuzz::{az_copy, blob::url::BlobUrl};
use onefuzz::{expand::Expand, fs::set_executable, fs::OwnedDir};
use reqwest::Url;
use serde::Deserialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    str,
};
use storage_queue::{QueueClient, EMPTY_QUEUE_DELAY};
use tokio::{fs, process::Command};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub analyzer_exe: String,
    pub analyzer_options: Vec<String>,
    pub analyzer_env: HashMap<String, String>,

    pub target_exe: PathBuf,
    pub target_options: Vec<String>,
    pub input_queue: Option<Url>,
    pub crashes: Option<SyncedDir>,

    pub analysis: SyncedDir,
    pub tools: SyncedDir,

    #[serde(flatten)]
    pub common: CommonConfig,
}

pub async fn spawn(config: Config) -> Result<()> {
    let tmp_dir = PathBuf::from(format!("./{}/tmp", config.common.task_id));
    let tmp = OwnedDir::new(tmp_dir);
    tmp.reset().await?;

    utils::init_dir(&config.analysis.path).await?;
    utils::init_dir(&config.tools.path).await?;
    utils::sync_remote_dir(&config.tools, utils::SyncOperation::Pull).await?;
    set_executable(&config.tools.path).await?;
    run_existing(&config).await?;
    poll_inputs(&config, tmp).await?;
    Ok(())
}

async fn run_existing(config: &Config) -> Result<()> {
    if let Some(crashes) = &config.crashes {
        utils::init_dir(&crashes.path).await?;
        utils::sync_remote_dir(&crashes, utils::SyncOperation::Pull).await?;

        let mut read_dir = fs::read_dir(&crashes.path).await?;
        while let Some(file) = read_dir.next().await {
            verbose!("Processing file {:?}", file);
            let file = file?;
            run_tool(file.path(), &config).await?;
        }
        utils::sync_remote_dir(&config.analysis, utils::SyncOperation::Push).await?;
    }
    Ok(())
}

async fn already_checked(config: &Config, input: &BlobUrl) -> Result<bool> {
    let result = if let Some(crashes) = &config.crashes {
        crashes.url.account() == input.account()
            && crashes.url.container() == input.container()
            && crashes.path.join(input.name()).exists()
    } else {
        false
    };

    Ok(result)
}

async fn poll_inputs(config: &Config, tmp_dir: OwnedDir) -> Result<()> {
    let heartbeat = config.common.init_heartbeat();
    if let Some(queue) = &config.input_queue {
        let mut input_queue = QueueClient::new(queue.clone());

        loop {
            heartbeat.alive();
            if let Some(message) = input_queue.pop().await? {
                let input_url = match BlobUrl::parse(str::from_utf8(message.data())?) {
                    Ok(url) => url,
                    Err(err) => {
                        error!("could not parse input URL from queue message: {}", err);
                        return Ok(());
                    }
                };

                if !already_checked(&config, &input_url).await? {
                    let file_name = input_url.name();
                    let mut destination_path = PathBuf::from(tmp_dir.path());
                    destination_path.push(file_name);
                    az_copy::copy(input_url.url().as_ref(), &destination_path, false).await?;

                    run_tool(destination_path, &config).await?;
                    utils::sync_remote_dir(&config.analysis, utils::SyncOperation::Push).await?;
                }
                input_queue.delete(message).await?;
            } else {
                warn!("no new candidate inputs found, sleeping");
                tokio::time::delay_for(EMPTY_QUEUE_DELAY).await;
            }
        }
    }

    Ok(())
}

pub async fn run_tool(input: impl AsRef<Path>, config: &Config) -> Result<()> {
    let mut tool_args = Expand::new();

    tool_args
        .input_path(&input)
        .target_exe(&config.target_exe)
        .target_options(&config.target_options)
        .analyzer_exe(&config.analyzer_exe)
        .analyzer_options(&config.analyzer_options)
        .output_dir(&config.analysis.path);

    let analyzer_path = Expand::new()
        .tools_dir(&config.tools.path)
        .evaluate_value(&config.analyzer_exe)?;

    let mut cmd = Command::new(analyzer_path);
    cmd.kill_on_drop(true).env_remove("RUST_LOG");

    for arg in tool_args.evaluate(&config.analyzer_options)? {
        cmd.arg(arg);
    }

    for (k, v) in &config.analyzer_env {
        cmd.env(k, tool_args.evaluate_value(v)?);
    }

    cmd.output().await?;
    Ok(())
}
