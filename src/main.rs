// Copyright 2019-2022 Parity Technologies (UK) Ltd.
// This file is part of subpub.
//
// subpub is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// subpub is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with subpub.  If not, see <http://www.gnu.org/licenses/>.

mod crate_details;
mod crates;
mod external;
mod git;
mod toml;
mod version;

use anyhow::anyhow;
use anyhow::Context;
use clap::{Parser, Subcommand};
use crates::Crates;
use git::with_git_checkpoint;
use std::collections::HashSet;
use std::path::PathBuf;
use tracing::{info, span, Level};
use tracing_subscriber::prelude::*;

use crate::git::GitCheckpoint;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    #[clap(about = "Publish crates in order from least to most dependees")]
    Publish(PublishOpts),
}

#[derive(Parser, Debug, Clone)]
#[clap(author, version, about, long_about = None)]
struct PublishOpts {
    #[clap(long, help = "Path to the workspace root")]
    root: PathBuf,

    #[clap(
        short = 'c',
        long = "crate",
        help = "Select crates to be published. If empty, all crates in the workspace of --root will be published."
    )]
    crates: Vec<String>,

    #[clap(
        short = 's',
        long = "start-from",
        help = "Start publishing from this crate. Useful to resume the process in case it fails for some reason. This option does not take into account crates which are ordered before the given crate, so you might potentially miiss added or renamed crates in case they are ordered before the given crate."
    )]
    start_from: Option<String>,

    #[clap(
        short = 'v',
        long = "verify-from",
        help = "When publishing, only verify crates starting from this crate. Useful to skip the verification process of all crates up to the given crate, which can be time-consuming if the crate depends on lots of other crates that are expensive to verify."
    )]
    verify_from: Option<String>,

    #[clap(
        long = "after-publish-delay",
        help = "How many seconds to wait after publishing a crate. Useful to work around crates.io publishing rate limits in case you need to publish lots of crates."
    )]
    after_publish_delay: Option<u64>,

    #[clap(
        long = "include-crates-dependents",
        help = "Also include dependents of crates which were passed through the CLI"
    )]
    include_crates_dependents: bool,

    #[clap(
        short = 'e',
        long = "exclude",
        help = "Crates to be excluded from the publishing process."
    )]
    exclude: Vec<String>,

    #[clap(
        short = 'k',
        long = "post-check",
        help = "Run post checks, e.g. cargo check, after publishing."
    )]
    post_check: bool,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .without_time()
                .with_writer(std::io::stdout)
                .with_target(false),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .without_time()
                .with_writer(std::io::stderr)
                .with_target(false)
                .with_filter(tracing_subscriber::filter::LevelFilter::ERROR),
        )
        .init();

    let args = Args::parse();

    match args.command {
        Command::Publish(opts) => publish(opts),
    }
}

fn publish(opts: PublishOpts) -> anyhow::Result<()> {
    let mut crates = Crates::load_crates_in_workspace(opts.root.clone())?;
    crates.setup_crates()?;

    struct OrderedCrate {
        name: String,
        rank: usize,
    }
    let mut publish_order: Vec<OrderedCrate> = vec![];
    loop {
        let mut progressed = false;
        for (krate, details) in &crates.details {
            if publish_order
                .iter()
                .any(|ord_crate| ord_crate.name == *krate)
            {
                continue;
            }
            let deps: HashSet<&String> = HashSet::from_iter(details.deps_to_publish());
            let ordered_deps = publish_order
                .iter()
                .filter(|ord_crate| deps.iter().any(|dep| **dep == ord_crate.name))
                .collect::<Vec<_>>();
            if ordered_deps.len() == deps.len() {
                publish_order.push(OrderedCrate {
                    rank: ordered_deps.iter().fold(1usize, |acc, ord_crate| {
                        acc.checked_add(ord_crate.rank).unwrap()
                    }),
                    name: krate.into(),
                });
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    publish_order.sort_by(|a, b| {
        use std::cmp::Ordering;
        match a.rank.cmp(&b.rank) {
            Ordering::Equal => a.name.cmp(&b.name),
            other => other,
        }
    });
    let publish_order: Vec<String> = publish_order
        .into_iter()
        .map(|ord_crate| ord_crate.name)
        .collect();
    info!(
        "If we were to publish all crates, it would happen in this order: {}",
        publish_order
            .iter()
            .map(|krate| krate.to_owned())
            .collect::<Vec<_>>()
            .join(", ")
    );

    let unordered_crates = crates
        .details
        .keys()
        .filter(|krate| !publish_order.iter().any(|ord_crate| ord_crate == *krate))
        .collect::<Vec<_>>();
    if !unordered_crates.is_empty() {
        anyhow::bail!(
            "Failed to determine publish order for the following crates: {}",
            unordered_crates
                .iter()
                .map(|krate| (*krate).into())
                .collect::<Vec<String>>()
                .join(", ")
        );
    }

    let crates_to_exclude = {
        let mut crates_to_exclude: HashSet<&String> = HashSet::from_iter(opts.exclude.iter());

        loop {
            let mut progressed = false;

            // Exclude also crates which depend on crates to be excluded
            let excluded_crates = crates_to_exclude
                .iter()
                .map(|excluded_crate| excluded_crate.to_owned())
                .collect::<Vec<_>>();
            for excluded_crate in excluded_crates {
                for krate in &publish_order {
                    let details = crates
                        .details
                        .get(krate)
                        .with_context(|| format!("Crate not found: {krate}"))?;
                    if details.deps_to_publish().any(|dep| dep == excluded_crate) {
                        let inserted = crates_to_exclude.insert(krate);
                        if inserted {
                            info!(
                                "Excluding crate {} because it depends on {}",
                                krate, excluded_crate
                            );
                        }
                        progressed |= inserted;
                    }
                }
            }

            if !progressed {
                break;
            }
        }

        crates_to_exclude
    };

    let input_crates = if opts.crates.is_empty() {
        publish_order
            .iter()
            .filter_map(|krate| {
                if opts
                    .start_from
                    .as_ref()
                    .map(|start_from| start_from == krate)
                    .unwrap_or(false)
                {
                    return Some(Ok(krate));
                }
                if crates_to_exclude
                    .iter()
                    .any(|excluded_crate| *excluded_crate == krate)
                {
                    return None;
                }
                if let Some(details) = crates.details.get(krate) {
                    if details.should_be_published {
                        Some(Ok(krate))
                    } else {
                        info!("Filtering out crate {krate} because it should not be published");
                        None
                    }
                } else {
                    Some(Err(anyhow!("Crate not found: {}", krate)))
                }
            })
            .collect::<anyhow::Result<Vec<_>>>()?
    } else {
        let mut crates_to_include: HashSet<&String> = HashSet::from_iter(opts.crates.iter());

        if opts.include_crates_dependents {
            loop {
                let mut progressed = false;

                let included_crates = crates_to_include
                    .iter()
                    .map(|krate| krate.to_owned())
                    .collect::<Vec<_>>();
                for included_crate in included_crates {
                    for krate in &publish_order {
                        if crates_to_exclude.get(krate).is_some() {
                            continue;
                        }
                        let details = crates
                            .details
                            .get(krate)
                            .with_context(|| format!("Crate not found: {krate}"))?;
                        if details.should_be_published
                            && details.deps_to_publish().any(|dep| dep == included_crate)
                        {
                            let inserted = crates_to_include.insert(krate);
                            if inserted {
                                info!(
                                    "Including crate {} because it depends on {}",
                                    krate, included_crate
                                );
                            }
                            progressed |= inserted;
                        }
                    }
                }

                if !progressed {
                    break;
                }
            }
        }

        publish_order
            .iter()
            .filter(|ordered_crate| crates_to_include.get(ordered_crate).is_some())
            .collect::<Vec<_>>()
    };

    let selected_crates = if let Some(start_from) = opts.start_from {
        let mut input_crates = input_crates;
        let mut keep = false;
        input_crates.retain_mut(|krate| {
            if **krate == start_from {
                keep = true;
                if crates_to_exclude
                    .iter()
                    .any(|excluded_crate| excluded_crate == krate)
                {
                    return false;
                }
            }
            keep
        });
        input_crates
    } else {
        input_crates
    };
    if selected_crates.is_empty() {
        anyhow::bail!("No crates could be selected from the CLI options");
    }

    info!(
        "Selected the following crates to be published, in order: {}",
        selected_crates
            .iter()
            .map(|krate| (*krate).into())
            .collect::<Vec<String>>()
            .join(", ")
    );

    fn validate_crates(
        crates: &Crates,
        initial_crate: &String,
        parent_crate: Option<&String>,
        krate: &String,
        excluded_crates: &HashSet<&String>,
        visited_crates: &[&String],
    ) -> anyhow::Result<()> {
        if visited_crates
            .iter()
            .any(|visited_crate| *visited_crate == krate)
        {
            return Ok(());
        }

        if excluded_crates
            .iter()
            .any(|excluded_crate| *excluded_crate == krate)
        {
            if let Some(parent_crate) = parent_crate {
                anyhow::bail!("Crate {krate} was excluded from CLI options, but it is a dependency of {parent_crate}, and that is a dependency of {initial_crate}, which would be published.");
            } else {
                anyhow::bail!("Crate {krate} was excluded from CLI options, but it is a dependency of  {initial_crate}, which would be published.");
            }
        }

        let details = crates
            .details
            .get(krate)
            .with_context(|| format!("Crate not found: {krate}"))?;
        if !details.should_be_published {
            if let Some(parent_crate) = parent_crate {
                anyhow::bail!("Crate {krate} should not be published, but it is a dependency of {parent_crate}, and that is a dependency of {initial_crate}, which would be published. Check if {krate} has \"publish = false\" in {:?}.", details.toml_path);
            } else {
                anyhow::bail!("Crate {krate} should not be published, but it is a dependency of {initial_crate}, which would be published. Check if {krate} has \"publish = false\" in {:?}.", details.toml_path);
            }
        }

        for dep in details.deps_to_publish() {
            let visited_crates = visited_crates
                .iter()
                .copied()
                .chain(vec![krate].into_iter())
                .collect::<Vec<_>>();
            validate_crates(
                crates,
                initial_crate,
                if krate == initial_crate {
                    None
                } else {
                    Some(krate)
                },
                dep,
                excluded_crates,
                &visited_crates,
            )?;
        }

        Ok(())
    }
    for krate in &selected_crates {
        info!("Validating crate {krate}");
        validate_crates(&crates, krate, None, krate, &crates_to_exclude, &[])?;
    }

    if let Ok(registry) = std::env::var("SPUB_REGISTRY") {
        for (_, details) in crates.details.iter() {
            details.set_registry(&registry)?
        }
    }

    let crates_to_verify = opts.verify_from.as_ref().map(|verify_from| {
        let mut verify = false;
        publish_order
            .iter()
            .filter(|krate| {
                if *krate == verify_from {
                    verify = true;
                }
                verify
            })
            .collect::<Vec<_>>()
    });

    let mut processed_crates: HashSet<&String> = HashSet::new();
    for sel_crate in selected_crates {
        let span = span!(Level::INFO, "_", crate = sel_crate);
        let _enter = span.enter();

        if processed_crates.get(sel_crate).is_some() {
            info!("Crate was already processed",);
            continue;
        }

        info!("Processing crate");

        with_git_checkpoint(&opts.root, GitCheckpoint::Save, || -> anyhow::Result<()> {
            let details = crates
                .details
                .get(sel_crate)
                .with_context(|| format!("Crate not found: {sel_crate}"))?;
            for krate in &publish_order {
                if krate == sel_crate {
                    break;
                }
                let crate_details = crates
                    .details
                    .get(krate)
                    .with_context(|| format!("Crate details not found for crate: {krate}"))?;
                details.write_dependency_version(krate, &crate_details.version)?;
            }
            Ok(())
        })??;

        let crates_to_publish = crates.what_needs_publishing(sel_crate, &publish_order)?;

        if crates_to_publish.is_empty() {
            info!("Crate does not need to be published");
            continue;
        } else if crates_to_publish.len() == 1 {
            info!(
                "Crate {} will be taken into account for publishing",
                crates_to_publish[0]
            )
        } else {
            info!(
                "Crates will be taken into account in the following order for publishing {sel_crate}: {}",
                crates_to_publish
                    .iter()
                    .map(|krate| (*krate).into())
                    .collect::<Vec<String>>()
                    .join(", ")
            );
        }

        let (already_processed_crates, crates_to_publish): (Vec<&String>, Vec<&String>) =
            crates_to_publish
                .into_iter()
                .partition(|krate| processed_crates.get(*krate).is_some());

        if !already_processed_crates.is_empty() {
            info!(
                "The following crates have already been processed, so they'll be skipped: {}",
                already_processed_crates
                    .iter()
                    .map(|krate| (*krate).into())
                    .collect::<Vec<String>>()
                    .join(", ")
            );
        }

        for krate in crates_to_publish {
            let last_version = {
                let details = crates
                    .details
                    .get_mut(krate)
                    .with_context(|| format!("Crate not found: {krate}"))?;
                let prev_versions = external::crates_io::crate_versions(krate)?;
                if details.needs_publishing(&opts.root, &prev_versions)? {
                    with_git_checkpoint(&opts.root, GitCheckpoint::Save, || {
                        details.maybe_bump_version(
                            prev_versions
                                .into_iter()
                                .map(|prev_version| prev_version.version)
                                .collect(),
                        )
                    })??;
                    let last_version = details.version.clone();
                    crates.publish(
                        krate,
                        crates_to_verify.as_ref(),
                        opts.after_publish_delay.as_ref(),
                    )?;
                    last_version
                } else {
                    info!("Crate {krate} does not need to be published");
                    details.version.clone()
                }
            };

            with_git_checkpoint(&opts.root, GitCheckpoint::Save, || -> anyhow::Result<()> {
                for (_, details) in crates.details.iter() {
                    details.write_dependency_version(krate, &last_version)?;
                }
                Ok(())
            })??;

            processed_crates.insert(krate);
        }

        processed_crates.insert(sel_crate);
    }

    if opts.post_check {
        let mut cmd = std::process::Command::new("cargo");
        let mut cmd = cmd.current_dir(&opts.root).arg("update");
        for krate in &processed_crates {
            info!("Updating crate {krate}");
            cmd = cmd.arg("--quiet").arg("-p").arg(krate);
        }
        if !cmd.status()?.success() {
            anyhow::bail!("Command failed: {cmd:?}");
        };

        for krate in &processed_crates {
            let mut cmd = std::process::Command::new("cargo");
            info!("Checking crate {krate}");
            cmd.current_dir(&opts.root)
                .arg("check")
                .arg("--quiet")
                .arg("-p")
                .arg(krate);
            if !cmd.status()?.success() {
                anyhow::bail!("Command failed: {cmd:?}");
            };
        }
    }

    Ok(())
}
